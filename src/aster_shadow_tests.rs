use super::aster_shadow::{ShadowConfig, ShadowMakerMonitor};
use crate::lighter::types::{OrderType, Side, SignalAction, TradeSignal};
use chrono::{TimeZone, Utc};

fn signal(
    action: SignalAction,
    side: Side,
    price: f64,
    quantity: f64,
    client_id: &str,
) -> TradeSignal {
    TradeSignal {
        action,
        symbol: "BTCUSDT".into(),
        market_id: 0,
        side,
        price,
        quantity,
        order_type: OrderType::Limit,
        reason: "shadow test".into(),
        timestamp: Utc.timestamp_millis_opt(1_000).single().unwrap(),
        expected_edge_bps: Some(10.0),
        risk_reducing: false,
        post_only: true,
        client_id: Some(client_id.into()),
    }
}

fn monitor() -> ShadowMakerMonitor {
    ShadowMakerMonitor::new(ShadowConfig {
        penetration_bps: 2.0,
        fill_ratio: 0.5,
        markout_horizons_ms: vec![1_000, 5_000],
        max_recent_fills: 10,
    })
    .unwrap()
}

#[test]
fn shadow_config_rejects_zero_fill_ratio() {
    assert!(ShadowMakerMonitor::new(ShadowConfig {
        penetration_bps: 2.0,
        fill_ratio: 0.0,
        markout_horizons_ms: vec![1_000],
        max_recent_fills: 10,
    })
    .is_err());
}

#[test]
fn quote_lifecycle_counts_places_requotes_and_cancels() {
    let mut shadow = monitor();
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_000,
    );
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 99.9, 2.0, "buy"),
        2_000,
    );
    let amend_pending = shadow.snapshot(2_000);
    assert_eq!(amend_pending.active_quotes, 1);
    assert_eq!(amend_pending.quote_requotes, 1);
    assert_eq!(amend_pending.estimated_order_requests, 3);
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 99.9, 2.0, "buy"),
        2_500,
    );
    shadow.apply_signal(
        &signal(SignalAction::Cancel, Side::Buy, 99.9, 2.0, "buy"),
        3_000,
    );

    let snapshot = shadow.snapshot(4_000);
    assert_eq!(snapshot.quote_places, 1);
    assert_eq!(snapshot.quote_requotes, 1);
    assert_eq!(snapshot.quote_cancels, 1);
    assert_eq!(snapshot.active_quotes, 0);
    assert_eq!(snapshot.estimated_order_requests, 4);
    assert_eq!(snapshot.estimated_modify_requests, 3);
    assert_eq!(snapshot.modify_request_savings, 1);
}

#[test]
fn same_price_signal_is_a_noop_like_real_execution() {
    let mut shadow = monitor();
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_000,
    );
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 3.0, "buy"),
        2_000,
    );
    let snapshot = shadow.snapshot(2_000);
    assert_eq!(snapshot.active_quotes, 1);
    assert_eq!(snapshot.quote_requotes, 0);
    assert_eq!(snapshot.estimated_order_requests, 1);
}

#[test]
fn virtual_fill_requires_penetration_and_applies_fill_ratio() {
    let mut shadow = monitor();
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_000,
    );

    shadow.observe_bbo("BTCUSDT", 99.98, 99.99, 1_050, 1_060);
    assert_eq!(shadow.snapshot(1_060).virtual_fills, 0);

    shadow.observe_bbo("BTCUSDT", 99.96, 99.97, 1_100, 1_110);
    let snapshot = shadow.snapshot(1_110);
    assert_eq!(snapshot.virtual_fills, 1);
    assert_eq!(snapshot.active_quotes, 1);
    assert!((snapshot.virtual_quantity - 1.0).abs() < 1e-12);
    assert!((snapshot.virtual_volume - 100.0).abs() < 1e-12);

    shadow.observe_bbo("BTCUSDT", 99.95, 99.96, 1_200, 1_210);
    assert_eq!(shadow.snapshot(1_210).virtual_fills, 1);
    shadow.observe_bbo("BTCUSDT", 100.0, 100.1, 1_300, 1_310);
    shadow.observe_bbo("BTCUSDT", 99.95, 99.96, 1_400, 1_410);
    let second_cross = shadow.snapshot(1_410);
    assert_eq!(second_cross.virtual_fills, 2);
    assert!((second_cross.virtual_quantity - 1.5).abs() < 1e-12);
}

#[test]
fn markout_is_side_adjusted_at_each_horizon() {
    let mut shadow = monitor();
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_000,
    );
    shadow.observe_bbo("BTCUSDT", 99.96, 99.97, 1_100, 1_100);

    shadow.observe_bbo("BTCUSDT", 100.99, 101.01, 2_100, 2_100);
    let one_second = shadow.snapshot(2_100);
    assert_eq!(one_second.markouts[0].samples, 1);
    assert!((one_second.markouts[0].mean_bps - 100.0).abs() < 1e-9);
    assert_eq!(one_second.markouts[1].samples, 0);

    shadow.observe_bbo("BTCUSDT", 98.99, 99.01, 6_100, 6_100);
    let five_seconds = shadow.snapshot(6_100);
    assert_eq!(five_seconds.markouts[1].samples, 1);
    assert!((five_seconds.markouts[1].mean_bps + 100.0).abs() < 1e-9);
}

#[test]
fn bbo_and_strategy_latency_metrics_are_bounded_and_aggregated() {
    let mut shadow = monitor();
    shadow.observe_bbo("BTCUSDT", 99.9, 100.1, 900, 1_000);
    shadow.observe_bbo("BTCUSDT", 100.0, 100.2, 1_050, 1_200);
    shadow.record_strategy_eval(40);
    shadow.record_strategy_eval(60);

    let snapshot = shadow.snapshot(2_000);
    assert_eq!(snapshot.bbo_events, 2);
    assert_eq!(snapshot.bbo_changes, 1);
    assert!((snapshot.mean_event_lag_ms - 125.0).abs() < 1e-9);
    assert_eq!(snapshot.max_event_lag_ms, 150);
    assert_eq!(snapshot.strategy_evaluations, 2);
    assert!((snapshot.mean_strategy_eval_micros - 50.0).abs() < 1e-9);
    assert_eq!(snapshot.max_strategy_eval_micros, 60);
}

#[test]
fn virtual_fills_update_signed_inventory_for_strategy_feedback() {
    let mut shadow = monitor();
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_000,
    );
    shadow.observe_bbo("BTCUSDT", 99.96, 99.97, 1_100, 1_100);
    assert!((shadow.virtual_positions()["BTCUSDT"] - 1.0).abs() < 1e-12);

    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Sell, 101.0, 2.0, "sell"),
        1_200,
    );
    shadow.observe_bbo("BTCUSDT", 101.03, 101.04, 1_300, 1_300);
    assert!(shadow.virtual_positions()["BTCUSDT"].abs() < 1e-12);
}

#[test]
fn leaving_pause_clears_shadow_quotes_inventory_and_blocks_new_collection() {
    let mut shadow = monitor();
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_000,
    );
    shadow.observe_bbo("BTCUSDT", 99.96, 99.97, 1_100, 1_100);
    shadow.set_collecting(false, 1_200);

    let stopped = shadow.snapshot(1_200);
    assert!(!stopped.collecting);
    assert_eq!(stopped.active_quotes, 0);
    assert!(shadow.virtual_positions().is_empty());
    assert_eq!(
        shadow.snapshot(10_000).runtime_seconds,
        stopped.runtime_seconds
    );

    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_300,
    );
    shadow.observe_bbo("BTCUSDT", 99.96, 99.97, 1_400, 1_400);
    assert_eq!(shadow.snapshot(1_400).virtual_fills, 1);
}

#[test]
fn suspending_one_market_removes_its_fillable_shadow_state() {
    let mut shadow = monitor();
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_000,
    );
    shadow.clear_symbol("BTCUSDT", 1_050);
    shadow.observe_bbo("BTCUSDT", 99.96, 99.97, 1_100, 1_100);
    assert_eq!(shadow.snapshot(1_100).virtual_fills, 0);
    assert!(shadow.virtual_positions().is_empty());
}

#[test]
fn pull_quotes_cancels_resting_quotes_without_wiping_inventory() {
    let mut shadow = monitor();
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_000,
    );
    shadow.observe_bbo("BTCUSDT", 99.96, 99.97, 1_100, 1_100);
    shadow.pull_quotes("BTCUSDT", 1_150);
    let snapshot = shadow.snapshot(1_150);
    assert_eq!(snapshot.active_quotes, 0);
    assert_eq!(snapshot.quote_cancels, 1);
    assert_eq!(snapshot.virtual_fills, 1);
    assert!((shadow.virtual_positions()["BTCUSDT"] - 1.0).abs() < 1e-12);
}

#[test]
fn requote_amend_stays_on_the_book_and_can_fill_without_a_gap() {
    let mut shadow = monitor();
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_000,
    );
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 99.9, 2.0, "buy"),
        1_050,
    );
    shadow.observe_bbo("BTCUSDT", 99.86, 99.87, 1_100, 1_100);
    let snapshot = shadow.snapshot(1_100);
    assert_eq!(snapshot.active_quotes, 1);
    assert_eq!(snapshot.quote_requotes, 1);
    assert_eq!(snapshot.virtual_fills, 1);
    assert!((snapshot.virtual_quantity - 1.0).abs() < 1e-12);
}

#[test]
fn depth_updates_measure_visible_queue_ahead_notional() {
    let mut shadow = monitor();
    shadow.apply_signal(
        &signal(SignalAction::Place, Side::Buy, 100.0, 2.0, "buy"),
        1_000,
    );
    shadow.observe_depth(
        "BTCUSDT",
        &[(101.0, 1.0), (100.0, 2.0), (99.9, 3.0)],
        &[(101.1, 1.0), (101.2, 2.0)],
        1_050,
        1_100,
    );
    let snapshot = shadow.snapshot(1_100);
    assert_eq!(snapshot.depth_events, 1);
    assert_eq!(snapshot.visible_queue_samples, 1);
    assert!((snapshot.mean_queue_ahead_notional - 301.0).abs() < 1e-9);
    assert_eq!(snapshot.depth_visibility_misses, 0);
}
