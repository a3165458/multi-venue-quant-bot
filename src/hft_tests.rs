use std::time::Duration;

use clap::Parser;

use crate::hft::{
    parse_bbo_update, plan_subscription_shards, BboUpdate, BookContinuity, BookHealth, ScanStats,
    StandardRateBudget,
};
use crate::lighter::{types::WsMessage, websocket::LighterWebSocket};
use crate::{Cli, Commands};

#[test]
fn subscription_plan_respects_per_connection_limit() {
    let market_ids = (0..205).collect::<Vec<_>>();

    let shards = plan_subscription_shards(&market_ids, 100).expect("valid shard plan");

    assert_eq!(shards.len(), 3);
    assert_eq!(shards[0].len(), 100);
    assert_eq!(shards[1].len(), 100);
    assert_eq!(shards[2].len(), 5);
    assert_eq!(shards.into_iter().flatten().collect::<Vec<_>>(), market_ids);
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

#[test]
fn websocket_emits_typed_bbo_updates() {
    let raw = serde_json::json!({
        "channel": "ticker:1",
        "nonce": 42,
        "ticker": {
            "s": "BTC",
            "a": {"price": "100.1", "size": "2"},
            "b": {"price": "100.0", "size": "3"}
        },
        "timestamp": 1773158679717_u64,
        "type": "update/ticker"
    })
    .to_string();

    let message = LighterWebSocket::parse_message(&raw)
        .expect("valid websocket envelope")
        .expect("data message");

    match message {
        WsMessage::BboUpdate(bbo) => {
            assert_eq!(bbo.market_id, 1);
            assert_eq!(bbo.nonce, 42);
        }
        other => panic!("expected BBO update, received {other:?}"),
    }
}

#[test]
fn ticker_subscription_uses_one_channel_per_market() {
    let message = LighterWebSocket::ticker_subscription_message(7);

    assert_eq!(
        message,
        serde_json::json!({
            "type": "subscribe",
            "channel": "ticker/7"
        })
    );
}

#[test]
fn scan_stats_report_event_rate_and_rank_current_spreads() {
    let mut stats = ScanStats::new();
    stats.record(BboUpdate {
        market_id: 1,
        symbol: "BTC".to_string(),
        nonce: 1,
        exchange_timestamp_ms: 1,
        bid_price: 100.0,
        bid_size: 2.0,
        ask_price: 100.1,
        ask_size: 2.0,
    });
    stats.record(BboUpdate {
        market_id: 2,
        symbol: "ETH".to_string(),
        nonce: 1,
        exchange_timestamp_ms: 1,
        bid_price: 50.0,
        bid_size: 2.0,
        ask_price: 50.2,
        ask_size: 2.0,
    });
    stats.record(BboUpdate {
        market_id: 1,
        symbol: "BTC".to_string(),
        nonce: 2,
        exchange_timestamp_ms: 2,
        bid_price: 100.0,
        bid_size: 2.0,
        ask_price: 100.2,
        ask_size: 2.0,
    });

    let summary = stats.summary(Duration::from_millis(100), 2);

    assert_eq!(summary.events, 3);
    assert_eq!(summary.live_markets, 2);
    assert_eq!(summary.events_per_second, 30.0);
    assert_eq!(summary.top_spreads.len(), 2);
    assert_eq!(summary.top_spreads[0].symbol, "ETH");
    assert!(summary.top_spreads[0].spread_bps > summary.top_spreads[1].spread_bps);
}

#[test]
fn scan_stats_can_reset_after_subscription_warmup() {
    let mut stats = ScanStats::new();
    stats.record(BboUpdate {
        market_id: 1,
        symbol: "BTC".to_string(),
        nonce: 1,
        exchange_timestamp_ms: 1,
        bid_price: 100.0,
        bid_size: 1.0,
        ask_price: 100.1,
        ask_size: 1.0,
    });

    stats.reset();
    let summary = stats.summary(Duration::from_secs(1), 10);

    assert_eq!(summary.events, 0);
    assert_eq!(summary.live_markets, 0);
    assert!(summary.top_spreads.is_empty());
}

#[test]
fn scan_cli_defaults_to_all_mainnet_markets_in_observation_mode() {
    let cli = Cli::try_parse_from(["lighter-bot", "scan"]).expect("valid scan command");

    match cli.command {
        Commands::Scan {
            url,
            ws_url,
            duration,
            top,
            market_type,
        } => {
            assert_eq!(url, "https://mainnet.zklighter.elliot.ai");
            assert_eq!(ws_url, "wss://mainnet.zklighter.elliot.ai/stream");
            assert_eq!(duration, 30);
            assert_eq!(top, 10);
            assert_eq!(market_type, "all");
        }
        _ => panic!("expected scan command"),
    }
}
