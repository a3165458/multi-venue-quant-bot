//! Commission, benchmark, and excess-return accounting regressions.

use super::engine_test_support::{
    expected_bh_return, known_candles, HoldAfterOpen, OpenThenReverse,
};
use super::*;
use std::sync::atomic::AtomicUsize;

#[tokio::test]
async fn reports_benchmark_excess_and_total_commission() {
    let data = known_candles();
    let initial = 10_000.0;
    let slip = 0.0005;
    let comm = 0.001;
    let mut engine = BacktestEngine::new(initial, data)
        .with_commission(comm)
        .with_slippage(slip);

    let results = engine
        .run(Box::new(HoldAfterOpen {
            side: Side::Buy,
            qty: 1.0,
            fired: AtomicUsize::new(0),
        }))
        .await
        .expect("run");

    let bh = expected_bh_return(100.0, 120.0, slip, comm, initial);
    assert!(
        (results.benchmark_return - bh).abs() < 1e-9,
        "benchmark_return {} != {}",
        results.benchmark_return,
        bh
    );
    assert!(
        (results.excess_return - (results.total_return - results.benchmark_return)).abs() < 1e-12
    );

    let open_px = 100.0 * (1.0 + slip);
    let close_px = 120.0 * (1.0 - slip);
    let expected_total_comm = open_px * comm * 1.0 + close_px * comm * 1.0;
    assert!(
        (results.total_commission - expected_total_comm).abs() < 1e-9,
        "total_commission {} != {}",
        results.total_commission,
        expected_total_comm
    );
}

#[tokio::test]
async fn total_commission_includes_open_add_close_reversal_and_forced() {
    // Path: open long, reverse to short (close + open), hold short → forced close.
    let data = known_candles();
    let initial = 50_000.0;
    let qty = 1.0;
    let slip = 0.0005;
    let comm = 0.001;
    let mut engine = BacktestEngine::new(initial, data)
        .with_commission(comm)
        .with_slippage(slip);

    let results = engine
        .run(Box::new(OpenThenReverse {
            qty,
            step: AtomicUsize::new(0),
        }))
        .await
        .expect("run");

    let sum_trade_comm: f64 = results.trades.iter().map(|t| t.commission).sum();
    // Open commissions (initial + reverse open) are charged but may not appear as trade rows.
    // total_commission must include every charged commission.
    assert!(
        results.total_commission + 1e-9 >= sum_trade_comm,
        "total_commission {} < sum of trade commissions {}",
        results.total_commission,
        sum_trade_comm
    );
    assert!(
        results.total_commission > sum_trade_comm + 1e-12,
        "total_commission should include open commissions not only close trade rows; got {} vs trade sum {}",
        results.total_commission,
        sum_trade_comm
    );

    // Explicit expected: open buy, reverse close sell + open sell, forced buy to flat.
    let px0 = 100.0 * (1.0 + slip); // buy open
    let px1_sell = 110.0 * (1.0 - slip); // close long + open short
    let px2_buy = 120.0 * (1.0 + slip); // force close short
    let open_comm = px0 * comm * qty;
    let rev_close_comm = px1_sell * comm * qty;
    let rev_open_comm = px1_sell * comm * qty;
    let forced_comm = px2_buy * comm * qty;
    let expected = open_comm + rev_close_comm + rev_open_comm + forced_comm;
    assert!(
        (results.total_commission - expected).abs() < 1e-9,
        "total_commission {} != expected {}",
        results.total_commission,
        expected
    );
}
