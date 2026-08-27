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
fn arcus_fill_exposes_authoritative_volume_and_net_realized_pnl() {
    let fill: arcus::ArcusFill = serde_json::from_str(
        r#"{"tradeId":"trade-1","orderId":"order-1","marketId":1,"marketDisplayName":"BTC-USD","side":"SELL","originalSize":"0.02","size":"0.01","price":"50000.5","fee":"0.25","closedPnl":"12.34","role":"MAKER","positionEffect":"CLOSE_LONG","createdAt":1785801600123456}"#,
    )
    .unwrap();

    assert!((fill.notional().unwrap() - 500.005).abs() < 1e-9);
    assert!((fill.net_realized_pnl().unwrap() - 12.09).abs() < 1e-9);
    assert!(fill.is_position_close());
}

#[test]
fn opening_fill_fee_is_counted_without_inventing_closed_pnl() {
    let fill: arcus::ArcusFill = serde_json::from_str(
        r#"{"tradeId":"trade-2","orderId":"order-2","marketId":1,"marketDisplayName":"BTC-USD","side":"BUY","originalSize":"0.01","size":"0.01","price":"50000","fee":"0.10","role":"TAKER","positionEffect":"OPEN_LONG","createdAt":1785801600123456}"#,
    )
    .unwrap();

    assert!((fill.net_realized_pnl().unwrap() + 0.10).abs() < 1e-9);
    assert!(!fill.is_position_close());
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

#[test]
fn websocket_orders_channel_parses_terminal_lifecycle() {
    let event = arcus::ArcusWsEvent::parse(
        r#"{"type":"channel_data","channel":"orders","id":"0xabc","contents":{"orderId":"ord-1","clientId":"mq_BTC-USD_buy","marketId":1,"marketDisplayName":"BTC-USD","side":"BUY","status":"FILLED","price":"50000.0","originalSize":"0.01","remainingSize":"0","createdAt":1712345678000000,"updatedAt":1712345679000000,"sequenceNumber":5001}}"#,
    )
    .expect("parse");

    let arcus::ArcusWsEvent::Orders(orders) = event else {
        panic!("expected orders event");
    };
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].client_id.as_deref(), Some("mq_BTC-USD_buy"));
    assert_eq!(orders[0].status, "FILLED");
}

#[test]
fn websocket_orders_channel_preserves_rejection_reason() {
    let event = arcus::ArcusWsEvent::parse(
        r#"{"type":"channel_data","channel":"orders","id":"0xabc","contents":{"orderId":"ord-2","clientId":"mq_USO-USD_buy","marketId":22,"marketDisplayName":"USO-USD","side":"BUY","status":"REJECTED","rejectionReason":"DUPLICATE_CLIENT_ID","price":"126.0","originalSize":"0.3","remainingSize":"0.3","createdAt":1712345678000000,"updatedAt":1712345679000000,"sequenceNumber":5002}}"#,
    )
    .expect("parse");

    let arcus::ArcusWsEvent::Orders(orders) = event else {
        panic!("expected orders event");
    };
    assert_eq!(
        orders[0].rejection_reason.as_deref(),
        Some("DUPLICATE_CLIENT_ID")
    );
}

#[test]
fn websocket_user_fill_channel_parses_trade_id() {
    let event = arcus::ArcusWsEvent::parse(
        r#"{"type":"channel_data","channel":"userFills","id":"0xabc","contents":{"isSnapshot":false,"tradeId":"trade-1","orderId":"ord-1","market":"BTC-USD","side":"BUY","fillPrice":"50000.00","fillSize":"0.01"}}"#,
    )
    .expect("parse");

    let arcus::ArcusWsEvent::UserFills(fills) = event else {
        panic!("expected user fills event");
    };
    assert_eq!(fills.len(), 1);
    assert_eq!(fills[0].trade_id, "trade-1");
}

#[test]
fn websocket_funding_channel_parses_signed_payment() {
    let event = arcus::ArcusWsEvent::parse(
        r#"{"type":"channel_data","channel":"funding","id":"0xabc","contents":{"marketId":1,"marketDisplayName":"BTC-USD","fundingRate":"0.00001234","size":"0.5","payment":"-3.21","time":1700003600000000}}"#,
    )
    .expect("parse");

    let arcus::ArcusWsEvent::Funding(payments) = event else {
        panic!("expected funding event");
    };
    assert_eq!(payments.len(), 1);
    assert!((payments[0].payment_value().expect("payment") + 3.21).abs() < 1e-12);
}
