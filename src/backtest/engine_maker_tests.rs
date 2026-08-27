//! maker 填充模型测试：--maker 模式下 post_only 信号只在K线区间穿越报价价时成交，
//! 成交价 = 报价价（无滑点）；非 maker 模式保持 taker 行为（信号即成交 + 滑点）。

use super::engine_test_support::signal_price;
use super::*;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 每根K线都发一条 post_only 限价信号（固定报价价，忽略快照）。
struct PostOnlyQuoter {
    side: Side,
    price: f64,
    qty: f64,
    fired: AtomicUsize,
}

#[async_trait]
impl Strategy for PostOnlyQuoter {
    fn name(&self) -> &str {
        "post_only_quoter"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        self.fired.fetch_add(1, Ordering::SeqCst);
        let (symbol, _, ts) = signal_price(snapshot);
        Ok(Some(vec![TradeSignal {
            symbol,
            market_id: 0,
            side: self.side,
            price: self.price,
            quantity: self.qty,
            order_type: OrderType::Limit,
            reason: "maker quote".into(),
            timestamp: ts,
            expected_edge_bps: Some(3.0),
            risk_reducing: false,
            post_only: true,
            ..Default::default()
        }]))
    }

    fn reset(&mut self) {}
}

/// 指定区间（open/high/low/close）的单根K线。
fn range_candle(ts: i64, open: f64, high: f64, low: f64, close: f64) -> Candlestick {
    Candlestick {
        timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
        open,
        high,
        low,
        close,
        volume: 1.0,
        symbol: "BTC".to_string(),
    }
}

/// 买单报价 100，K线 low=101（未穿越）→ 不成交。
#[tokio::test]
async fn buy_quote_not_crossed_never_fills() {
    let data = vec![range_candle(1_700_000_000, 102.0, 103.0, 101.0, 102.0)];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_maker_fills(true);
    let strategy = Box::new(PostOnlyQuoter {
        side: Side::Buy,
        price: 100.0,
        qty: 1.0,
        fired: AtomicUsize::new(0),
    });
    let results = engine.run(strategy).await.expect("backtest runs");
    assert_eq!(results.trades.len(), 0, "未穿越的报价不得成交");
    assert_eq!(results.total_return, 0.0, "无成交则无盈亏");
}

/// 买单在首根 K 线收盘后生成，只能从下一根 K 线开始成交。
#[tokio::test]
async fn buy_quote_crossed_on_next_bar_fills_at_quote_price() {
    let data = vec![
        range_candle(1_700_000_000, 102.0, 103.0, 101.0, 102.0),
        range_candle(1_700_003_600, 102.0, 103.0, 99.0, 102.0),
    ];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_maker_fills(true);
    let strategy = Box::new(PostOnlyQuoter {
        side: Side::Buy,
        price: 100.0,
        qty: 1.0,
        fired: AtomicUsize::new(0),
    });
    let results = engine.run(strategy).await.expect("backtest runs");
    assert_eq!(
        results.trades.len(),
        1,
        "仅期末强制平仓一笔: {:#?}",
        results.trades
    );
    assert_eq!(results.trades[0].side, Side::Sell);
    assert!(
        (results.trades[0].pnl - 2.0).abs() < 1e-9,
        "入场应为报价价 100，pnl 应 2.0, got {}",
        results.trades[0].pnl
    );
}

#[tokio::test]
async fn current_bar_range_cannot_fill_a_quote_created_at_its_close() {
    let data = vec![
        range_candle(1_700_000_000, 102.0, 103.0, 99.0, 102.0),
        range_candle(1_700_003_600, 102.0, 103.0, 101.0, 102.0),
    ];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_maker_fills(true);
    let strategy = Box::new(PostOnlyQuoter {
        side: Side::Buy,
        price: 100.0,
        qty: 1.0,
        fired: AtomicUsize::new(0),
    });

    let results = engine.run(strategy).await.expect("backtest runs");

    assert_eq!(
        results.trades.len(),
        0,
        "首根 K 线的 low 不得回填收盘后订单"
    );
}

/// 卖单在首根 K 线收盘后生成，下一根 high 穿越后以报价成交。
#[tokio::test]
async fn sell_quote_crossed_on_next_bar_fills_at_quote_price() {
    let data = vec![
        range_candle(1_700_000_000, 98.0, 99.0, 97.0, 98.0),
        range_candle(1_700_003_600, 98.0, 101.0, 97.0, 98.0),
    ];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_maker_fills(true);
    let strategy = Box::new(PostOnlyQuoter {
        side: Side::Sell,
        price: 100.0,
        qty: 1.0,
        fired: AtomicUsize::new(0),
    });
    let results = engine.run(strategy).await.expect("backtest runs");
    assert_eq!(
        results.trades.len(),
        1,
        "仅期末强制平仓一笔: {:#?}",
        results.trades
    );
    assert_eq!(results.trades[0].side, Side::Buy);
    assert!(
        (results.trades[0].pnl - 2.0).abs() < 1e-9,
        "入场应为报价价 100，pnl 应 2.0, got {}",
        results.trades[0].pnl
    );
}

/// 同一信号，不开 maker 模式 → 走 taker 路径：信号即成交 + 滑点（买滑点为正）。
#[tokio::test]
async fn same_signal_without_maker_mode_is_taker_with_slippage() {
    let data = vec![range_candle(1_700_000_000, 102.0, 103.0, 101.0, 102.0)];
    // 注意：maker 模式下未穿越不成交；taker 模式直接成交——同一根K线做对照
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_commission(0.0)
        .with_slippage(0.0005);
    let strategy = Box::new(PostOnlyQuoter {
        side: Side::Buy,
        price: 100.0,
        qty: 1.0,
        fired: AtomicUsize::new(0),
    });
    let results = engine.run(strategy).await.expect("backtest runs");
    assert_eq!(
        results.trades.len(),
        1,
        "taker 模式信号即成交: {:#?}",
        results.trades
    );
    // taker: 入场 = 100 * 1.0005 = 100.05，收盘 102 * 0.9995 = 101.949 → pnl = 1.899
    assert!(
        (results.trades[0].pnl - 1.899).abs() < 1e-9,
        "taker 应含滑点，pnl 应 1.899, got {}",
        results.trades[0].pnl
    );
}

/// 多根K线：订单只从生成后的下一根K线开始检查，成交一次后不会重复填充。
#[tokio::test]
async fn quote_fills_once_then_rests() {
    let data = vec![
        range_candle(1_700_000_000, 102.0, 103.0, 101.0, 102.0), // 生成报价
        range_candle(1_700_003_600, 102.0, 103.0, 99.0, 102.0),  // 下一根穿越
        range_candle(1_700_007_200, 102.0, 103.0, 101.0, 102.0), // 未穿越
    ];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_maker_fills(true);
    let strategy = Box::new(PostOnlyQuoter {
        side: Side::Buy,
        price: 100.0,
        qty: 1.0,
        fired: AtomicUsize::new(0),
    });
    let results = engine.run(strategy).await.expect("backtest runs");
    assert_eq!(
        results.trades.len(),
        1,
        "仅期末强制平仓一笔: {:#?}",
        results.trades
    );
    assert!(
        (results.trades[0].pnl - 2.0).abs() < 1e-9,
        "仅第二根K线应成交一次，pnl 应 2.0, got {}",
        results.trades[0].pnl
    );
}
