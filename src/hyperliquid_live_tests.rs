//! Unit tests for the Hyperliquid live-loop policy helpers.

use crate::hyperliquid_live::{
    account_totals, cancel_asset_for_tracked_order, cancel_error_means_gone,
    hyperliquid_anyhow_is_transient, hyperliquid_ws_is_recoverable, hyperliquid_ws_is_session_age,
    l1_request_budget_exhausted,
    positions_json, quote_replace_decision, submission_failure_decision, subscription_messages,
    tracked_position, validate_runtime_positions, HyperliquidLedger, LocalOrder, LocalOrderStatus,
    OrderTracker, QuoteReplaceDecision, SubmissionFailureDecision, TrackedPosition,
};
use multi_venue_quant_bot::hyperliquid::{
    ClearinghouseState, FundingDelta, HyperliquidError, Leverage, PerpPosition, UserFundingEntry,
    WsOrderState, WsOrderUpdate,
};

fn local_order(strategy_key: &str, status: LocalOrderStatus) -> LocalOrder {
    LocalOrder {
        strategy_key: strategy_key.to_string(),
        cloid: format!("0x{:032x}", strategy_key.len()),
        oid: None,
        coin: "io:SNDK".to_string(),
        is_buy: true,
        price: 40.0,
        quantity: 1.0,
        status,
        last_status_time: 0,
    }
}

fn ws_update(oid: u64, cloid: Option<&str>, status: &str, status_time: u64) -> WsOrderUpdate {
    WsOrderUpdate {
        order: WsOrderState {
            coin: "io:SNDK".to_string(),
            side: "B".to_string(),
            limit_px: "41.5".to_string(),
            sz: "1.0".to_string(),
            oid,
            timestamp: status_time,
            orig_sz: "1.0".to_string(),
            cloid: cloid.map(str::to_string),
        },
        status: status.to_string(),
        status_timestamp: status_time,
    }
}

fn perp_position(coin: &str, szi: &str, kind: &str) -> PerpPosition {
    PerpPosition {
        coin: coin.to_string(),
        szi: szi.to_string(),
        entry_px: Some("40.0".to_string()),
        position_value: "80.0".to_string(),
        unrealized_pnl: "1.5".to_string(),
        liquidation_px: None,
        margin_used: "40.0".to_string(),
        leverage: Leverage {
            kind: kind.to_string(),
            value: 2,
        },
    }
}

#[test]
fn tracker_links_ws_updates_by_cloid_and_oid() {
    let mut tracker = OrderTracker::default();
    let order = local_order("mq_io:SNDK_buy", LocalOrderStatus::Pending);
    let cloid = order.cloid.clone();
    tracker.insert(order);

    // First update binds the oid and promotes to Live.
    assert!(tracker.apply_ws_order(&ws_update(77, Some(&cloid), "open", 10)));
    let local = &tracker.by_strategy_key["mq_io:SNDK_buy"];
    assert_eq!(local.oid, Some(77));
    assert_eq!(local.status, LocalOrderStatus::Live);
    assert!((local.price - 41.5).abs() < 1e-9);

    // Stale updates (same or older statusTimestamp) are ignored.
    assert!(!tracker.apply_ws_order(&ws_update(77, None, "canceled", 10)));
    assert!(tracker.by_strategy_key.contains_key("mq_io:SNDK_buy"));

    // A newer terminal update removes the order, matched by oid alone.
    assert!(tracker.apply_ws_order(&ws_update(77, None, "filled", 11)));
    assert!(tracker.by_strategy_key.is_empty());
}

#[test]
fn tracker_ignores_unknown_orders() {
    let mut tracker = OrderTracker::default();
    assert!(!tracker.apply_ws_order(&ws_update(1, Some("0xabc"), "open", 1)));
}

#[test]
fn quote_replace_requires_a_resolved_live_order() {
    let live = local_order("k", LocalOrderStatus::Live);
    assert_eq!(
        quote_replace_decision(&live, live.price),
        QuoteReplaceDecision::Noop
    );
    assert_eq!(
        quote_replace_decision(&live, live.price + 0.5),
        QuoteReplaceDecision::CancelThenWait
    );
    for status in [LocalOrderStatus::Pending, LocalOrderStatus::Unknown] {
        let unresolved = local_order("k", status);
        assert_eq!(
            quote_replace_decision(&unresolved, unresolved.price + 0.5),
            QuoteReplaceDecision::BlockedUnresolved
        );
    }
}

#[test]
fn ambiguous_submission_failures_reconcile_and_rejections_fail() {
    assert_eq!(
        submission_failure_decision(&HyperliquidError::RateLimited {
            message: "429".into()
        }),
        SubmissionFailureDecision::Reconcile
    );
    assert_eq!(
        submission_failure_decision(&HyperliquidError::InvalidResponse("garbled".into())),
        SubmissionFailureDecision::Reconcile
    );
    assert_eq!(
        submission_failure_decision(&HyperliquidError::ActionRejected {
            message: "User or API Wallet does not exist".into()
        }),
        SubmissionFailureDecision::Reject
    );
    assert_eq!(
        submission_failure_decision(&HyperliquidError::InvalidRequest("bad price".into())),
        SubmissionFailureDecision::Reject
    );
    assert_eq!(
        submission_failure_decision(&HyperliquidError::ActionRejected {
            message: "Too many cumulative requests sent (14295 > 14294) for cumulative volume traded $4295.52. Place taker orders to free up 1 request per USDC traded.".into()
        }),
        SubmissionFailureDecision::Skip
    );
    assert!(l1_request_budget_exhausted(
        "Too many cumulative requests sent (1 > 0) for cumulative volume traded $0"
    ));
    assert!(!l1_request_budget_exhausted(
        "User or API Wallet does not exist"
    ));
}

#[test]
fn cancel_rejections_for_gone_orders_are_terminal() {
    assert!(cancel_error_means_gone(
        "Order was never placed, already canceled, or filled."
    ));
    assert!(!cancel_error_means_gone("Vault not registered"));
}

#[test]
fn runtime_positions_reject_unconfigured_coins_and_cross_margin() {
    let coins = vec!["io:SNDK".to_string()];
    let flat_unknown = tracked_position(&perp_position("BTC", "0", "cross")).unwrap();
    let isolated = tracked_position(&perp_position("io:SNDK", "2.0", "isolated")).unwrap();
    assert!(validate_runtime_positions(&[flat_unknown, isolated.clone()], &coins, true).is_ok());

    let open_unknown = tracked_position(&perp_position("BTC", "1.0", "isolated")).unwrap();
    assert!(validate_runtime_positions(&[open_unknown], &coins, true).is_err());

    let crossed = tracked_position(&perp_position("io:SNDK", "2.0", "cross")).unwrap();
    assert!(validate_runtime_positions(std::slice::from_ref(&crossed), &coins, true).is_err());
    assert!(validate_runtime_positions(&[crossed], &coins, false).is_ok());
}

#[test]
fn tracked_position_derives_mark_price_from_position_value() {
    let position = tracked_position(&perp_position("io:SNDK", "2.0", "isolated")).unwrap();
    assert!((position.mark_px - 40.0).abs() < 1e-9);
    assert!((position.szi - 2.0).abs() < 1e-9);
    let json = positions_json(&[position]);
    assert_eq!(json.len(), 1);
    assert_eq!(json[0]["side"], "Buy");
    assert_eq!(json[0]["symbol"], "io:SNDK");

    let flat = tracked_position(&perp_position("io:SNDK", "0", "isolated")).unwrap();
    assert!(positions_json(&[flat]).is_empty());

    let short = TrackedPosition {
        coin: "io:ANTH".to_string(),
        szi: -1.0,
        entry_px: 10.0,
        mark_px: 9.0,
        unrealized_pnl: 1.0,
        leverage_kind: "isolated".to_string(),
        leverage_value: 1.0,
    };
    assert_eq!(positions_json(&[short])[0]["side"], "Sell");
}

#[test]
fn account_totals_sum_across_builder_dexs() {
    let state: ClearinghouseState = serde_json::from_value(serde_json::json!({
        "marginSummary": {
            "accountValue": "100.5",
            "totalNtlPos": "80.0",
            "totalMarginUsed": "40.0"
        },
        "withdrawable": "60.5",
        "assetPositions": [{
            "position": {
                "coin": "io:SNDK",
                "szi": "2.0",
                "entryPx": "40.0",
                "positionValue": "80.0",
                "unrealizedPnl": "1.5",
                "marginUsed": "40.0",
                "leverage": {"type": "isolated", "value": 1}
            }
        }],
        "time": 1u64
    }))
    .unwrap();
    let io_state: ClearinghouseState = serde_json::from_value(serde_json::json!({
        "marginSummary": {
            "accountValue": "25.0",
            "totalNtlPos": "0.0",
            "totalMarginUsed": "0.0"
        },
        "withdrawable": "25.0",
        "assetPositions": [],
        "time": 2u64
    }))
    .unwrap();
    let (equity, available, unrealized) =
        account_totals(&[state.clone(), io_state.clone()], None).unwrap();
    assert!((equity - 125.5).abs() < 1e-9);
    assert!((available - 85.5).abs() < 1e-9);
    assert!((unrealized - 1.5).abs() < 1e-9);

    let (unified_equity, unified_available, unified_unrealized) =
        account_totals(&[state, io_state], Some((2504.846418, 2504.846418))).unwrap();
    assert!((unified_equity - 2504.846418).abs() < 1e-9);
    assert!((unified_available - 2504.846418).abs() < 1e-9);
    assert!((unified_unrealized - 1.5).abs() < 1e-9);
}

#[test]
fn ledger_deduplicates_fills_and_funding() {
    let mut ledger = HyperliquidLedger::default();
    assert!(ledger.record_fill(1_000, 42));
    assert!(!ledger.record_fill(1_000, 42));
    assert!(ledger.record_fill(999, 43));
    assert_eq!(ledger.fill_high_water_ms, 1_000);

    let entry = UserFundingEntry {
        time: 2_000,
        delta: FundingDelta {
            coin: "io:SNDK".to_string(),
            usdc: "-0.12".to_string(),
            szi: "2.0".to_string(),
            funding_rate: "0.0001".to_string(),
        },
        hash: "0xdead".to_string(),
    };
    assert!(ledger.record_funding(&entry));
    assert!(!ledger.record_funding(&entry));
    assert_eq!(ledger.funding_high_water_ms, 2_000);
}

#[test]
fn subscriptions_cover_every_coin_plus_user_streams() {
    let coins = vec!["io:SNDK".to_string(), "io:ANTH".to_string()];
    let messages = subscription_messages(&coins, "0xabc");
    assert_eq!(messages.len(), 4);
    assert!(messages[0].contains(r#""type":"bbo""#) && messages[0].contains("io:SNDK"));
    assert!(messages[1].contains("io:ANTH"));
    assert!(messages[2].contains(r#""type":"orderUpdates""#));
    assert!(messages[3].contains(r#""type":"userFills""#));
    assert!(messages[2].contains("0xabc") && messages[3].contains("0xabc"));
}

#[test]
fn cancel_asset_uses_configured_market_id_not_thread_local() {
    let existing = local_order("mq_io:ANTH_buy", LocalOrderStatus::Live);
    let mut anth = existing.clone();
    anth.coin = "io:ANTH".to_string();
    assert_eq!(
        cancel_asset_for_tracked_order(&anth, "io:ANTH", 200001).unwrap(),
        200001
    );
    assert_eq!(
        cancel_asset_for_tracked_order(
            &local_order("mq_io:SNDK_buy", LocalOrderStatus::Live),
            "io:SNDK",
            200002,
        )
        .unwrap(),
        200002
    );
    let err = cancel_asset_for_tracked_order(&anth, "io:SNDK", 200002).unwrap_err();
    assert!(err.to_string().contains("does not match market io:SNDK"));
}

#[test]
fn websocket_disconnect_is_recoverable_and_session_age_is_not() {
    assert!(hyperliquid_ws_is_recoverable(&anyhow::anyhow!(
        "Hyperliquid WebSocket disconnected"
    )));
    assert!(hyperliquid_ws_is_recoverable(&anyhow::anyhow!(
        "Hyperliquid WebSocket failed"
    ).context("io")));
    assert!(hyperliquid_ws_is_recoverable(&anyhow::anyhow!(
        "Connection reset by peer (os error 104)"
    )));
    assert!(!hyperliquid_ws_is_recoverable(&anyhow::anyhow!(
        "Hyperliquid WebSocket returned an error: auth"
    )));
    assert!(hyperliquid_ws_is_session_age(&anyhow::anyhow!(
        "Hyperliquid WebSocket session reached safe restart age"
    )));
    assert!(!hyperliquid_ws_is_session_age(&anyhow::anyhow!(
        "Hyperliquid WebSocket disconnected"
    )));
}

#[test]
fn timeout_and_unknown_oid_are_transient_and_do_not_pause() {
    assert!(hyperliquid_anyhow_is_transient(&anyhow::anyhow!(
        "Hyperliquid order status remained unknown: transport timeout"
    )));
    assert!(hyperliquid_anyhow_is_transient(&anyhow::anyhow!(
        "Hyperliquid could not prove the ambiguous order's state (unknownOid)"
    )));
    assert!(hyperliquid_anyhow_is_transient(&anyhow::anyhow!(
        "API error HTTP 502"
    )));
    assert!(!hyperliquid_anyhow_is_transient(&anyhow::anyhow!(
        "Hyperliquid WebSocket returned an error: auth"
    )));
}

#[test]
fn live_loop_reconnects_recoverable_websocket_drops() {
    let src = include_str!("hyperliquid_live.rs");
    assert!(src.contains("hyperliquid_ws_is_recoverable"));
    assert!(src.contains("reconnecting without pausing"));
    assert!(src.contains("open_hyperliquid_ws"));
}

#[test]
fn cross_dex_basis_probe_is_spawned_and_single_flight() {
    let src = include_str!("hyperliquid_live.rs");
    assert!(src.contains("BASIS_PROBE_IN_FLIGHT"));
    assert!(src.contains("struct BasisProbeGuard"));
    assert!(src.contains("probe_io_xyz_sndk_basis"));
    assert!(src.contains("compare_exchange(false, true"));
    assert!(
        src.contains("tokio::spawn(async move {"),
        "basis probe must leave the WS select"
    );
    assert!(
        !src.contains("probe_io_xyz_sndk_basis(&client, &dashboard_state).await"),
        "must not await the probe inside tokio::select"
    );
}
