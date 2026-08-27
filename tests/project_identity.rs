use std::fs;

const PRODUCT_NAME: &str = "Multi-Venue Quant Bot";
const BINARY_NAME: &str = "multi-venue-quant-bot";

#[test]
fn rust_package_and_user_facing_brand_are_not_lighter_bot() {
    let cargo = fs::read_to_string("Cargo.toml").unwrap();
    let main = fs::read_to_string("src/main.rs").unwrap();
    assert!(cargo.contains(r#"name = "multi-venue-quant-bot""#));
    assert!(main.contains(PRODUCT_NAME));
    assert!(!main.contains("Starting Lighter Trading Bot"));
}

#[test]
fn deployment_surfaces_use_the_new_binary_and_service_name() {
    for path in [
        "Dockerfile",
        "docker-compose.yml",
        "ecosystem.config.js",
        "ecosystem.dashboard.config.js",
        "start.sh",
        "scripts/periodic_backtest.sh",
    ] {
        let contents = fs::read_to_string(path).unwrap();
        assert!(
            contents.contains(BINARY_NAME),
            "{path} does not reference {BINARY_NAME}"
        );
        assert!(
            !contents.contains("/root/lighter-quant-bot"),
            "{path} still points at the legacy checkout"
        );
    }
}

#[test]
fn new_live_launcher_is_primary_and_old_launcher_is_only_a_wrapper() {
    let primary = fs::read_to_string("scripts/run_venue_live.sh").unwrap();
    let legacy = fs::read_to_string("scripts/run_live.sh").unwrap();
    assert!(primary.contains(BINARY_NAME));
    assert!(primary.contains("arcus-mainnet"));
    assert!(primary.contains("aster-mainnet"));
    assert!(primary.contains("config/settings.aster.yaml"));
    assert!(legacy.contains("run_venue_live.sh"));
    assert!(legacy.contains("deprecated"));
    assert!(!legacy.contains("cargo build --release"));
}

#[test]
fn dashboard_brand_describes_the_multi_venue_product() {
    for path in ["src/dashboard/ui/index.html", "src/dashboard/server.rs"] {
        let contents = fs::read_to_string(path).unwrap();
        assert!(
            contents.contains(PRODUCT_NAME),
            "{path} is missing the product brand"
        );
    }
}

#[test]
fn dashboard_uses_the_dedicated_4028_port_everywhere() {
    for path in [
        "src/main.rs",
        "config/settings.yaml",
        "config/settings.robinhood.yaml",
        "config/settings.arcus.yaml",
        "config/settings.arcus-testnet.yaml",
        "config/settings.aster.yaml",
        "Dockerfile",
        "docker-compose.yml",
        "scripts/track_pnl.sh",
        "README.md",
    ] {
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("4028"), "{path} does not use port 4028");
        assert!(
            !contents.contains("3028"),
            "{path} still references the conflicting port 3028"
        );
    }
}

#[test]
fn aster_profile_starts_live_and_is_conservatively_bounded() {
    let settings = config::Config::builder()
        .add_source(config::File::with_name("config/settings.aster.yaml"))
        .build()
        .unwrap();
    assert_eq!(settings.get_string("exchange.kind").unwrap(), "aster");
    assert_eq!(
        settings.get_string("exchange.environment").unwrap(),
        "mainnet"
    );
    assert!(!settings.get_bool("trading.start_paused").unwrap());
    assert!(settings
        .get_bool("trading.require_isolated_margin")
        .unwrap());
    assert_eq!(
        settings.get_array("trading.symbols").unwrap()[0]
            .clone()
            .into_string()
            .unwrap(),
        "BTCUSDT"
    );
    assert_eq!(settings.get_int("trading.max_open_orders").unwrap(), 2);
    assert!(settings
        .get_bool("trading.strategies.maker_quote.enabled")
        .unwrap());
    assert!(settings.get_bool("trading.shadow_maker.enabled").unwrap());
    assert!(settings.get_bool("trading.hft_shadow.enabled").unwrap());
    assert!(
        settings
            .get_float("trading.hft_shadow.toxicity_1s_bps")
            .unwrap()
            < 0.0
    );
    assert!(
        settings
            .get_int("trading.hft_shadow.toxicity_min_samples")
            .unwrap()
            > 0
    );
    assert!(!settings
        .get_bool("trading.strategies.grid_trading.enabled")
        .unwrap());
    assert!(!settings
        .get_bool("trading.strategies.trend_following.enabled")
        .unwrap());
    assert_eq!(
        settings
            .get_float("trading.strategies.maker_quote.per_quote_notional")
            .unwrap(),
        10.0
    );
    assert_eq!(
        settings
            .get_float("trading.strategies.maker_quote.total_quote_budget")
            .unwrap(),
        20.0
    );
    assert!(settings
        .get_bool("trading.strategies.maker_quote.cash_open_guard")
        .unwrap());
    assert_eq!(
        settings
            .get_float("risk.position_limit.max_leverage")
            .unwrap(),
        2.0
    );
    assert_eq!(
        settings
            .get_float("risk.position_limit.max_position_size")
            .unwrap(),
        50.0
    );
    assert_eq!(settings.get_int("dashboard.port").unwrap(), 4028);
}

#[test]
fn both_live_exchange_paths_restore_and_apply_dashboard_strategy_updates() {
    let main = fs::read_to_string("src/main.rs").unwrap();
    let arcus_live = main
        .split("async fn run_arcus_live_trading")
        .nth(1)
        .expect("Arcus live path exists");
    assert_eq!(
        arcus_live
            .matches("apply_pending_strategy_update(&dash_state, &strategy)")
            .count(),
        2,
        "Arcus must apply saved settings at startup and dashboard changes while live"
    );
}
