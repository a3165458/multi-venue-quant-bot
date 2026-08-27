use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const AUDIT_FILE: &str = "quant_agent_audit.json";
const MAX_AUDIT_RECORDS: usize = 200;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BacktestEvidence {
    pub data_file: String,
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub end: String,
    #[serde(default)]
    pub capital: f64,
    pub total_return_pct: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown_pct: f64,
    pub total_trades: u64,
    #[serde(default)]
    pub peak_notional_pct: f64,
    #[serde(default)]
    pub validation_return_pct: f64,
    #[serde(default)]
    pub validation_sharpe_ratio: f64,
    #[serde(default)]
    pub validation_max_drawdown_pct: f64,
    #[serde(default)]
    pub validation_total_trades: u64,
    #[serde(default)]
    pub validation_peak_notional_pct: f64,
    #[serde(default)]
    pub rolling_days: u64,
    #[serde(default)]
    pub rolling_profitable_days: u64,
    #[serde(default)]
    pub cash_open_return_pct: f64,
    #[serde(default)]
    pub cash_open_trades: u64,
    #[serde(default)]
    pub validation_market_count: u64,
    #[serde(default)]
    pub validation_profitable_market_count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProposalInput {
    pub strategy: String,
    pub params: HashMap<String, serde_json::Value>,
    pub evidence: BacktestEvidence,
    #[serde(default)]
    pub rationale: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PolicySnapshot {
    pub equity: f64,
    pub trading_paused: bool,
    pub emergency_triggered: bool,
    pub max_drawdown_pct: f64,
    pub max_notional_pct: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PolicyDecision {
    pub allowed: bool,
    pub checks: Vec<String>,
    pub violations: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentProposal {
    pub id: String,
    pub created_at: String,
    pub status: String,
    pub input: ProposalInput,
    pub policy: PolicySnapshot,
    pub decision: PolicyDecision,
    pub approval_phrase: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AgentLedger {
    pub proposals: Vec<AgentProposal>,
}

fn numeric(params: &HashMap<String, serde_json::Value>, key: &str) -> Option<f64> {
    params.get(key).and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    })
}

fn boolean(params: &HashMap<String, serde_json::Value>, key: &str) -> Option<bool> {
    params.get(key).and_then(|value| match value {
        serde_json::Value::Bool(flag) => Some(*flag),
        serde_json::Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

fn param_string(params: &HashMap<String, serde_json::Value>, key: &str) -> Option<String> {
    params.get(key).and_then(|value| match value {
        serde_json::Value::String(text) => Some(text.clone()),
        other if !other.is_null() => Some(other.to_string().trim_matches('"').to_string()),
        _ => None,
    })
}

pub fn validate_strategy_params(
    strategy: &str,
    params: &HashMap<String, serde_json::Value>,
    policy: &PolicySnapshot,
) -> Vec<String> {
    let mut violations = Vec::new();
    let notional_cap = policy.equity * policy.max_notional_pct / 100.0;
    match strategy {
        "grid" | "grid_trading" => {
            let count = numeric(params, "grid_count");
            let investment =
                numeric(params, "investment").or_else(|| numeric(params, "investment_per_grid"));
            let deviation =
                numeric(params, "deviation").or_else(|| numeric(params, "price_deviation"));
            if !matches!(count, Some(v) if (4.0..=40.0).contains(&v)) {
                violations.push("grid_count must be between 4 and 40".to_string());
            }
            if !matches!(investment, Some(v) if v > 0.0 && v <= notional_cap) {
                violations.push(format!("grid investment exceeds ${notional_cap:.2} cap"));
            }
            if !matches!(deviation, Some(v) if (0.001..=0.05).contains(&v)) {
                violations.push("grid deviation must be between 0.001 and 0.05".to_string());
            }
        }
        "trend" | "trend_following" => {
            let fast = numeric(params, "fast_ma");
            let slow = numeric(params, "slow_ma");
            let notional = numeric(params, "notional");
            if !matches!((fast, slow), (Some(f), Some(s)) if f >= 2.0 && f < s && s <= 500.0) {
                violations.push(
                    "trend moving averages must satisfy 2 <= fast_ma < slow_ma <= 500".to_string(),
                );
            }
            if !matches!(notional, Some(v) if v > 0.0 && v <= notional_cap) {
                violations.push(format!("trend notional exceeds ${notional_cap:.2} cap"));
            }
        }
        "maker_quote" | "maker" => {
            let spread = numeric(params, "spread_bps");
            let quote = numeric(params, "per_quote_notional");
            let requote = numeric(params, "requote_threshold_bps");
            let cooldown = numeric(params, "requote_cooldown_secs");
            let soft_cap = numeric(params, "soft_cap_notional");
            let hard_cap = numeric(params, "hard_cap_notional");
            let ema_period = numeric(params, "ema_period");
            let trend_block = numeric(params, "trend_block_bps");
            let minimum = numeric(params, "min_quote_notional");
            let feature_interval = numeric(params, "feature_interval_secs");
            let budget = numeric(params, "total_quote_budget");
            let vol_window = numeric(params, "vol_window");
            let vol_multiplier = numeric(params, "vol_multiplier");
            let max_skew = numeric(params, "max_skew_bps");
            let cash_open_guard = boolean(params, "cash_open_guard");
            let cash_before = numeric(params, "cash_open_guard_before_minutes");
            let cash_after = numeric(params, "cash_open_guard_after_minutes");
            let jump_breaker = numeric(params, "jump_circuit_breaker_bps");
            let min_book_spread = numeric(params, "min_book_spread_bps");
            let max_book_spread = numeric(params, "max_book_spread_bps");
            let wide_book_size_mult = numeric(params, "wide_book_size_mult");
            let max_bbo_imbalance = numeric(params, "max_bbo_imbalance");
            let breaker_cooldown = numeric(params, "circuit_breaker_cooldown_secs");
            let join_inside_ticks = numeric(params, "join_inside_ticks");
            let flatten_mid_secs = numeric(params, "flatten_mid_secs");
            let flatten_ioc_secs = numeric(params, "flatten_ioc_secs");
            if !matches!(spread, Some(v) if (1.0..=100.0).contains(&v)) {
                violations.push("maker spread_bps must be between 1 and 100".to_string());
            }
            if !matches!(quote, Some(v) if v > 0.0 && v <= notional_cap) {
                violations.push(format!(
                    "maker quote notional exceeds ${notional_cap:.2} cap"
                ));
            }
            if !matches!(requote, Some(v) if (0.0..=100.0).contains(&v)) {
                violations
                    .push("maker requote threshold must be between 0 and 100 bps".to_string());
            }
            if !matches!(cooldown, Some(v) if (1.0..=300.0).contains(&v)) {
                violations
                    .push("maker requote cooldown must be between 1 and 300 seconds".to_string());
            }
            if !matches!((soft_cap, hard_cap), (Some(soft), Some(hard))
                if soft > 0.0 && soft <= hard && hard <= notional_cap)
            {
                violations.push(format!(
                    "maker inventory caps must satisfy 0 < soft <= hard <= ${notional_cap:.2}"
                ));
            }
            if !matches!((minimum, quote), (Some(minimum), Some(quote))
                if minimum > 0.0 && minimum <= quote)
            {
                violations.push("maker minimum quote must not exceed quote notional".to_string());
            }
            if !matches!(budget, Some(v) if v > 0.0 && v <= notional_cap) {
                violations.push(format!(
                    "maker total quote budget exceeds ${notional_cap:.2} cap"
                ));
            }
            if !matches!(ema_period, Some(v) if (2.0..=500.0).contains(&v))
                || !matches!(trend_block, Some(v) if (0.0..=100.0).contains(&v))
                || !matches!(feature_interval, Some(v) if (1.0..=86_400.0).contains(&v))
                || !matches!(vol_window, Some(v) if (0.0..=10_000.0).contains(&v))
                || !matches!(vol_multiplier, Some(v) if (0.0..=5.0).contains(&v))
                || !matches!(max_skew, Some(v) if (0.0..=100.0).contains(&v))
            {
                violations.push("maker adaptive parameters are outside safe bounds".to_string());
            }
            if cash_open_guard != Some(true)
                || !matches!(cash_before, Some(v) if (0.0..=180.0).contains(&v))
                || !matches!(cash_after, Some(v) if (0.0..=180.0).contains(&v))
                || !matches!(jump_breaker, Some(v) if (1.0..=500.0).contains(&v))
                || !matches!(max_book_spread, Some(v) if (1.0..=500.0).contains(&v))
                || !matches!(breaker_cooldown, Some(v) if (1.0..=3_600.0).contains(&v))
            {
                violations
                    .push("maker market-protection parameters are outside safe bounds".to_string());
            }
            if let Some(min_spread) = min_book_spread {
                if !(0.0..500.0).contains(&min_spread) {
                    violations
                        .push("maker min_book_spread_bps must be between 0 and 500".to_string());
                } else if let Some(max_spread) = max_book_spread {
                    if min_spread > 0.0 && min_spread >= max_spread {
                        violations.push(
                            "maker min_book_spread_bps must be below max_book_spread_bps"
                                .to_string(),
                        );
                    }
                }
            }
            if let Some(mult) = wide_book_size_mult {
                if !(1.0..=4.0).contains(&mult) {
                    violations
                        .push("maker wide_book_size_mult must be between 1 and 4".to_string());
                }
            }
            if let Some(imbalance) = max_bbo_imbalance {
                if !(0.0..=50.0).contains(&imbalance) {
                    violations.push("maker max_bbo_imbalance must be between 0 and 50".to_string());
                }
            }
            if let Some(ticks) = join_inside_ticks {
                if !(0.0..=20.0).contains(&ticks) {
                    violations.push("maker join_inside_ticks must be between 0 and 20".to_string());
                }
            }
            if let (Some(mid), Some(ioc)) = (flatten_mid_secs, flatten_ioc_secs) {
                if mid < 0.0 || ioc < mid || ioc > 300.0 {
                    violations.push(
                        "maker flatten timers must satisfy 0 <= flatten_mid_secs <= flatten_ioc_secs <= 300"
                            .to_string(),
                    );
                }
            }
            if let Some(mode) = param_string(params, "quote_mode") {
                let mode = mode.trim().to_ascii_lowercase();
                if !mode.is_empty()
                    && !matches!(
                        mode.as_str(),
                        "mid_spread" | "spread" | "mid" | "join_best" | "join" | "bbo" | "inside"
                    )
                {
                    violations.push("maker quote_mode must be mid_spread or join_best".to_string());
                }
            }
        }
        _ => violations.push("strategy is not in the live allowlist".to_string()),
    }
    violations
}

pub fn evaluate_proposal(input: &ProposalInput, policy: &PolicySnapshot) -> PolicyDecision {
    let mut checks = vec![
        format!("evidence dataset: {}", input.evidence.data_file),
        format!("max drawdown <= {:.2}%", policy.max_drawdown_pct),
        format!("max notional <= {:.2}% equity", policy.max_notional_pct),
    ];
    let mut violations = Vec::new();

    if policy.trading_paused {
        checks.push("entry pause stays active while configuration is staged".to_string());
    }
    if policy.emergency_triggered {
        violations.push("risk emergency is active".to_string());
    }
    if policy.equity <= 0.0 || !policy.equity.is_finite() {
        violations.push("account equity is unavailable".to_string());
    }
    let e = &input.evidence;
    if e.data_file.trim().is_empty() {
        violations.push("backtest dataset is missing".to_string());
    }
    if !e.total_return_pct.is_finite() || e.total_return_pct <= 0.0 {
        violations.push("verified backtest return must be positive".to_string());
    }
    if !e.sharpe_ratio.is_finite() || e.sharpe_ratio <= 0.0 {
        violations.push("verified Sharpe ratio must be positive".to_string());
    }
    if !e.max_drawdown_pct.is_finite() || e.max_drawdown_pct.abs() > policy.max_drawdown_pct {
        violations.push(format!(
            "verified drawdown exceeds {:.2}%",
            policy.max_drawdown_pct
        ));
    }
    if !e.peak_notional_pct.is_finite()
        || e.peak_notional_pct < 0.0
        || e.peak_notional_pct > policy.max_notional_pct
    {
        violations.push(format!(
            "verified peak notional exceeds {:.2}%",
            policy.max_notional_pct
        ));
    }
    if e.total_trades < 8 {
        violations.push("at least 8 verified full-period trades are required".to_string());
    }
    if !e.validation_return_pct.is_finite() || e.validation_return_pct <= 0.0 {
        violations.push("out-of-sample return must be positive".to_string());
    }
    if !e.validation_sharpe_ratio.is_finite() || e.validation_sharpe_ratio <= 0.0 {
        violations.push("out-of-sample Sharpe ratio must be positive".to_string());
    }
    if !e.validation_max_drawdown_pct.is_finite()
        || e.validation_max_drawdown_pct.abs() > policy.max_drawdown_pct
    {
        violations.push(format!(
            "out-of-sample drawdown exceeds {:.2}%",
            policy.max_drawdown_pct
        ));
    }
    if !e.validation_peak_notional_pct.is_finite()
        || e.validation_peak_notional_pct < 0.0
        || e.validation_peak_notional_pct > policy.max_notional_pct
    {
        violations.push(format!(
            "out-of-sample peak notional exceeds {:.2}%",
            policy.max_notional_pct
        ));
    }
    if e.validation_total_trades < 3 {
        violations.push("at least 3 out-of-sample trades are required".to_string());
    }
    if matches!(input.strategy.as_str(), "maker_quote" | "maker") {
        if e.rolling_days < 5 || e.rolling_profitable_days * 2 <= e.rolling_days {
            violations.push(
                "maker evidence requires a profitable majority of rolling validation days"
                    .to_string(),
            );
        }
        if e.cash_open_trades == 0
            || !e.cash_open_return_pct.is_finite()
            || e.cash_open_return_pct <= 0.0
        {
            violations.push("maker cash-open window must be independently profitable".to_string());
        }
        if e.validation_market_count == 0
            || e.validation_profitable_market_count != e.validation_market_count
        {
            violations.push(
                "maker out-of-sample evidence must be profitable in every market".to_string(),
            );
        }
    }

    violations.extend(validate_strategy_params(
        &input.strategy,
        &input.params,
        policy,
    ));

    checks.push("model has proposal authority only".to_string());
    PolicyDecision {
        allowed: violations.is_empty(),
        checks,
        violations,
    }
}

impl AgentLedger {
    pub fn load(network: &str) -> Self {
        let Ok(path) = super::runtime_paths::data_file(network, AUDIT_FILE) else {
            return Self::default();
        };
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, network: &str) -> std::io::Result<()> {
        let path =
            super::runtime_paths::data_file(network, AUDIT_FILE).map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    pub fn record(&mut self, proposal: AgentProposal) {
        self.proposals.push(proposal);
        if self.proposals.len() > MAX_AUDIT_RECORDS {
            self.proposals
                .drain(..self.proposals.len() - MAX_AUDIT_RECORDS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn safe_input() -> ProposalInput {
        ProposalInput {
            strategy: "trend".into(),
            params: HashMap::from([
                ("fast_ma".into(), serde_json::json!(14)),
                ("slow_ma".into(), serde_json::json!(50)),
                ("notional".into(), serde_json::json!(20)),
            ]),
            evidence: BacktestEvidence {
                data_file: "BTC.csv".into(),
                start: "2026-01-01".into(),
                end: "2026-01-31".into(),
                capital: 100.0,
                total_return_pct: 2.0,
                sharpe_ratio: 1.2,
                max_drawdown_pct: 3.0,
                total_trades: 8,
                peak_notional_pct: 20.0,
                validation_return_pct: 1.0,
                validation_sharpe_ratio: 0.8,
                validation_max_drawdown_pct: 2.5,
                validation_total_trades: 6,
                validation_peak_notional_pct: 18.0,
                rolling_days: 10,
                rolling_profitable_days: 7,
                cash_open_return_pct: 0.1,
                cash_open_trades: 3,
                validation_market_count: 1,
                validation_profitable_market_count: 1,
            },
            rationale: "verified candidate".into(),
        }
    }

    fn policy() -> PolicySnapshot {
        PolicySnapshot {
            equity: 100.0,
            trading_paused: false,
            emergency_triggered: false,
            max_drawdown_pct: 10.0,
            max_notional_pct: 25.0,
        }
    }

    fn safe_maker_params() -> HashMap<String, serde_json::Value> {
        HashMap::from([
            ("spread_bps".into(), serde_json::json!(30)),
            ("per_quote_notional".into(), serde_json::json!(20)),
            ("requote_threshold_bps".into(), serde_json::json!(2)),
            ("requote_cooldown_secs".into(), serde_json::json!(5)),
            ("soft_cap_notional".into(), serde_json::json!(20)),
            ("hard_cap_notional".into(), serde_json::json!(24)),
            ("trend_filter".into(), serde_json::json!(true)),
            ("ema_period".into(), serde_json::json!(20)),
            ("trend_block_bps".into(), serde_json::json!(6)),
            ("min_quote_notional".into(), serde_json::json!(5)),
            ("feature_interval_secs".into(), serde_json::json!(60)),
            ("total_quote_budget".into(), serde_json::json!(24)),
            ("vol_window".into(), serde_json::json!(24)),
            ("vol_multiplier".into(), serde_json::json!(0.5)),
            ("max_skew_bps".into(), serde_json::json!(3)),
            ("cash_open_guard".into(), serde_json::json!(true)),
            (
                "cash_open_guard_before_minutes".into(),
                serde_json::json!(5),
            ),
            (
                "cash_open_guard_after_minutes".into(),
                serde_json::json!(20),
            ),
            ("jump_circuit_breaker_bps".into(), serde_json::json!(20)),
            ("min_book_spread_bps".into(), serde_json::json!(8)),
            ("max_book_spread_bps".into(), serde_json::json!(40)),
            ("wide_book_size_mult".into(), serde_json::json!(2)),
            ("max_bbo_imbalance".into(), serde_json::json!(6)),
            ("flatten_only".into(), serde_json::json!(false)),
            ("join_inside_ticks".into(), serde_json::json!(2)),
            ("flatten_mid_secs".into(), serde_json::json!(6)),
            ("flatten_ioc_secs".into(), serde_json::json!(15)),
            (
                "circuit_breaker_cooldown_secs".into(),
                serde_json::json!(60),
            ),
            ("quote_mode".into(), serde_json::json!("join_best")),
        ])
    }

    #[test]
    fn allows_verified_bounded_proposal() {
        assert!(evaluate_proposal(&safe_input(), &policy()).allowed);
    }

    #[test]
    fn allows_verified_candidate_while_entries_are_paused() {
        let mut p = policy();
        p.trading_paused = true;
        assert!(evaluate_proposal(&safe_input(), &p).allowed);
    }

    #[test]
    fn rejects_candidate_without_profitable_out_of_sample_evidence() {
        let mut input = safe_input();
        input.evidence.validation_return_pct = -0.1;
        let decision = evaluate_proposal(&input, &policy());
        assert!(!decision.allowed);
        assert!(decision
            .violations
            .iter()
            .any(|violation| violation.contains("out-of-sample")));
    }

    #[test]
    fn allows_bounded_verified_maker_quote() {
        let mut input = safe_input();
        input.strategy = "maker_quote".into();
        input.params = safe_maker_params();
        assert!(evaluate_proposal(&input, &policy()).allowed);
    }

    #[test]
    fn join_best_string_params_pass_runtime_resume_policy() {
        let mut params = safe_maker_params();
        for (key, value) in params.clone() {
            params.insert(
                key,
                serde_json::Value::String(value.to_string().trim_matches('"').to_string()),
            );
        }
        params.insert("quote_mode".into(), serde_json::json!("join_best"));
        params.insert("requote_threshold_bps".into(), serde_json::json!("0"));
        params.insert("requote_cooldown_secs".into(), serde_json::json!("1"));
        params.insert("max_book_spread_bps".into(), serde_json::json!("200"));
        params.insert("min_book_spread_bps".into(), serde_json::json!("8"));
        params.insert("wide_book_size_mult".into(), serde_json::json!("2"));
        let violations = validate_strategy_params("maker_quote", &params, &policy());
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn rejects_min_book_spread_at_or_above_max() {
        let mut params = safe_maker_params();
        params.insert("min_book_spread_bps".into(), serde_json::json!(80));
        params.insert("max_book_spread_bps".into(), serde_json::json!(40));
        let violations = validate_strategy_params("maker_quote", &params, &policy());
        assert!(
            violations.iter().any(|v| v.contains("min_book_spread")),
            "{violations:?}"
        );
    }

    #[test]
    fn rejects_unknown_quote_mode() {
        let mut params = safe_maker_params();
        params.insert("quote_mode".into(), serde_json::json!("cross_spread"));
        let violations = validate_strategy_params("maker_quote", &params, &policy());
        assert!(violations.iter().any(|v| v.contains("quote_mode")));
    }

    #[test]
    fn rejects_maker_without_majority_profitable_rolling_days() {
        let mut input = safe_input();
        input.strategy = "maker_quote".into();
        input.params = safe_maker_params();
        input.evidence.rolling_days = 10;
        input.evidence.rolling_profitable_days = 5;
        let decision = evaluate_proposal(&input, &policy());
        assert!(!decision.allowed);
        assert!(decision
            .violations
            .iter()
            .any(|violation| violation.contains("rolling")));
    }

    #[test]
    fn rejects_maker_with_negative_cash_open_result() {
        let mut input = safe_input();
        input.strategy = "maker_quote".into();
        input.params = safe_maker_params();
        input.evidence.cash_open_return_pct = -0.01;
        let decision = evaluate_proposal(&input, &policy());
        assert!(!decision.allowed);
        assert!(decision
            .violations
            .iter()
            .any(|violation| violation.contains("cash-open")));
    }

    #[test]
    fn rejects_maker_when_any_validation_market_is_not_profitable() {
        let mut input = safe_input();
        input.strategy = "maker_quote".into();
        input.params = safe_maker_params();
        input.evidence.validation_market_count = 10;
        input.evidence.validation_profitable_market_count = 9;
        let decision = evaluate_proposal(&input, &policy());
        assert!(!decision.allowed);
        assert!(decision
            .violations
            .iter()
            .any(|violation| violation.contains("every market")));
    }

    #[test]
    fn rejects_active_maker_caps_above_runtime_equity_limit() {
        let mut input = safe_input();
        input.strategy = "maker_quote".into();
        input.params = HashMap::from([
            ("spread_bps".into(), serde_json::json!(30)),
            ("per_quote_notional".into(), serde_json::json!(20)),
            ("requote_threshold_bps".into(), serde_json::json!(2)),
            ("requote_cooldown_secs".into(), serde_json::json!(5)),
            ("soft_cap_notional".into(), serde_json::json!(60)),
            ("hard_cap_notional".into(), serde_json::json!(100)),
            ("ema_period".into(), serde_json::json!(20)),
            ("trend_block_bps".into(), serde_json::json!(6)),
            ("min_quote_notional".into(), serde_json::json!(5)),
            ("feature_interval_secs".into(), serde_json::json!(60)),
            ("total_quote_budget".into(), serde_json::json!(100)),
            ("vol_window".into(), serde_json::json!(24)),
            ("vol_multiplier".into(), serde_json::json!(0.5)),
            ("max_skew_bps".into(), serde_json::json!(3)),
        ]);
        let violations = validate_strategy_params(&input.strategy, &input.params, &policy());
        assert!(violations
            .iter()
            .any(|violation| violation.contains("inventory caps")));
        assert!(violations
            .iter()
            .any(|violation| violation.contains("total quote budget")));
    }

    #[test]
    fn rejects_model_requested_notional_above_hard_cap() {
        let mut input = safe_input();
        input
            .params
            .insert("notional".into(), serde_json::json!(26));
        let decision = evaluate_proposal(&input, &policy());
        assert!(!decision.allowed);
        assert!(decision.violations.iter().any(|v| v.contains("cap")));
    }

    #[test]
    fn rejects_when_risk_emergency_is_active() {
        let mut p = policy();
        p.emergency_triggered = true;
        assert!(!evaluate_proposal(&safe_input(), &p).allowed);
    }
}
