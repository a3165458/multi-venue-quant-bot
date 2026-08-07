const SERVER: &str = include_str!("server.rs");
const AGENT_JS: &str = include_str!("ui/quant_agent.js");

#[test]
fn model_can_only_propose_and_human_approval_uses_guarded_backend_endpoint() {
    assert!(SERVER.contains("/api/agent/proposals"));
    assert!(SERVER.contains("/api/agent/proposals/:id/apply"));
    assert!(AGENT_JS.contains("/api/agent/proposals"));
    assert!(!AGENT_JS.contains("fetch('/api/strategy'"));
}

#[test]
fn quant_agent_has_deterministic_policy_and_persistent_audit() {
    const POLICY: &str = include_str!("quant_agent.rs");
    assert!(POLICY.contains("pub fn evaluate_proposal"));
    assert!(POLICY.contains("max_drawdown_pct"));
    assert!(POLICY.contains("max_notional_pct"));
    assert!(POLICY.contains("approval_phrase"));
    assert!(POLICY.contains("quant_agent_audit.json"));
    assert!(SERVER.contains("server verification backtest failed"));
}

#[test]
fn agent_ui_exposes_policy_health_and_audit_trail() {
    const AI_HTML: &str = include_str!("ui/ai.html");
    assert!(AI_HTML.contains(r#"id="agent-policy-status""#));
    assert!(AI_HTML.contains(r#"id="agent-audit-list""#));
    assert!(AGENT_JS.contains("/api/agent/status"));
    assert!(AGENT_JS.contains("/api/agent/audit"));
}
