const DASHBOARD_HTML: &str = include_str!("ui/index.html");
const DASHBOARD_APP_JS: &str = include_str!("ui/app.js");

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
fn dashboard_exposes_venue_and_maker_quote_controls() {
    assert!(
        DASHBOARD_HTML.contains(r#"id="hero-venue""#),
        "hero must show the live venue"
    );
    assert!(
        DASHBOARD_HTML.contains(r#"id="maker-card""#),
        "strategies page must include maker quote controls"
    );
    assert!(
        DASHBOARD_HTML.contains(r#"id="mq-fields""#),
        "maker param grid container must exist"
    );
    assert!(
        DASHBOARD_APP_JS.contains("const MQ_PARAMS"),
        "maker param spec must drive the generated form"
    );
    assert!(
        DASHBOARD_APP_JS.contains("'flatten_only'"),
        "maker flatten-only toggle must be part of the generated form"
    );
    assert!(
        DASHBOARD_APP_JS.contains("quant_trades_"),
        "CSV export must not use the Lighter-only filename"
    );
    assert!(
        DASHBOARD_APP_JS.contains("|| 'cn'"),
        "dashboard language must default to Chinese"
    );
    assert!(
        DASHBOARD_APP_JS.contains("mq_quote_mode"),
        "maker labels must be i18n keys"
    );
    assert!(
        DASHBOARD_APP_JS.contains("中间价铺开"),
        "maker quote mode must have a Chinese label"
    );
    assert!(
        DASHBOARD_APP_JS.contains("function updateConnectionStatus"),
        "header pill must track WebSocket state instead of staying on connecting"
    );
    assert!(
        DASHBOARD_APP_JS.contains("t('liveTrading')"),
        "connected status must use the i18n liveTrading key, not hardcoded English"
    );
    let connecting_attr = format!("data-i18n={q}connecting{q}", q = '"');
    assert_eq!(
        DASHBOARD_HTML.find(&connecting_attr),
        None,
        "connection pill must not bind the connecting i18n key"
    );
    assert!(
        DASHBOARD_HTML.contains("status-label"),
        "header must keep the connection status pill"
    );
    let dict_start = DASHBOARD_APP_JS
        .find("const i18nStrings")
        .expect("i18n dictionary must exist");
    let dict_end = DASHBOARD_APP_JS
        .find("currentLang = localStorage")
        .expect("currentLang init must follow the i18n dictionary");
    let self_call = format!("t({q}", q = '\'');
    assert_eq!(
        DASHBOARD_APP_JS[dict_start..dict_end].find(&self_call),
        None,
        "i18n dictionary values must be literals; calling t() inside the dictionary \
         is a TDZ ReferenceError that kills the whole dashboard script"
    );
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
    // Full-bleed log rows: padding lives on each chip, not on the rail inset.
    assert!(body_rule.contains("padding:0"));

    let rail_rule = css_rule(".event-stream-section .rail {");
    assert!(rail_rule.contains("position:absolute"));
    assert!(rail_rule.contains("inset:0"));
    assert!(rail_rule.contains("max-height:none"));

    // Four-column log rows (time / kind / detail / age) fill the card width.
    let chip_rule = css_rule(".rail-chip {");
    assert!(chip_rule.contains("display:grid"));
    assert!(chip_rule.contains("grid-template-columns:auto auto 1fr auto"));
    assert!(chip_rule.contains("padding:8px 16px"));

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
fn settings_exposes_restart_safe_network_selection() {
    for id in [
        "network-lighter-mainnet",
        "network-lighter-robinhood",
        "network-arcus-mainnet",
        "network-arcus-testnet",
        "network-aster-mainnet",
        "network-hyperliquid-mainnet",
        "network-hyperliquid-testnet",
        "network-rest-url",
        "network-ws-url",
        "network-chain-id",
        "network-fee-tier",
        "network-cross-dex",
        "network-strategy-source",
        "btn-save-network",
        "network-msg",
    ] {
        assert!(
            DASHBOARD_HTML.contains(&format!(r#"id="{id}""#)),
            "missing network setting control: {id}"
        );
    }
    assert!(DASHBOARD_HTML.contains("/api/network"));
    assert!(DASHBOARD_HTML.contains("requires_restart"));
    assert!(
        DASHBOARD_HTML.contains("credentials") || DASHBOARD_HTML.contains("凭据"),
        "network settings must warn that network credentials are isolated"
    );
}

#[test]
fn pnl_stats_pages_render_hyperliquid_fill_pnl_and_daily_history() {
    // Daily pnl bars must render independently of Chart.js init order:
    // renderPnlHistory has to run before the revenueChart early-return,
    // otherwise the panel stays on the loading placeholder forever.
    let render_idx = DASHBOARD_APP_JS
        .find("renderPnlHistory(pnlMap);")
        .expect("updateRevenueChart must render daily pnl bars");
    let guard_idx = DASHBOARD_APP_JS
        .find("if (!revenueChart) return;")
        .expect("revenueChart guard must exist");
    assert!(
        render_idx < guard_idx,
        "renderPnlHistory must be called before the revenueChart guard"
    );

    // Hyperliquid fills all carry action="Fill" with net realized pnl; the
    // history/stat pages must treat pnl-bearing fills as closed-trade rows.
    assert!(DASHBOARD_APP_JS.contains("const hasTradePnl"));
    assert!(DASHBOARD_APP_JS.contains("hasTradePnl(t)"));

    // Stats panels refresh periodically instead of only on WS reconnect,
    // and the refresh must not refetch /api/strategy (form stomping).
    assert!(DASHBOARD_APP_JS.contains("function refreshPnlStats"));
    assert!(DASHBOARD_APP_JS.contains("setInterval(refreshPnlStats, 60000);"));

    // Empty-state strings go through i18n, not hardcoded English.
    assert!(!DASHBOARD_APP_JS.contains("No closed positions yet"));
    assert!(!DASHBOARD_APP_JS.contains(">No daily data yet<"));
}

#[test]
fn dashboard_exposes_aster_shadow_maker_metrics() {
    for id in [
        "shadow-maker-section",
        "shadow-state",
        "shadow-lag",
        "shadow-depth-lag",
        "shadow-eval",
        "shadow-queue",
        "shadow-requests",
        "shadow-amend-savings",
        "shadow-fills",
        "shadow-volume",
        "shadow-markout",
        "shadow-depth-misses",
    ] {
        assert!(
            DASHBOARD_HTML.contains(&format!(r#"id="{id}""#)),
            "missing shadow-maker metric: {id}"
        );
    }
    assert!(DASHBOARD_HTML.contains("fetch('/api/shadow')"));
    assert!(DASHBOARD_HTML.contains("SHADOW ACTIVE"));
    assert!(DASHBOARD_HTML.contains("RISK EXITS REMAIN ARMED"));
    assert!(
        DASHBOARD_HTML.contains(r#"id="shadow-maker-section" hidden"#),
        "Aster shadow maker must start hidden; live Hyperliquid must not show it"
    );
    assert!(
        DASHBOARD_HTML.contains(r#"id="hft-shadow-section" hidden"#),
        "HFT shadow lab must start hidden; live Hyperliquid must not show it"
    );
    assert!(
        DASHBOARD_HTML.contains("function showAsterOnlyPanels"),
        "shadow panels must be gated to the live Aster venue"
    );
    assert!(
        DASHBOARD_HTML.contains("indexOf('aster-') === 0"),
        "shadow panels must key off the aster- venue prefix"
    );
    for id in [
        "hft-shadow-section",
        "hft-shadow-state",
        "hft-shadow-body",
        "hft-shadow-recommend",
    ] {
        assert!(DASHBOARD_HTML.contains(&format!(r#"id="{id}""#)));
    }
    assert!(DASHBOARD_HTML.contains("fetch('/api/hft-shadow')"));
    assert!(DASHBOARD_HTML.contains("NO REAL ORDERS"));
    assert!(DASHBOARD_HTML.contains("recommended_profile"));
    assert!(DASHBOARD_HTML.contains("TOXIC"));
}

#[test]
fn credential_editor_is_venue_aware_and_keeps_all_secrets_write_only() {
    for group in [
        "credentials-lighter",
        "credentials-arcus",
        "credentials-aster",
        "credentials-hyperliquid",
    ] {
        assert!(
            DASHBOARD_HTML.contains(&format!(r#"id="{group}""#)),
            "missing venue credential group: {group}"
        );
    }
    for secret in [
        "LIGHTER_SECRET_KEY",
        "ARCUS_SIGNING_KEY",
        "ASTER_SIGNER_PRIVATE_KEY",
        "HYPERLIQUID_SIGNER_PRIVATE_KEY",
    ] {
        assert!(
            DASHBOARD_HTML.contains(&format!(r#"data-secret-key="{secret}""#)),
            "missing write-only secret control: {secret}"
        );
    }
    assert!(DASHBOARD_HTML.contains("ARCUS_API_KEY"));
    assert!(DASHBOARD_HTML.contains("ASTER_SIGNER_ADDRESS"));
    assert!(DASHBOARD_HTML.contains("HYPERLIQUID_ACCOUNT_ADDRESS"));
    assert!(DASHBOARD_HTML.contains("write-only"));
    assert!(DASHBOARD_HTML.contains("leaving this blank"));
}
