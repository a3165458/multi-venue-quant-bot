use lighter_bot::arcus::{
    ArcusKeypair, ArcusMarket, ArcusWsEvent, DecimalGrid, MarketPosition, OrderSide,
};

#[test]
fn rfc8032_private_key_derives_the_expected_arcus_api_key() {
    let key = ArcusKeypair::from_secret_hex(
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
    )
    .unwrap();
    assert_eq!(
        key.public_key_hex(),
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
    );
    assert_eq!(
        key.sign_hex(b"") ,
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
         5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
            .replace(char::is_whitespace, "")
    );
}

#[test]
fn decimal_grid_converts_without_floating_point_rounding() {
    let grid = DecimalGrid::new("0.05").unwrap();
    assert_eq!(grid.units("123.45").unwrap(), 2469);
    assert_eq!(grid.decimal(2469), "123.45");
    assert!(grid.units("123.456").is_err());
    assert!(DecimalGrid::new("0").is_err());
}

#[test]
fn arcus_market_builds_matching_signing_and_rest_values() {
    let market = ArcusMarket {
        market_id: 7,
        symbol: "BTC-USD".into(),
        tick_size: DecimalGrid::new("0.1").unwrap(),
        step_size: DecimalGrid::new("0.001").unwrap(),
    };
    let order = market
        .order_values("50000.2", "0.015", OrderSide::Buy)
        .unwrap();
    assert_eq!(order.price_ticks, 500_002);
    assert_eq!(order.quantity_quantums, 15);
    assert_eq!(order.price, "50000.2");
    assert_eq!(order.quantity, "0.015");
}

#[test]
fn websocket_bbo_messages_are_normalized_for_the_strategy_loop() {
    let event = ArcusWsEvent::parse(
        r#"{"type":"channel_data","channel":"bbo","id":"BTC-USD","contents":{"bestBid":{"price":"49999.9","size":"1.2"},"bestAsk":{"price":"50000.1","size":"0.8"},"lastSequenceId":42}}"#,
    )
    .unwrap();
    assert_eq!(
        event,
        ArcusWsEvent::Bbo {
            symbol: "BTC-USD".into(),
            bid: 49_999.9,
            ask: 50_000.1,
            sequence: 42,
        }
    );
    assert!(ArcusWsEvent::parse(r#"{"type":"channel_data","channel":"bbo"}"#).is_err());
}

#[test]
fn signed_positions_preserve_long_and_short_direction() {
    let long: MarketPosition = serde_json::from_str(
        r#"{"marketId":1,"marketDisplayName":"BTC-USD","side":"LONG","size":"0.2","entryPrice":"50000","unrealizedPnl":"25","leverage":"3"}"#,
    )
    .unwrap();
    let short: MarketPosition = serde_json::from_str(
        r#"{"marketId":2,"marketDisplayName":"ETH-USD","side":"SHORT","size":"1.5","entryPrice":"3000","unrealizedPnl":"-10","leverage":"2"}"#,
    )
    .unwrap();
    assert_eq!(long.signed_size().unwrap(), 0.2);
    assert_eq!(short.signed_size().unwrap(), -1.5);
}
