const DASHBOARD_HTML: &str = include_str!("ui/index.html");
const DASHBOARD_APP_JS: &str = include_str!("ui/app.js");
const DASHBOARD_AI_JS: &str = include_str!("ui/ai.js");

fn css_rule(selector: &str) -> &str {
    let selector_start = DASHBOARD_HTML
        .find(selector)
        .unwrap_or_else(|| panic!("missing CSS selector: {selector}"));
    let rule_start = DASHBOARD_HTML[selector_start..]
        .find('{')
        .map(|offset| selector_start + offset + 1)
        .expect("CSS rule must have an opening brace");
    let rule_end = DASHBOARD_HTML[rule_start..]
        .find('}')
        .map(|offset| rule_start + offset)
        .expect("CSS rule must have a closing brace");

    &DASHBOARD_HTML[rule_start..rule_end]
}

#[test]
fn event_stream_fills_its_desktop_card_and_stays_bounded_on_narrow_screens() {
    let event_title = DASHBOARD_HTML
        .find(r#"data-i18n="eventLog""#)
        .expect("event stream title must exist");
    let event_card = DASHBOARD_HTML[..event_title]
        .rfind("\n      <div class=\"sec")
        .map(|start| &DASHBOARD_HTML[start + 7..event_title])
        .expect("event stream card must exist");
    assert!(
        event_card.starts_with(r#"<div class="sec event-stream-section">"#),
        "the card containing the event stream needs a dedicated layout class"
    );

    let section_rule = css_rule(".event-stream-section {");
    assert!(section_rule.contains("display:flex"));
    assert!(section_rule.contains("flex-direction:column"));
    assert!(section_rule.contains("min-height:0"));

    let body_rule = css_rule(".event-stream-section > .sec-body {");
    assert!(body_rule.contains("flex:1"));
    assert!(body_rule.contains("min-height:270px"));

    let rail_rule = css_rule(".event-stream-section .rail {");
    assert!(rail_rule.contains("position:absolute"));
    assert!(rail_rule.contains("inset:16px"));
    assert!(rail_rule.contains("max-height:none"));
    assert!(rail_rule.contains("min-height:0"));

    let chip_rule = css_rule(".event-stream-section .rail-chip {");
    assert!(chip_rule.contains("flex:1 0 auto"));
    assert!(chip_rule.contains("min-height:38px"));
    assert!(chip_rule.contains("background:var(--bg-card)"));

    assert!(
        DASHBOARD_HTML.contains(
            ".event-stream-section .rail { position:static; max-height:360px; min-height:0; }"
        ),
        "stacked layouts must cap a long event stream"
    );
}

#[test]
fn event_stream_renders_server_persisted_history_instead_of_page_local_deltas() {
    assert!(DASHBOARD_HTML.contains("function renderEvents(events)"));
    assert!(DASHBOARD_HTML.contains("msg.type === 'events'"));
    assert!(DASHBOARD_HTML.contains("fetch('/api/events')"));
    assert!(!DASHBOARD_HTML.contains("rail('risk'"));
    assert!(!DASHBOARD_HTML.contains("rail('order'"));
    assert!(!DASHBOARD_HTML.contains("rail('fill'"));
}

#[test]
fn control_loop_lights_only_real_actions_at_their_orbit_contact_time() {
    assert!(
        DASHBOARD_HTML.contains(r#"id="loop-path" d="M 450,24"#),
        "the orbit must begin at PUSH so a state frame and the dot share one clock"
    );
    assert!(DASHBOARD_HTML.contains(
        r#"<animateMotion id="loop-motion" dur="3s" begin="indefinite" repeatCount="indefinite">"#
    ));

    for (node, delay) in [("risk", 750), ("order", 1500), ("fill", 2250)] {
        assert!(
            DASHBOARD_HTML.contains(&format!("scheduleLoopContact('{node}', {delay})")),
            "{node} must be checked when the dot reaches it"
        );
    }

    assert!(DASHBOARD_HTML.contains("queueLoopAction('risk')"));
    assert!(DASHBOARD_HTML.contains("queueLoopAction('order')"));
    assert!(DASHBOARD_HTML.contains("queueLoopAction('fill')"));
    assert!(DASHBOARD_HTML.contains("restartLoopMotion()"));
}

#[test]
fn dashboard_escapes_exchange_data_before_using_inner_html() {
    assert!(DASHBOARD_APP_JS.contains("const escapeHtml = value =>"));
    assert!(DASHBOARD_APP_JS.contains("replace(/[&<>\"']/g"));

    for escaped_value in [
        "escapeHtml(n.message)",
        "escapeHtml(p.symbol)",
        "escapeHtml(p.side)",
        "escapeHtml(p.size)",
        "escapeHtml(String(o.id).slice(-6))",
        "escapeHtml(o.symbol || 'BTC')",
        "escapeHtml(o.side)",
        "escapeHtml(total)",
        "escapeHtml(fill)",
        "escapeHtml(o.status || 'Open')",
        "escapeHtml(t.symbol || t.market)",
        "escapeHtml(t.side)",
        "escapeHtml(action)",
        "escapeHtml(asset)",
        "escapeHtml(shortDate)",
        "escapeHtml(l.msg)",
    ] {
        assert!(
            DASHBOARD_APP_JS.contains(escaped_value),
            "server-derived value must be HTML-escaped: {escaped_value}"
        );
    }

    for unsafe_interpolation in [
        ">${n.message}<",
        ">${p.symbol}<",
        ">${p.side}<",
        ">${o.side}<",
        ">${total}<",
        ">${fill}<",
        ">${action}<",
        ">${t.side}<",
        ">${asset}<",
        ">${l.msg}<",
    ] {
        assert!(
            !DASHBOARD_APP_JS.contains(unsafe_interpolation),
            "raw server-derived HTML interpolation remains: {unsafe_interpolation}"
        );
    }
}

#[test]
fn dynamic_market_order_and_fill_strings_are_escaped_before_inner_html_rendering() {
    assert!(
        DASHBOARD_HTML.contains("var safeSym = esc(sym);")
            && DASHBOARD_HTML.contains("var safeSide = esc(pos && pos.side || '');"),
        "the market tape must escape server-provided symbols and position sides"
    );

    for escaped_fill_field in [
        "esc(t.side || '')",
        "esc(t.symbol || t.market || '')",
        "esc(t.quantity || '')",
    ] {
        assert!(
            DASHBOARD_HTML.contains(escaped_fill_field),
            "fill toasts must escape {escaped_fill_field} before assigning innerHTML"
        );
    }

    assert!(
        DASHBOARD_HTML.contains("var safeOrderSymbol = esc(o.symbol || '—');")
            && DASHBOARD_HTML.contains("var safeOrderSide = esc(side);"),
        "the order swarm must escape server-provided symbols and sides"
    );

    for unsafe_fragment in [
        "'<span class=\"tape-sym\">' + sym + '</span>'",
        "'<div class=\"toast-main\">' + (t.side || '')",
        "'<span class=\"swarm-name\">' + (o.symbol || '—')",
    ] {
        assert!(
            !DASHBOARD_HTML.contains(unsafe_fragment),
            "raw server data must not reach innerHTML: {unsafe_fragment}"
        );
    }
}

#[test]
fn ai_api_key_is_never_persisted_in_browser_storage() {
    assert!(
        DASHBOARD_AI_JS.contains("delete s.key"),
        "older stored keys must be scrubbed"
    );
    assert!(
        !DASHBOARD_AI_JS.contains("key: document.getElementById('ai-key').value"),
        "the API key must not be serialized into localStorage"
    );
    assert!(
        !DASHBOARD_AI_JS.contains("document.getElementById('ai-key').value = s.key"),
        "the API key must not be restored from localStorage"
    );
    assert!(
        !DASHBOARD_AI_JS.contains("'ai-model','ai-key','ai-goal'"),
        "API-key input events must not trigger settings persistence"
    );
}

#[test]
fn ai_backtest_html_escapes_dynamic_text() {
    assert!(DASHBOARD_AI_JS.contains("escapeHtml(msg)"));
    assert!(DASHBOARD_AI_JS.contains("escapeHtml(data.strategy || 'grid')"));
    assert!(DASHBOARD_AI_JS.contains("escapeHtml(data.data_file || '-')"));
    assert!(DASHBOARD_AI_JS.contains("escapeHtml(currentParams)"));
}
