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
    assert!(legacy.contains("run_venue_live.sh"));
    assert!(legacy.contains("deprecated"));
    assert!(!legacy.contains("cargo build --release"));
}

#[test]
fn dashboard_brand_describes_the_multi_venue_product() {
    for path in [
        "src/dashboard/ui/index.html",
        "src/dashboard/ui/ai.html",
        "src/dashboard/server.rs",
    ] {
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
