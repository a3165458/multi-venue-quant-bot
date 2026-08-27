//! Live-parity regressions for maker execution costs and risk exits.

use super::engine_test_support::{candle, HoldAfterOpen};
use super::*;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use std::sync::atomic::{AtomicUsize, Ordering};

struct OneMakerBuy {
    fired: AtomicUsize,
}

#[async_trait]
impl Strategy for OneMakerBuy {
    fn name(&self) -> &str {
        "one_maker_buy"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        if self.fired.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(None);
        }
        let timestamp = snapshot
            .order_books
            .values()
            .next()
            .expect("book")
            .timestamp;
        Ok(Some(vec![TradeSignal {
            symbol: "BTC".into(),
            market_id: 0,
            side: Side::Buy,
            price: 100.0,
            quantity: 1.0,
            order_type: OrderType::Limit,
            reason: "maker entry".into(),
            timestamp,
            expected_edge_bps: Some(15.0),
            risk_reducing: false,
            post_only: true,
            client_id: Some("mq_BTC_buy".into()),
            ..Default::default()
        }]))
    }

    fn reset(&mut self) {}
}

fn bar(ts: i64, open: f64, high: f64, low: f64, close: f64) -> Candlestick {
    Candlestick {
        timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
        open,
        high,
        low,
        close,
        volume: 10.0,
        symbol: "BTC".into(),
    }
}

fn maker_strategy() -> Box<dyn Strategy> {
    Box::new(OneMakerBuy {
        fired: AtomicUsize::new(0),
    })
}

struct SequentialMarketBuys {
    fired: AtomicUsize,
}

#[async_trait]
impl Strategy for SequentialMarketBuys {
    fn name(&self) -> &str {
        "sequential_market_buys"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        let index = self.fired.fetch_add(1, Ordering::SeqCst);
        let symbol = match index {
            0 => "BTC",
            1 => "ETH",
            _ => return Ok(None),
        };
        let timestamp = snapshot
            .order_books
            .get(symbol)
            .expect("current market book")
            .timestamp;
        Ok(Some(vec![TradeSignal {
            symbol: symbol.into(),
            market_id: index as u32,
            side: Side::Buy,
            price: 100.0,
            quantity: 2.0,
            order_type: OrderType::Limit,
            reason: "aggregate exposure test".into(),
            timestamp,
            expected_edge_bps: Some(15.0),
            risk_reducing: false,
            ..Default::default()
        }]))
    }

    fn reset(&mut self) {}
}

fn market_bar(symbol: &str, ts: i64) -> Candlestick {
    let mut candle = bar(ts, 100.0, 101.0, 99.0, 100.0);
    candle.symbol = symbol.into();
    candle
}

#[tokio::test]
async fn stop_loss_uses_taker_fee_and_gap_price() {
    let data = vec![
        bar(1_700_000_000, 100.0, 101.0, 100.0, 100.0),
        bar(1_700_000_060, 100.0, 101.0, 99.0, 100.0),
        bar(1_700_000_120, 96.0, 96.0, 95.0, 95.0),
    ];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_maker_fills(true)
        .with_execution_costs(0.0, 2.25 / 10_000.0, 0.0)
        .expect("valid costs")
        .with_position_risk(0.03, 0.05)
        .expect("valid risk");

    let results = engine.run(maker_strategy()).await.expect("run");

    assert_eq!(results.stop_loss_exits, 1);
    assert_eq!(results.take_profit_exits, 0);
    assert_eq!(results.trades.len(), 1);
    assert!((results.trades[0].price - 96.0).abs() < 1e-12);
    assert!((results.trades[0].pnl + 4.0).abs() < 1e-12);
    assert!((results.total_commission - 96.0 * 2.25 / 10_000.0).abs() < 1e-12);
}

#[tokio::test]
async fn take_profit_uses_gap_price_and_is_counted() {
    let data = vec![
        bar(1_700_000_000, 100.0, 101.0, 100.0, 100.0),
        bar(1_700_000_060, 100.0, 101.0, 99.0, 100.0),
        bar(1_700_000_120, 106.0, 107.0, 105.0, 106.0),
    ];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_maker_fills(true)
        .with_execution_costs(0.0, 0.0, 0.0)
        .expect("valid costs")
        .with_position_risk(0.03, 0.05)
        .expect("valid risk");

    let results = engine.run(maker_strategy()).await.expect("run");

    assert_eq!(results.stop_loss_exits, 0);
    assert_eq!(results.take_profit_exits, 1);
    assert_eq!(results.trades.len(), 1);
    assert!((results.trades[0].price - 106.0).abs() < 1e-12);
    assert!((results.trades[0].pnl - 6.0).abs() < 1e-12);
}

#[tokio::test]
async fn projected_symbol_exposure_cannot_cross_live_cap() {
    let mut engine = BacktestEngine::new(10_000.0, vec![candle(1_700_000_000, 100.0)])
        .with_execution_costs(0.0, 0.0, 0.0)
        .expect("valid costs")
        .with_max_position_notional(100.0)
        .expect("valid cap");
    let results = engine
        .run(Box::new(HoldAfterOpen {
            side: Side::Buy,
            qty: 2.0,
            fired: AtomicUsize::new(0),
        }))
        .await
        .expect("run");

    assert_eq!(results.blocked_by_position_limit, 1);
    assert!(results.trades.is_empty());
    assert_eq!(results.final_capital, results.initial_capital);
}

#[tokio::test]
async fn projected_account_exposure_cannot_cross_policy_cap() {
    let data = vec![
        market_bar("BTC", 1_700_000_000),
        market_bar("ETH", 1_700_000_060),
    ];
    let mut engine = BacktestEngine::new(1_000.0, data)
        .with_execution_costs(0.0, 0.0, 0.0)
        .expect("valid costs")
        .with_max_total_notional_pct(0.25)
        .expect("valid total cap");
    let results = engine
        .run(Box::new(SequentialMarketBuys {
            fired: AtomicUsize::new(0),
        }))
        .await
        .expect("run");

    assert_eq!(results.blocked_by_total_position_limit, 1);
    assert_eq!(results.trades.len(), 1, "only BTC should open and close");
    assert!(results.peak_notional <= 200.0 + f64::EPSILON);
}

#[tokio::test]
async fn conservative_maker_model_requires_penetration_and_partially_fills() {
    let data = vec![
        bar(1_700_000_000, 100.0, 101.0, 100.0, 100.0),
        bar(1_700_000_060, 100.0, 101.0, 99.99, 100.0),
        bar(1_700_000_120, 100.0, 101.0, 99.0, 100.0),
        bar(1_700_000_180, 100.0, 101.0, 99.0, 100.0),
    ];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_maker_fills(true)
        .with_execution_costs(0.0, 0.0, 0.0)
        .expect("valid costs")
        .with_conservative_maker_model(0.5, 2.0, 0.0)
        .expect("valid maker model");

    let results = engine.run(maker_strategy()).await.expect("run");

    assert_eq!(
        results.trades.len(),
        1,
        "only the terminal close is recorded"
    );
    assert!((results.trades[0].quantity - 0.75).abs() < 1e-12);
}

#[tokio::test]
async fn maker_adverse_selection_penalty_is_separate_from_commission() {
    let data = vec![
        bar(1_700_000_000, 100.0, 101.0, 100.0, 100.0),
        bar(1_700_000_060, 100.0, 101.0, 99.0, 100.0),
    ];
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_maker_fills(true)
        .with_execution_costs(0.0, 0.0, 0.0)
        .expect("valid costs")
        .with_conservative_maker_model(1.0, 0.0, 1.0)
        .expect("valid maker model");

    let results = engine.run(maker_strategy()).await.expect("run");

    assert_eq!(results.total_commission, 0.0);
    assert!((results.total_adverse_selection - 0.01).abs() < 1e-12);
    assert!((results.final_capital - 9_999.99).abs() < 1e-9);
}
