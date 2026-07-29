use config::{Config, File, FileFormat};

use crate::lighter::types::{OrderType, Side, TradeSignal};

use super::profitability::{ProfitabilityGuard, SignalEconomics};
use super::risk_manager::RiskManager;

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
    max_position_size: 10000
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
}

#[tokio::test]
async fn risk_manager_never_blocks_explicit_risk_reduction_for_profitability() {
    let manager = RiskManager::new(&live_settings()).expect("risk manager");

    assert!(manager
        .check_signal(&signal(None, true))
        .await
        .expect("decision"));
}
