use super::aster_interval_millis;
use super::aster_live::{
    account_totals, apply_ws_account_update, calculate_exposure, combined_book_ticker_url,
    maker_strategy_allowed, merge_rest_positions, modify_order_is_gone, one_way_positions,
    quote_replace_decision, rest_order_is_complete, signal_allowed, submission_failure_decision,
    unique_client_id, validate_runtime_positions, AsterLedger, ExposureInput, LiveDispatch,
    LocalOrder, LocalOrderStatus, OrderTracker, QuoteReplaceDecision, SubmissionFailureDecision,
};
use multi_venue_quant_bot::aster::{
    Account, AccountUpdate, AsterError, Income, Order, OrderSide, OrderTradeUpdate, OrderUpdate,
    PositionRisk, WsPosition,
};
use std::collections::HashMap;

fn local_order(
    strategy_key: &str,
    client_id: &str,
    order_id: Option<u64>,
    side: OrderSide,
    status: LocalOrderStatus,
) -> LocalOrder {
    LocalOrder {
        strategy_key: strategy_key.to_string(),
        exchange_client_id: client_id.to_string(),
        order_id,
        symbol: "BTCUSDT".to_string(),
        side,
        price: 100.0,
        quantity: 2.0,
        status,
        last_event_time: 0,
        last_transaction_time: 0,
    }
}

fn order_update(event_time: u64, status: &str) -> OrderTradeUpdate {
    OrderTradeUpdate {
        event_time,
        transaction_time: event_time,
        order: OrderUpdate {
            symbol: "BTCUSDT".to_string(),
            client_order_id: "exchange-1".to_string(),
            side: "BUY".to_string(),
            order_type: "LIMIT".to_string(),
            time_in_force: "GTX".to_string(),
            original_quantity: "2".to_string(),
            original_price: "100".to_string(),
            average_price: "0".to_string(),
            execution_type: "NEW".to_string(),
            status: status.to_string(),
            order_id: 42,
            last_filled_quantity: "0".to_string(),
            accumulated_filled_quantity: "0".to_string(),
            last_filled_price: "0".to_string(),
            commission: None,
            commission_asset: None,
            trade_time: event_time,
            trade_id: -1,
            maker: true,
            reduce_only: false,
            position_side: "BOTH".to_string(),
            realized_profit: "0".to_string(),
        },
    }
}

#[test]
fn live_dispatch_is_explicit_and_unknown_fails_closed() {
    assert_eq!(
        LiveDispatch::parse("lighter").unwrap(),
        LiveDispatch::Lighter
    );
    assert_eq!(LiveDispatch::parse("arcus").unwrap(), LiveDispatch::Arcus);
    assert_eq!(LiveDispatch::parse("aster").unwrap(), LiveDispatch::Aster);
    assert!(LiveDispatch::parse("").is_err());
    assert!(LiveDispatch::parse("other").is_err());
}

#[test]
fn maker_only_and_pause_gates_are_fail_closed() {
    assert!(maker_strategy_allowed("maker_quote"));
    assert!(maker_strategy_allowed("maker"));
    assert!(!maker_strategy_allowed("grid"));

    assert!(signal_allowed(false, false, &[7], 7, false, true));
    assert!(!signal_allowed(false, true, &[7], 7, false, true));
    assert!(!signal_allowed(false, false, &[7], 8, false, true));
    assert!(!signal_allowed(false, false, &[7], 7, false, false));
    assert!(signal_allowed(false, true, &[], 7, true, false));
    assert!(signal_allowed(false, false, &[7], 7, true, true));
    assert!(!signal_allowed(false, true, &[], 7, true, true));
    assert!(signal_allowed(true, true, &[], 99, false, false));
}

#[test]
fn order_state_ignores_old_events_and_terminal_wins() {
    let mut tracker = OrderTracker::default();
    tracker.insert(local_order(
        "mq_BTC_buy",
        "exchange-1",
        None,
        OrderSide::Buy,
        LocalOrderStatus::Pending,
    ));
    assert!(tracker.apply_ws_order(&order_update(20, "NEW")));
    assert!(!tracker.apply_ws_order(&order_update(19, "FILLED")));
    assert_eq!(
        tracker.by_strategy_key["mq_BTC_buy"].status,
        LocalOrderStatus::Live
    );
    let mut same_millisecond_terminal = order_update(20, "FILLED");
    same_millisecond_terminal.transaction_time = 21;
    assert!(tracker.apply_ws_order(&same_millisecond_terminal));
    assert!(!tracker.by_strategy_key.contains_key("mq_BTC_buy"));
}

#[test]
fn unknown_execution_requires_reconciliation() {
    let error = AsterError::UnknownExecution {
        message: "service unavailable".to_string(),
    };
    assert_eq!(
        submission_failure_decision(&error),
        SubmissionFailureDecision::Reconcile
    );
    assert_eq!(
        submission_failure_decision(&AsterError::InvalidRequest("bad".to_string())),
        SubmissionFailureDecision::Reject
    );
}

#[test]
fn exposure_deduplicates_rest_and_local_but_counts_pending() {
    let duplicate = local_order(
        "mq_buy",
        "rest-client",
        Some(10),
        OrderSide::Buy,
        LocalOrderStatus::Live,
    );
    let pending = local_order(
        "mq_sell",
        "pending-client",
        None,
        OrderSide::Sell,
        LocalOrderStatus::Pending,
    );
    let exposure = calculate_exposure(
        &ExposureInput {
            positions: vec![("BTCUSDT".to_string(), 1.0, 100.0)],
            rest_orders: vec![(
                "BTCUSDT".to_string(),
                "rest-client".to_string(),
                Some(10),
                OrderSide::Buy,
                100.0,
                2.0,
            )],
            local_orders: vec![duplicate, pending],
        },
        "BTCUSDT",
    );
    assert_eq!(exposure.symbol_position_notional, 100.0);
    assert_eq!(exposure.symbol_buy_open_notional, 200.0);
    assert_eq!(exposure.symbol_sell_open_notional, 200.0);
    assert_eq!(exposure.total_worst_case_notional, 300.0);
}

#[test]
fn ledger_ids_are_idempotent() {
    let mut ledger = AsterLedger::default();
    assert!(ledger.record_trade_id("BTCUSDT", 9));
    assert!(!ledger.record_trade_id("BTCUSDT", 9));
    let income = Income {
        symbol: "BTCUSDT".to_string(),
        income_type: "COMMISSION".to_string(),
        income: "-0.01".to_string(),
        asset: "USDT".to_string(),
        info: String::new(),
        time: 100,
        tran_id: "t".to_string(),
        trade_id: "9".to_string(),
    };
    assert!(ledger.record_income(&income));
    assert!(!ledger.record_income(&income));
}

#[test]
fn one_way_position_mapping_preserves_signed_side() {
    let positions = vec![
        position("1.5", "BOTH"),
        PositionRisk {
            symbol: "ETHUSDT".to_string(),
            position_amt: "-2".to_string(),
            ..position("0", "BOTH")
        },
    ];
    let mapped = one_way_positions(&positions).unwrap();
    assert_eq!(mapped[0]["side"], "Buy");
    assert_eq!(mapped[0]["size"], 1.5);
    assert_eq!(mapped[1]["side"], "Sell");
    assert_eq!(mapped[1]["size"], 2.0);
    assert!(one_way_positions(&[position("1", "LONG")]).is_err());
}

#[test]
fn account_updates_merge_partial_positions_and_ignore_older_events() {
    let mut positions = vec![position("1", "BOTH")];
    let mut last_events = HashMap::new();
    let prices = HashMap::from([("ETHUSDT".to_string(), 2000.0)]);
    let update = AccountUpdate {
        event_time: 20,
        transaction_time: 20,
        reason: "ORDER".into(),
        balances: Vec::new(),
        positions: vec![WsPosition {
            symbol: "ETHUSDT".into(),
            position_amount: "-2".into(),
            entry_price: "2100".into(),
            accumulated_realized: "0".into(),
            unrealized_pnl: "5".into(),
            margin_type: "isolated".into(),
            isolated_wallet: "10".into(),
            position_side: "BOTH".into(),
        }],
    };
    apply_ws_account_update(&mut positions, &update, &mut last_events, &prices).unwrap();
    assert_eq!(positions.len(), 2);
    assert_eq!(positions[0].position_amt, "1");
    assert_eq!(positions[1].position_amt, "-2");
    assert_eq!(positions[1].mark_price, "2000");

    let mut older = update.clone();
    older.event_time = 19;
    older.positions[0].position_amount = "-3".into();
    apply_ws_account_update(&mut positions, &older, &mut last_events, &prices).unwrap();
    assert_eq!(positions[1].position_amt, "-2");

    let mut same_event_later_transaction = update;
    same_event_later_transaction.transaction_time = 21;
    same_event_later_transaction.positions[0].position_amount = "-4".into();
    apply_ws_account_update(
        &mut positions,
        &same_event_later_transaction,
        &mut last_events,
        &prices,
    )
    .unwrap();
    assert_eq!(positions[1].position_amt, "-4");
}

#[test]
fn minimal_order_ack_is_not_treated_as_authoritative_open_order() {
    let minimal: Order =
        serde_json::from_str(r#"{"symbol":"BTCUSDT","orderId":42,"clientOrderId":"maker-1"}"#)
            .unwrap();
    assert!(!rest_order_is_complete(&minimal));

    let full: Order = serde_json::from_str(
        r#"{"symbol":"BTCUSDT","orderId":42,"clientOrderId":"maker-1","status":"NEW","side":"BUY","price":"50000","origQty":"0.001"}"#,
    )
    .unwrap();
    assert!(rest_order_is_complete(&full));
}

#[test]
fn stale_rest_snapshot_cannot_overwrite_newer_ws_position() {
    let mut current = vec![PositionRisk {
        symbol: "ETHUSDT".into(),
        position_amt: "-4".into(),
        update_time: 20,
        ..position("0", "BOTH")
    }];
    let mut watermarks = HashMap::from([("ETHUSDT".to_string(), (20, 21))]);
    let stale = PositionRisk {
        symbol: "ETHUSDT".into(),
        position_amt: "-1".into(),
        update_time: 19,
        ..position("0", "BOTH")
    };
    merge_rest_positions(&mut current, vec![stale], &mut watermarks);
    assert_eq!(current[0].position_amt, "-4");

    let newer = PositionRisk {
        symbol: "ETHUSDT".into(),
        position_amt: "-5".into(),
        update_time: 22,
        ..position("0", "BOTH")
    };
    merge_rest_positions(&mut current, vec![newer], &mut watermarks);
    assert_eq!(current[0].position_amt, "-5");
}

#[test]
fn runtime_margin_mode_drift_fails_closed() {
    let symbols = vec!["BTCUSDT".to_string()];
    let isolated = PositionRisk {
        margin_type: "isolated".into(),
        ..position("0", "BOTH")
    };
    assert!(validate_runtime_positions(&[isolated], &symbols, true).is_ok());

    let cross = PositionRisk {
        margin_type: "cross".into(),
        ..position("0", "BOTH")
    };
    assert!(validate_runtime_positions(&[cross], &symbols, true).is_err());
}

#[test]
fn account_totals_fall_back_to_margin_available_assets() {
    let account: Account = serde_json::from_str(
        r#"{"canTrade":true,"canDeposit":true,"canWithdraw":false,"totalWalletBalance":"0","totalUnrealizedProfit":"0","totalMarginBalance":"0","availableBalance":"0","maxWithdrawAmount":"0","assets":[{"asset":"USDF","walletBalance":"5000","unrealizedProfit":"0","marginBalance":"5000","availableBalance":"5000","marginAvailable":true}],"positions":[]}"#,
    )
    .unwrap();
    let (equity, available, unrealized) = account_totals(&account).unwrap();
    assert_eq!(equity, 5000.0);
    assert_eq!(available, 5000.0);
    assert_eq!(unrealized, 0.0);
}

fn position(amount: &str, side: &str) -> PositionRisk {
    PositionRisk {
        symbol: "BTCUSDT".to_string(),
        position_amt: amount.to_string(),
        entry_price: "90".to_string(),
        mark_price: "100".to_string(),
        un_realized_profit: "10".to_string(),
        liquidation_price: "50".to_string(),
        leverage: "2".to_string(),
        margin_type: "cross".to_string(),
        position_side: side.to_string(),
        update_time: 0,
    }
}

#[test]
fn client_ids_are_unique_and_within_aster_limit() {
    let first = unique_client_id(
        "mq_a_very_long_strategy_key_that_is_not_sent_to_exchange",
        u32::MAX,
        OrderSide::Buy,
    );
    let second = unique_client_id(
        "mq_a_very_long_strategy_key_that_is_not_sent_to_exchange",
        u32::MAX,
        OrderSide::Buy,
    );
    assert!(first.len() <= 36);
    assert!(second.len() <= 36);
    assert_ne!(first, second);
}

#[test]
fn aster_download_intervals_are_validated() {
    assert_eq!(aster_interval_millis("1m").unwrap(), 60_000);
    assert_eq!(aster_interval_millis("1h").unwrap(), 3_600_000);
    assert_eq!(aster_interval_millis("1d").unwrap(), 86_400_000);
    assert!(aster_interval_millis("7m").is_err());
}

#[test]
fn market_stream_combines_bbo_and_fast_partial_depth() {
    let url = combined_book_ticker_url(&["BTCUSDT".into()]);
    assert!(url.contains("btcusdt@bookTicker"));
    assert!(url.contains("btcusdt@depth20@100ms"));
}

#[test]
fn live_maker_requotes_amend_when_the_order_id_is_known() {
    let live = local_order(
        "mq_BTC_buy",
        "exchange-1",
        Some(42),
        OrderSide::Buy,
        LocalOrderStatus::Live,
    );
    assert_eq!(
        quote_replace_decision(&live, 100.0, true),
        QuoteReplaceDecision::Noop
    );
    assert_eq!(
        quote_replace_decision(&live, 99.9, true),
        QuoteReplaceDecision::Amend
    );
    assert_eq!(
        quote_replace_decision(&live, 99.9, false),
        QuoteReplaceDecision::CancelThenWait
    );

    let pending = local_order(
        "mq_BTC_buy",
        "exchange-1",
        Some(42),
        OrderSide::Buy,
        LocalOrderStatus::Pending,
    );
    assert_eq!(
        quote_replace_decision(&pending, 99.9, true),
        QuoteReplaceDecision::BlockedUnresolved
    );

    let live_without_id = local_order(
        "mq_BTC_buy",
        "exchange-1",
        None,
        OrderSide::Buy,
        LocalOrderStatus::Live,
    );
    assert_eq!(
        quote_replace_decision(&live_without_id, 99.9, true),
        QuoteReplaceDecision::CancelThenWait
    );
}

#[test]
fn modify_gone_errors_are_order_not_found_only() {
    assert!(modify_order_is_gone(&AsterError::Api {
        status: 400,
        code: Some(-2011),
        message: "Unknown order sent".into(),
    }));
    assert!(modify_order_is_gone(&AsterError::Api {
        status: 400,
        code: Some(-2013),
        message: "Order does not exist.".into(),
    }));
    assert!(!modify_order_is_gone(&AsterError::Api {
        status: 400,
        code: Some(-4016),
        message: "Price not increased".into(),
    }));
    assert!(!modify_order_is_gone(&AsterError::UnknownExecution {
        message: "service unavailable".into(),
    }));
}
