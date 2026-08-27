//! 多市场引擎测试：不同 symbol 的持仓独立跟踪（开/加/平互不干扰），
//! 权益按各 symbol 最近收盘价聚合，强平为账户级（全部持仓一起平）。

use super::*;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn candle(symbol: &str, ts: i64, close: f64) -> Candlestick {
    Candlestick {
        timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
        open: close,
        high: close,
        low: close,
        close,
        volume: 1.0,
        symbol: symbol.to_string(),
    }
}

/// 第一步在 BTC-USD 开多，第二步在 ETH-USD 开空，之后安静。
struct TwoMarketOpener {
    step: AtomicUsize,
}

#[async_trait]
impl Strategy for TwoMarketOpener {
    fn name(&self) -> &str {
        "two_market_opener"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let price_of = |sym: &str| -> f64 {
            snapshot
                .candles
                .get(sym)
                .and_then(|c| c.last())
                .map(|c| c.close)
                .unwrap_or(100.0)
        };
        let mk = |symbol: String, side: Side, qty: f64| TradeSignal {
            market_id: 0,
            side,
            price: price_of(&symbol),
            quantity: qty,
            order_type: OrderType::Market,
            reason: "multi-market opener".into(),
            timestamp: ts,
            expected_edge_bps: None,
            risk_reducing: false,
            symbol,
            ..Default::default()
        };
        match step {
            0 => Ok(Some(vec![mk("BTC-USD".to_string(), Side::Buy, 1.0)])),
            1 => Ok(Some(vec![mk("ETH-USD".to_string(), Side::Sell, 2.0)])),
            _ => Ok(None),
        }
    }

    fn reset(&mut self) {}
}

#[tokio::test]
async fn holds_positions_in_two_markets_independently() {
    let data = vec![
        candle("BTC-USD", 1_700_000_000, 100.0),
        candle("ETH-USD", 1_700_000_000, 2_000.0),
        candle("BTC-USD", 1_700_000_100, 110.0),
        candle("ETH-USD", 1_700_000_100, 1_900.0),
    ];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_commission(0.0)
        .with_slippage(0.0);
    let results = engine
        .run(Box::new(TwoMarketOpener {
            step: AtomicUsize::new(0),
        }))
        .await
        .expect("run");

    // 开仓不记 trades；两笔强制平仓各记一笔 → 共 2 笔
    assert_eq!(results.trades.len(), 2, "强制平仓2笔: {:?}", results.trades);
    // BTC 多 1 @ 100 → 平于 110：+10；ETH 空 2 @ 2000 → 平于 1900：+200 → 合计 210
    assert!(
        (results.final_capital - 10_210.0).abs() < 1.0,
        "权益应为 10210，实测 {}",
        results.final_capital
    );
}

/// 权益曲线聚合：BTC 上涨而 ETH 下跌，多空相互抵消的部分不重复计入。
#[tokio::test]
async fn equity_aggregates_unrealized_across_markets() {
    let data = vec![
        candle("BTC-USD", 1_700_000_000, 100.0),
        candle("ETH-USD", 1_700_000_000, 2_000.0),
        candle("BTC-USD", 1_700_000_100, 110.0),   // BTC +10%
        candle("ETH-USD", 1_700_000_100, 1_900.0), // ETH -5%
    ];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_commission(0.0)
        .with_slippage(0.0);
    let results = engine
        .run(Box::new(TwoMarketOpener {
            step: AtomicUsize::new(0),
        }))
        .await
        .expect("run");
    // 最后一根K线（ETH 1900）后：BTC 多 1 浮盈 10，ETH 空 2 浮盈 200 → 权益 10210
    let last = results.equity_curve.last().unwrap();
    assert!(
        (last.1 - 10_210.0).abs() < 1.0,
        "聚合权益应为 10210，实测 {:.2}",
        last.1
    );
}

struct MarketCountObserver {
    max_seen: Arc<AtomicUsize>,
}

#[async_trait]
impl Strategy for MarketCountObserver {
    fn name(&self) -> &str {
        "market_count_observer"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        self.max_seen
            .fetch_max(snapshot.order_books.len(), Ordering::SeqCst);
        Ok(None)
    }

    fn reset(&mut self) {}
}

#[tokio::test]
async fn snapshot_retains_latest_book_for_every_seen_market() {
    let data = vec![
        candle("BTC-USD", 1_700_000_000, 100.0),
        candle("ETH-USD", 1_700_000_000, 2_000.0),
        candle("BTC-USD", 1_700_000_100, 110.0),
        candle("ETH-USD", 1_700_000_100, 1_900.0),
    ];
    let counter = Arc::new(AtomicUsize::new(0));
    let observer = Box::new(MarketCountObserver {
        max_seen: counter.clone(),
    });
    let mut engine = BacktestEngine::new(10_000.0, data);

    let _ = engine.run(observer).await.expect("run");

    assert_eq!(counter.load(Ordering::SeqCst), 2);
}
