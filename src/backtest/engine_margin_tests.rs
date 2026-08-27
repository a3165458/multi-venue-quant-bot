//! 引擎级保证金/强平 fixture：验证峰值口径与强平后账目一致。

use super::engine_test_support::{candle, known_candles, signal_price, HoldAfterOpen};
use super::*;
use async_trait::async_trait;
use std::sync::atomic::AtomicUsize;

/// 每根K线都同向加仓——这是回测里唯一能把杠杆推过 1x 的路径：
/// 开仓/加仓的现金校验只比较**单笔**成本与现金余额，不看累计名义敞口。
struct AddEveryBar {
    qty: f64,
}

#[async_trait]
impl Strategy for AddEveryBar {
    fn name(&self) -> &str {
        "add_every_bar"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        let (symbol, price, ts) = signal_price(snapshot);
        Ok(Some(vec![TradeSignal {
            symbol,
            market_id: 0,
            side: Side::Buy,
            price,
            quantity: self.qty,
            order_type: OrderType::Market,
            reason: "add".into(),
            timestamp: ts,
            expected_edge_bps: None,
            risk_reducing: false,
            ..Default::default()
        }]))
    }

    fn reset(&mut self) {}
}

#[tokio::test]
async fn peaks_use_bar_close_notional_and_equity() {
    // 100 / 110 / 120 三根K线，零费零滑点：持仓 qty=1 全程不动。
    let mut engine = BacktestEngine::new(1000.0, known_candles())
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_grid_unit_notional(30.0)
        .with_soft_cap_grids(3.0);

    let results = engine
        .run(Box::new(HoldAfterOpen {
            side: Side::Buy,
            qty: 1.0,
            fired: AtomicUsize::new(0),
        }))
        .await
        .expect("run");

    // 峰值取收盘价毛名义（120），不是成本价（100）
    assert!(
        (results.peak_notional - 120.0).abs() < 1e-9,
        "peak_notional {} != 120",
        results.peak_notional
    );
    // 最后一根：notional 120 / equity (1000 + 20 浮盈) = 0.117647...
    let expected_lev = 120.0 / 1020.0;
    assert!(
        (results.peak_leverage - expected_lev).abs() < 1e-9,
        "peak_leverage {} != {}",
        results.peak_leverage,
        expected_lev
    );
    // 120 / 30 = 4 格；三根K线的格数 3.33 / 3.67 / 4.0 全部 >= soft_cap 3
    assert!((results.peak_position_grids - 4.0).abs() < 1e-9);
    assert_eq!(results.bars_over_soft_cap, 3);
    assert_eq!(results.liq_count, 0, "本例保证金充足，不应强平");
}

#[tokio::test]
async fn grid_metrics_stay_zero_without_explicit_unit() {
    let mut engine = BacktestEngine::new(1000.0, known_candles())
        .with_commission(0.0)
        .with_slippage(0.0);

    let results = engine
        .run(Box::new(HoldAfterOpen {
            side: Side::Buy,
            qty: 1.0,
            fired: AtomicUsize::new(0),
        }))
        .await
        .expect("run");

    // 不从成交反推单格名义：未显式设置就报 0，避免 soft 模式下被缩放的首笔信号污染
    assert_eq!(results.peak_position_grids, 0.0);
    assert_eq!(results.bars_over_soft_cap, 0);
    assert!(results.peak_notional > 0.0);
}

#[tokio::test]
async fn liquidation_fires_at_zero_free_margin_and_flattens() {
    // 价格恒定 100，资金 1000，每根K线加仓 3 张（名义 300）。
    // 第 k 根后 notional = 300(k+1)，equity 恒为 1000（无浮盈亏），
    // IM = notional/3 → 第 10 根 notional=3000, IM=1000, free_margin=0 → 强平。
    let data: Vec<Candlestick> = (0..10)
        .map(|i| candle(1_700_000_000 + i * 3600, 100.0))
        .collect();

    let mut engine = BacktestEngine::new(1000.0, data)
        .with_commission(0.0)
        .with_slippage(0.0);

    let results = engine
        .run(Box::new(AddEveryBar { qty: 3.0 }))
        .await
        .expect("run");

    assert_eq!(results.liq_count, 1, "free_margin<=0 必须触发一次强平");
    assert!(
        (results.peak_notional - 3000.0).abs() < 1e-9,
        "peak_notional {} != 3000",
        results.peak_notional
    );
    assert!(
        (results.peak_leverage - 3.0).abs() < 1e-9,
        "peak_leverage {} != 3.0",
        results.peak_leverage
    );
    // 强平在写权益曲线之前发生：最后一根的权益 = 平仓后的现金
    let last_equity = results.equity_curve.last().expect("equity").1;
    assert!(
        (last_equity - results.final_capital).abs() < 1e-9,
        "强平后权益 {} 与现金 {} 脱节",
        last_equity,
        results.final_capital
    );
    // 恒价 + 零成本 → 强平不产生盈亏
    assert!((results.final_capital - 1000.0).abs() < 1e-9);
    // 强平这笔计入 trades（与收盘强制平仓同一路径）
    let liq_trade = results.trades.last().expect("liq trade");
    assert_eq!(liq_trade.side, Side::Sell);
    assert!((liq_trade.quantity - 30.0).abs() < 1e-9);
}

#[tokio::test]
async fn higher_max_leverage_avoids_the_same_liquidation() {
    let data: Vec<Candlestick> = (0..10)
        .map(|i| candle(1_700_000_000 + i * 3600, 100.0))
        .collect();

    let mut engine = BacktestEngine::new(1000.0, data)
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_max_leverage(10.0);

    let results = engine
        .run(Box::new(AddEveryBar { qty: 3.0 }))
        .await
        .expect("run");

    assert_eq!(results.liq_count, 0, "10x 下 IM=300 远小于权益，不应强平");
    assert!((results.peak_notional - 3000.0).abs() < 1e-9);
}
