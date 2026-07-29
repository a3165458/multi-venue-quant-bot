//! Regression tests for forced liquidation costs, equity/capital consistency,
//! and total return after final forced close.

use super::engine_test_support::{known_candles, HoldAfterOpen};
use super::*;
use std::sync::atomic::AtomicUsize;

#[tokio::test]
async fn forced_liquidation_applies_slippage_and_commission() {
    let data = known_candles();
    let initial = 10_000.0;
    let qty = 1.0;
    let mut engine = BacktestEngine::new(initial, data)
        .with_commission(0.001)
        .with_slippage(0.0005);

    let results = engine
        .run(Box::new(HoldAfterOpen {
            side: Side::Buy,
            qty,
            fired: AtomicUsize::new(0),
        }))
        .await
        .expect("run");

    // Open at first close with buy slip; force-close at last with sell slip + commission.
    let open_px = 100.0 * (1.0 + 0.0005);
    let close_px = 120.0 * (1.0 - 0.0005);
    let open_comm = open_px * 0.001 * qty;
    let close_comm = close_px * 0.001 * qty;
    let pnl = (close_px - open_px) * qty;
    let expected_final = initial - open_comm + pnl - close_comm;

    assert!(
        (results.final_capital - expected_final).abs() < 1e-9,
        "final_capital {} != expected {} (old behavior ignored force-close costs)",
        results.final_capital,
        expected_final
    );

    let forced = results.trades.last().expect("forced close trade");
    assert!(
        (forced.commission - close_comm).abs() < 1e-9,
        "forced close commission {} != {}",
        forced.commission,
        close_comm
    );
    assert!(
        (forced.price - close_px).abs() < 1e-9,
        "forced close price {} != slipped {}",
        forced.price,
        close_px
    );
}

#[tokio::test]
async fn final_equity_equals_final_capital_after_liquidation() {
    let data = known_candles();
    let mut engine = BacktestEngine::new(10_000.0, data)
        .with_commission(0.001)
        .with_slippage(0.0005);

    let results = engine
        .run(Box::new(HoldAfterOpen {
            side: Side::Buy,
            qty: 1.0,
            fired: AtomicUsize::new(0),
        }))
        .await
        .expect("run");

    let last_equity = results.equity_curve.last().expect("equity curve").1;
    assert!(
        (last_equity - results.final_capital).abs() < 1e-9,
        "last equity {} != final_capital {} (old behavior left unrealized equity)",
        last_equity,
        results.final_capital
    );
}

#[tokio::test]
async fn total_return_includes_final_forced_close() {
    let data = known_candles();
    let initial = 10_000.0;
    let qty = 1.0;
    let mut engine = BacktestEngine::new(initial, data)
        .with_commission(0.001)
        .with_slippage(0.0005);

    let results = engine
        .run(Box::new(HoldAfterOpen {
            side: Side::Buy,
            qty,
            fired: AtomicUsize::new(0),
        }))
        .await
        .expect("run");

    let expected_return = (results.final_capital - initial) / initial;
    assert!(
        (results.total_return - expected_return).abs() < 1e-12,
        "total_return {} != (final-initial)/initial {}",
        results.total_return,
        expected_return
    );
    // Must reflect positive move after costs (not pre-liquidation capital alone).
    assert!(results.total_return > 0.0);
}
