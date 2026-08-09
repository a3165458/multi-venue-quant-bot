#[path = "../src/arcus.rs"]
mod arcus;

use arcus::{ArcusEnvironment, OrderSide, PlaceOrder};

#[test]
fn environments_use_the_official_arcus_endpoints() {
    assert_eq!(ArcusEnvironment::Mainnet.rest_url(), "https://api.arcus.xyz");
    assert_eq!(
        ArcusEnvironment::Testnet.websocket_url(),
        "wss://api.testnet.arcus.xyz/v1/ws"
    );
}

#[test]
fn place_order_builds_the_arcus_typed_canonical_payload() {
    let order = PlaceOrder {
        address: "0xAABBCCDDEEFF0011223344556677889900AABBCC".into(),
        account_index: 2,
        market_id: 7,
        side: OrderSide::Sell,
        price_ticks: 12345,
        quantity_quantums: 67,
        good_til_time_ns: 4_102_444_800_000_000_000,
        time_in_force: arcus::TimeInForce::Ioc,
        reduce_only: true,
        client_id: Some("strategy-42".into()),
    };

    assert_eq!(
        order.canonical_payload(1_712_345_678_000_000_000).unwrap(),
        r#"{"ad":"0xaabbccddeeff0011223344556677889900aabbcc","ai":2,"c":"strategy-42","ct":1712345678000000000,"g":4102444800000000000,"m":7,"op":1,"p":12345,"q":67,"r":1,"s":1,"t":2,"v":1}"#
    );
}

#[test]
fn place_order_rejects_invalid_signing_inputs() {
    let mut order = PlaceOrder {
        address: "0xAABBCCDDEEFF0011223344556677889900AABBCC".into(),
        account_index: 0,
        market_id: 1,
        side: OrderSide::Buy,
        price_ticks: 1,
        quantity_quantums: 1,
        good_til_time_ns: 4_102_444_800_000_000_000,
        time_in_force: arcus::TimeInForce::Gtt,
        reduce_only: false,
        client_id: Some("contains space".into()),
    };

    assert!(order.canonical_payload(1_712_345_678_000_000_000).is_err());
    order.client_id = None;
    order.account_index = 10;
    assert!(order.canonical_payload(1_712_345_678_000_000_000).is_err());
}
