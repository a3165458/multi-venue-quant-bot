use config::{Config, File, FileFormat};

use super::profitability::{ProfitabilityGuard, SignalEconomics};

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

    let nan_buffer = settings(
        r#"
profitability:
  enabled: true
  min_net_edge_bps: .nan
"#,
    );
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
