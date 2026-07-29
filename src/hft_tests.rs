use std::time::Duration;

use crate::hft::{
    parse_bbo_update, plan_subscription_shards, BookContinuity, BookHealth, StandardRateBudget,
};

#[test]
fn subscription_plan_respects_per_connection_limit() {
    let market_ids = (0..205).collect::<Vec<_>>();

    let shards = plan_subscription_shards(&market_ids, 100).expect("valid shard plan");

    assert_eq!(shards.len(), 3);
    assert_eq!(shards[0].len(), 100);
    assert_eq!(shards[1].len(), 100);
    assert_eq!(shards[2].len(), 5);
    assert_eq!(
        shards.into_iter().flatten().collect::<Vec<_>>(),
        market_ids
    );
}

#[test]
fn subscription_plan_rejects_zero_capacity() {
    let error = plan_subscription_shards(&[1, 2], 0).expect_err("zero capacity must fail");

    assert!(error.to_string().contains("greater than zero"));
}

#[test]
fn standard_rate_budget_allows_at_most_one_action_per_second() {
    let mut budget = StandardRateBudget::new();

    assert!(budget.try_acquire_at(Duration::ZERO));
    assert!(!budget.try_acquire_at(Duration::from_millis(999)));
    assert!(budget.try_acquire_at(Duration::from_secs(1)));
    assert!(!budget.try_acquire_at(Duration::from_millis(1_999)));
    assert!(budget.try_acquire_at(Duration::from_secs(2)));
}

#[test]
fn standard_rate_budget_does_not_burst_after_idle_time() {
    let mut budget = StandardRateBudget::new();

    assert!(budget.try_acquire_at(Duration::from_secs(120)));
    assert!(!budget.try_acquire_at(Duration::from_secs(120)));
}

#[test]
fn book_continuity_accepts_snapshot_and_contiguous_delta() {
    let mut continuity = BookContinuity::new();

    assert_eq!(continuity.apply_snapshot(10), BookHealth::Live);
    assert_eq!(continuity.apply_delta(10, 14), BookHealth::Live);
    assert_eq!(continuity.last_nonce(), Some(14));
}

#[test]
fn book_continuity_halts_on_nonce_gap_until_new_snapshot() {
    let mut continuity = BookContinuity::new();
    continuity.apply_snapshot(10);

    assert_eq!(continuity.apply_delta(11, 15), BookHealth::Halted);
    assert_eq!(continuity.apply_delta(15, 16), BookHealth::Halted);
    assert_eq!(continuity.apply_snapshot(20), BookHealth::Live);
    assert_eq!(continuity.last_nonce(), Some(20));
}

#[test]
fn parses_official_ticker_bbo_shape() {
    let message = serde_json::json!({
        "channel": "ticker:17",
        "nonce": 6442420597_u64,
        "ticker": {
            "s": "SOL",
            "a": {"price": "215.10", "size": "4.65"},
            "b": {"price": "214.99", "size": "17.45"}
        },
        "timestamp": 1773158679717_u64,
        "type": "update/ticker"
    });

    let bbo = parse_bbo_update(&message).expect("valid ticker");

    assert_eq!(bbo.market_id, 17);
    assert_eq!(bbo.symbol, "SOL");
    assert_eq!(bbo.nonce, 6_442_420_597);
    assert_eq!(bbo.exchange_timestamp_ms, 1_773_158_679_717);
    assert_eq!(bbo.bid_price, 214.99);
    assert_eq!(bbo.bid_size, 17.45);
    assert_eq!(bbo.ask_price, 215.10);
    assert_eq!(bbo.ask_size, 4.65);
}

#[test]
fn rejects_ticker_without_two_sided_positive_prices() {
    let missing_ask = serde_json::json!({
        "channel": "ticker:1",
        "nonce": 2,
        "ticker": {
            "s": "BTC",
            "a": {"price": "0", "size": "1"},
            "b": {"price": "100", "size": "1"}
        },
        "timestamp": 3,
        "type": "update/ticker"
    });

    assert!(parse_bbo_update(&missing_ask).is_err());
}
