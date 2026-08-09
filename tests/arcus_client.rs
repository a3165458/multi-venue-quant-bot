use multi_venue_quant_bot::arcus;
use multi_venue_quant_bot::arcus::{ArcusEnvironment, OrderSide, PlaceOrder, PlaceOrderRequest};

#[test]
fn environments_use_the_official_arcus_endpoints() {
    assert_eq!(
        ArcusEnvironment::Mainnet.rest_url(),
        "https://api.arcus.xyz"
    );
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

#[test]
fn rest_order_must_match_every_signed_field_and_timestamp_unit() {
    let signed = PlaceOrder {
        address: "0xAABBCCDDEEFF0011223344556677889900AABBCC".into(),
        account_index: 0,
        market_id: 1,
        side: OrderSide::Buy,
        price_ticks: 50_000,
        quantity_quantums: 100,
        good_til_time_ns: 4_102_444_800_000_000_000,
        time_in_force: arcus::TimeInForce::Gtt,
        reduce_only: false,
        client_id: Some("order_1".into()),
    };
    let mut request = PlaceOrderRequest {
        address: signed.address.clone(),
        market_id: 1,
        account_index: 0,
        order_side: OrderSide::Buy,
        order_type: "LIMIT".into(),
        quantity: "0.01".into(),
        price: "50000".into(),
        time_in_force: arcus::TimeInForce::Gtt,
        good_til_time: "4102444800000000".into(),
        timestamp: 1_712_345_678_000_000_000,
        client_id: Some("order_1".into()),
        reduce_only: false,
    };

    signed.validate_rest_request(&request).unwrap();
    request.good_til_time = "4102444800000001".into();
    assert!(signed.validate_rest_request(&request).is_err());
}

#[test]
fn official_market_and_bbo_shapes_deserialize() {
    let markets: arcus::MarketsResponse = serde_json::from_str(
        r#"{"markets":[{"marketDisplayName":"BTC-USD","marketId":1,"status":"ONLINE","baseAsset":"BTC","quoteAsset":"USD","tickSize":"0.1","stepSize":"0.001","type":"PERPETUAL","category":"CRYPTO","minOrderSize":"0.001","maxOrderSize":"100"}]}"#,
    )
    .unwrap();
    assert_eq!(markets.markets[0].market_id, 1);
    assert_eq!(markets.markets[0].tick_size, "0.1");

    let bbo: arcus::Bbo = serde_json::from_str(
        r#"{"bestBid":{"price":"49999.9","size":"1.2"},"bestAsk":null,"lastSequenceId":42}"#,
    )
    .unwrap();
    assert_eq!(bbo.best_bid.unwrap().price, "49999.9");
    assert!(bbo.best_ask.is_none());
}

#[test]
fn cancel_by_order_id_uses_the_official_typed_payload() {
    let cancel = arcus::CancelOrder {
        address: "0xAABBCCDDEEFF0011223344556677889900AABBCC".into(),
        account_index: 2,
        market_id: 7,
        target: arcus::CancelTarget::OrderId("abc123".into()),
    };
    assert_eq!(
        cancel.canonical_payload(1_712_345_678_000_000_000).unwrap(),
        r#"{"ad":"0xaabbccddeeff0011223344556677889900aabbcc","ai":2,"ct":1712345678000000000,"id":"abc123","m":7,"op":2,"v":1}"#
    );
}
