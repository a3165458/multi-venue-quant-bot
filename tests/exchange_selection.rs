use lighter_bot::exchange::{ExchangeKind, LiveVenue};
use std::str::FromStr;

#[test]
fn every_live_venue_has_an_unambiguous_exchange_and_config() {
    let cases = [
        ("lighter-mainnet", ExchangeKind::Lighter, "config/settings.yaml"),
        (
            "lighter-robinhood",
            ExchangeKind::Lighter,
            "config/settings.robinhood.yaml",
        ),
        (
            "arcus-mainnet",
            ExchangeKind::Arcus,
            "config/settings.arcus.yaml",
        ),
        (
            "arcus-testnet",
            ExchangeKind::Arcus,
            "config/settings.arcus-testnet.yaml",
        ),
    ];

    for (name, exchange, config) in cases {
        let venue = LiveVenue::from_str(name).unwrap();
        assert_eq!(venue.exchange(), exchange);
        assert_eq!(venue.config_path(), config);
        assert_eq!(venue.as_str(), name);
    }
}

#[test]
fn legacy_network_names_remain_backward_compatible() {
    assert_eq!(
        LiveVenue::from_str("mainnet").unwrap(),
        LiveVenue::LighterMainnet
    );
    assert_eq!(
        LiveVenue::from_str("robinhood").unwrap(),
        LiveVenue::LighterRobinhood
    );
}

#[test]
fn invalid_venue_values_fail_closed() {
    assert!(LiveVenue::from_str("arcus").is_err());
    assert!(LiveVenue::from_str("../arcus-mainnet").is_err());
    assert!(LiveVenue::from_str("").is_err());
}

#[test]
fn venue_credentials_are_isolated() {
    assert_eq!(
        LiveVenue::ArcusMainnet.credential_key("SECRET_KEY"),
        "ARCUS_MAINNET_SECRET_KEY"
    );
    assert_eq!(
        LiveVenue::ArcusTestnet.credential_key("ADDRESS"),
        "ARCUS_TESTNET_ADDRESS"
    );
    assert_eq!(
        LiveVenue::LighterRobinhood.credential_key("ACCOUNT_INDEX"),
        "LIGHTER_ROBINHOOD_ACCOUNT_INDEX"
    );
}

#[test]
fn dashboard_exposes_both_exchanges_as_live_venues() {
    let html = std::fs::read_to_string("src/dashboard/ui/index.html").unwrap();
    let server = std::fs::read_to_string("src/dashboard/server.rs").unwrap();
    for venue in [
        "lighter-mainnet",
        "lighter-robinhood",
        "arcus-mainnet",
        "arcus-testnet",
    ] {
        assert!(html.contains(venue), "dashboard is missing {venue}");
        assert!(server.contains(venue), "network API is missing {venue}");
    }
}
