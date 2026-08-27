use std::collections::HashMap;

use config::{Config, File, FileFormat};

use crate::lighter::types::{OrderType, Position, Side, TradeSignal};

use super::profitability::{ProfitabilityGuard, SignalEconomics};
use super::risk_manager::{RiskExposure, RiskManager};

fn settings(yaml: &str) -> Config {
    Config::builder()
        .add_source(File::from_str(yaml, FileFormat::Yaml))
        .build()
        .expect("test config")
}

#[test]
fn accepts_only_when_expected_edge_exceeds_all_costs_and_buffer() {
    let guard = ProfitabilityGuard::from_config(&settings(
        r#"
profitability:
  enabled: true
  entry_fee_bps: 2.0
  exit_fee_bps: 2.0
  entry_slippage_bps: 1.0
  exit_slippage_bps: 1.0
  funding_bps: 0.5
  adverse_selection_bps: 1.5
  min_net_edge_bps: 2.0
"#,
    ))
    .expect("valid guard");

    let accepted = guard.evaluate(SignalEconomics::entry(Some(11.0)));
    assert!(accepted.allowed);
    assert!((accepted.total_cost_bps - 8.0).abs() < 1e-12);
    assert!((accepted.net_edge_bps.expect("net edge") - 3.0).abs() < 1e-12);

    let break_even = guard.evaluate(SignalEconomics::entry(Some(10.0)));
    assert!(!break_even.allowed, "打平安全垫不能算可交易利润");
}

#[test]
fn rejects_entry_when_strategy_cannot_quantify_expected_edge() {
    let guard = ProfitabilityGuard::from_config(&settings(
        r#"
profitability:
  enabled: true
  entry_fee_bps: 0
  exit_fee_bps: 0
  entry_slippage_bps: 0
  exit_slippage_bps: 0
  funding_bps: 0
  adverse_selection_bps: 0
  min_net_edge_bps: 0
"#,
    ))
    .expect("valid guard");

    let decision = guard.evaluate(SignalEconomics::entry(None));
    assert!(!decision.allowed);
    assert_eq!(decision.reason, "missing_expected_edge");
}

#[test]
fn always_allows_risk_reducing_exit() {
    let guard = ProfitabilityGuard::from_config(&settings(
        r#"
profitability:
  enabled: true
  entry_fee_bps: 20
  exit_fee_bps: 20
  entry_slippage_bps: 10
  exit_slippage_bps: 10
  funding_bps: 5
  adverse_selection_bps: 5
  min_net_edge_bps: 10
"#,
    ))
    .expect("valid guard");

    let decision = guard.evaluate(SignalEconomics::exit());
    assert!(decision.allowed);
    assert_eq!(decision.reason, "risk_reducing_exit");
}

#[test]
fn rejects_non_finite_or_structurally_invalid_configuration() {
    let negative_slippage = settings(
        r#"
profitability:
  enabled: true
  entry_slippage_bps: -1
"#,
    );
    assert!(ProfitabilityGuard::from_config(&negative_slippage).is_err());

    let nan_buffer = Config::builder()
        .set_override("profitability.enabled", true)
        .expect("enabled override")
        .set_override("profitability.min_net_edge_bps", f64::NAN)
        .expect("NaN override")
        .build()
        .expect("test config");
    assert!(ProfitabilityGuard::from_config(&nan_buffer).is_err());
}

#[test]
fn supports_maker_rebates_as_negative_fee_costs() {
    let guard = ProfitabilityGuard::from_config(&settings(
        r#"
profitability:
  enabled: true
  entry_fee_bps: -1
  exit_fee_bps: -1
  entry_slippage_bps: 1
  exit_slippage_bps: 1
  funding_bps: 0
  adverse_selection_bps: 1
  min_net_edge_bps: 1
"#,
    ))
    .expect("maker rebates are valid");

    let decision = guard.evaluate(SignalEconomics::entry(Some(3.0)));
    assert!(decision.allowed);
    assert!((decision.total_cost_bps - 1.0).abs() < 1e-12);
}

#[test]
fn post_only_maker_quotes_use_maker_cost_not_round_trip() {
    let guard = ProfitabilityGuard::from_config(&settings(
        r#"
profitability:
  enabled: true
  entry_fee_bps: 1.5
  exit_fee_bps: 4.5
  entry_slippage_bps: 0.0
  exit_slippage_bps: 1.0
  funding_bps: 1.0
  adverse_selection_bps: 2.0
  min_net_edge_bps: 1.0
"#,
    ))
    .expect("valid guard");

    let taker = guard.evaluate(SignalEconomics::entry(Some(0.5)));
    assert!(!taker.allowed);
    assert_eq!(taker.reason, "insufficient_net_edge");

    let tight = guard.evaluate(SignalEconomics::maker_entry(Some(0.5)));
    assert!(!tight.allowed);
    assert_eq!(tight.reason, "insufficient_net_edge");
    assert!((tight.total_cost_bps - 3.5).abs() < 1e-9);

    // maker cost 3.5 + min_net 1.0 → need edge > 4.5
    let wide = guard.evaluate(SignalEconomics::maker_entry(Some(5.0)));
    assert!(wide.allowed);
    assert_eq!(wide.reason, "post_only_maker");

    let missing = guard.evaluate(SignalEconomics::maker_entry(None));
    assert!(!missing.allowed);
    assert_eq!(missing.reason, "missing_expected_edge");
}

#[test]
fn hip3_growth_schedule_allows_join_best_only_on_wide_books() {
    let guard = ProfitabilityGuard::from_config(&settings(
        r#"
profitability:
  enabled: true
  entry_fee_bps: 1.5
  exit_fee_bps: 4.5
  entry_slippage_bps: 0.0
  exit_slippage_bps: 1.0
  funding_bps: 1.0
  adverse_selection_bps: 2.0
  min_net_edge_bps: 1.0
"#,
    ))
    .expect("valid guard")
    .with_schedule(
        super::profitability::HIP3_GROWTH_MAKER_FEE_BPS,
        super::profitability::HIP3_GROWTH_TAKER_FEE_BPS,
        super::profitability::HIP3_GROWTH_ADVERSE_BPS,
    )
    .expect("hip3 schedule");

    // io:SNDK live half-spread ~0.33 bps cannot cover 0.29+0.73 + 1.0 buffer
    let tight = guard.evaluate(SignalEconomics::maker_entry(Some(0.33)));
    assert!(!tight.allowed);
    assert_eq!(tight.reason, "insufficient_net_edge");

    // 6 bps book → 3 bps half-spread > 1.02 cost + 1.0 buffer
    let wide = guard.evaluate(SignalEconomics::maker_entry(Some(3.0)));
    assert!(wide.allowed);
    assert_eq!(wide.reason, "post_only_maker");
    assert!((wide.total_cost_bps - 1.02).abs() < 1e-9);
}

#[test]
fn hip3_growth_maker_cost_ignores_hl_perp_t4_zero_add() {
    // Observed io: maker fills paid ~0.29 bps even while HL userFees showed T0.
    // T4 (perp add=0) must not zero this schedule or join-best on 2 bps books
    // would look profitable and repeat the -1.02 bps realized loss.
    let guard = ProfitabilityGuard::from_config(&settings(
        r#"
profitability:
  enabled: true
  entry_fee_bps: 0.0
  exit_fee_bps: 0.0
  entry_slippage_bps: 0.0
  exit_slippage_bps: 0.0
  funding_bps: 0.0
  adverse_selection_bps: 0.0
  min_net_edge_bps: 1.0
"#,
    ))
    .expect("valid guard")
    .with_schedule(
        super::profitability::HIP3_GROWTH_MAKER_FEE_BPS,
        super::profitability::HIP3_GROWTH_TAKER_FEE_BPS,
        super::profitability::HIP3_GROWTH_ADVERSE_BPS,
    )
    .expect("hip3 schedule");
    assert!((guard.maker_cost_bps() - 1.02).abs() < 1e-9);
    let at_floor = guard.evaluate(SignalEconomics::maker_entry(Some(2.02)));
    assert!(!at_floor.allowed);
    assert_eq!(at_floor.reason, "insufficient_net_edge");
    let over_floor = guard.evaluate(SignalEconomics::maker_entry(Some(2.03)));
    assert!(over_floor.allowed);
    assert_eq!(over_floor.reason, "post_only_maker");
}

fn live_settings() -> Config {
    settings(
        r#"
profitability:
  enabled: true
  entry_fee_bps: 2
  exit_fee_bps: 2
  entry_slippage_bps: 1
  exit_slippage_bps: 1
  funding_bps: 0
  adverse_selection_bps: 1
  min_net_edge_bps: 2
risk:
  stop_loss:
    max_drawdown_percent: 10
    daily_loss_limit_percent: 5
    position_stop_loss_percent: 3
    position_take_profit_percent: 5
  position_limit:
    max_leverage: 3
    max_position_size: 100
trading:
  position:
    max_single_trade_percent: 50
    max_total_position_percent: 100
"#,
    )
}

fn signal(expected_edge_bps: Option<f64>, risk_reducing: bool) -> TradeSignal {
    TradeSignal {
        symbol: "BTC".to_string(),
        market_id: 1,
        side: Side::Buy,
        price: 100.0,
        quantity: 1.0,
        order_type: OrderType::Limit,
        reason: "profitability integration test".to_string(),
        timestamp: chrono::Utc::now(),
        expected_edge_bps,
        risk_reducing,
        ..Default::default()
    }
}

#[tokio::test]
async fn risk_manager_enforces_profitability_before_position_limits_pass() {
    let manager = RiskManager::new(&live_settings()).expect("risk manager");

    assert!(!manager
        .check_signal(&signal(Some(9.0), false))
        .await
        .expect("decision"));
    assert!(manager
        .check_signal(&signal(Some(10.0), false))
        .await
        .expect("decision"));

    let mut tight = signal(Some(0.3), false);
    tight.post_only = true;
    let tight_allowed = manager.check_signal(&tight).await.expect("decision");
    if tight_allowed {
        panic!("join-best ALO on a sub-bps book must fail the maker-cost gate");
    }

    let mut wide = signal(Some(6.0), false);
    wide.post_only = true;
    assert!(
        manager.check_signal(&wide).await.expect("decision"),
        "ALO quotes that clear maker fee + adverse + min_net must pass"
    );
}

#[tokio::test]
async fn risk_manager_never_blocks_explicit_risk_reduction_for_profitability() {
    let manager = RiskManager::new(&live_settings()).expect("risk manager");

    assert!(manager
        .check_signal(&signal(None, true))
        .await
        .expect("decision"));
}

#[tokio::test]
async fn risk_manager_rejects_projected_symbol_position_above_cap() {
    let mut manager = RiskManager::new(&live_settings()).expect("risk manager");
    manager.update_equity(1_000.0);
    let exposure = RiskExposure {
        symbol_position_notional: 80.0,
        symbol_buy_open_notional: 0.0,
        symbol_sell_open_notional: 0.0,
        total_worst_case_notional: 80.0,
    };
    let mut entry = signal(Some(10.0), false);
    entry.quantity = 0.3;

    assert!(
        !manager
            .check_signal_with_exposure(&entry, exposure)
            .await
            .expect("decision"),
        "80 current + 30 order must exceed the $100 symbol cap"
    );
}

#[test]
fn position_loss_and_profit_thresholds_emit_opposite_side_closes() {
    let manager = RiskManager::new(&live_settings()).expect("risk manager");
    let long = Position {
        symbol: "BTC".into(),
        side: Side::Buy,
        size: 2.0,
        entry_price: 100.0,
        unrealized_pnl: -8.0,
        leverage: 1.0,
    };

    let stop = manager.check_position_stop_loss_take_profit(
        std::slice::from_ref(&long),
        &HashMap::from([("BTC".to_string(), 96.0)]),
    );
    assert_eq!(stop.len(), 1);
    assert_eq!(stop[0].side_to_close, Side::Sell);
    assert_eq!(stop[0].size, 2.0);

    let take_profit = manager.check_position_stop_loss_take_profit(
        &[long],
        &HashMap::from([("BTC".to_string(), 106.0)]),
    );
    assert_eq!(take_profit.len(), 1);
    assert_eq!(take_profit[0].side_to_close, Side::Sell);
}

#[tokio::test]
async fn risk_manager_rejects_projected_total_worst_case_exposure() {
    let mut manager = RiskManager::new(&live_settings()).expect("risk manager");
    manager.update_equity(1_000.0);
    let exposure = RiskExposure {
        symbol_position_notional: 0.0,
        symbol_buy_open_notional: 50.0,
        symbol_sell_open_notional: 0.0,
        total_worst_case_notional: 950.0,
    };

    assert!(
        !manager
            .check_signal_with_exposure(&signal(Some(10.0), false), exposure)
            .await
            .expect("decision"),
        "$950 worst-case exposure with $50 already bid plus a new $100 bid must exceed the cap"
    );
}

#[tokio::test]
async fn opposite_quote_does_not_double_count_worst_case_exposure() {
    let mut manager = RiskManager::new(&live_settings()).expect("risk manager");
    manager.update_equity(1_000.0);
    let exposure = RiskExposure {
        symbol_position_notional: 0.0,
        symbol_buy_open_notional: 100.0,
        symbol_sell_open_notional: 0.0,
        total_worst_case_notional: 1_000.0,
    };
    let mut opposite = signal(Some(10.0), false);
    opposite.side = Side::Sell;

    assert!(
        manager
            .check_signal_with_exposure(&opposite, exposure)
            .await
            .expect("decision"),
        "an equal opposite quote leaves worst-case directional exposure unchanged"
    );
}

#[tokio::test]
async fn risk_reduction_is_allowed_after_daily_loss_limit() {
    let mut manager = RiskManager::new(&live_settings()).expect("risk manager");
    manager.update_equity(1_000.0);
    manager.update_daily_pnl(-60.0);

    assert!(
        manager
            .check_signal_with_exposure(
                &signal(None, true),
                RiskExposure {
                    symbol_position_notional: -200.0,
                    symbol_buy_open_notional: 0.0,
                    symbol_sell_open_notional: 0.0,
                    total_worst_case_notional: 200.0,
                },
            )
            .await
            .expect("decision"),
        "loss gates must not prevent an explicit position reduction"
    );
}
