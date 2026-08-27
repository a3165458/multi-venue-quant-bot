use std::collections::HashSet;
use std::sync::{Arc, Barrier};

use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use multi_venue_quant_bot::aster::{
    decode_api_error, eip712_digest, form_encode, AsterCredentials, AsterError, AsterMarket,
    AsterNonce, AsterWsEvent, CountdownResponse, ExchangeInfo, ModifyOrderRequest, NewOrderRequest,
    OrderSide, UserTradesQuery,
};

const PRIVATE_KEY: &str = "0x4fd0a42218f3eae43a6ce26d22544e986139a01e5b34a62db53757ffca81bae1";
const SIGNER: &str = "0x21cF8Ae13Bb72632562c6Fff438652Ba1a151bb0";
const WRONG_SIGNER: &str = "0x63DD5aCC6b1aa0f563956C0e534DD30B6dcF7C4e";

#[test]
fn credentials_are_strict_derive_the_signer_and_redact_debug() {
    let credentials = AsterCredentials::new(SIGNER, PRIVATE_KEY).unwrap();
    assert_eq!(credentials.signer(), SIGNER.to_ascii_lowercase());

    let debug = format!("{credentials:?}");
    assert!(!debug.contains("4fd0a422"));
    assert!(debug.contains("<redacted>"));

    assert!(AsterCredentials::new(&SIGNER[2..], PRIVATE_KEY).is_err());
    assert!(AsterCredentials::new(WRONG_SIGNER, PRIVATE_KEY).is_err());
    assert!(AsterCredentials::new(SIGNER, &PRIVATE_KEY[2..]).is_err());
}

#[test]
fn nonce_is_microsecond_shaped_and_unique_under_concurrency() {
    let nonce = Arc::new(AsterNonce::new());
    let barrier = Arc::new(Barrier::new(16));
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let nonce = nonce.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                (0..128).map(|_| nonce.next()).collect::<Vec<_>>()
            })
        })
        .collect();

    let values: Vec<_> = handles
        .into_iter()
        .flat_map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(values.iter().copied().collect::<HashSet<_>>().len(), 2048);
    assert!(values.iter().all(|value| *value > 1_000_000_000_000_000));
}

#[test]
fn eip712_signature_is_ethereum_recoverable() {
    let credentials = AsterCredentials::new(SIGNER, PRIVATE_KEY).unwrap();
    let payload = "symbol=BTCUSDT&side=BUY&type=LIMIT&nonce=1748310859508867&signer=0x21cf8ae13bb72632562c6fff438652ba1a151bb0";
    let digest = eip712_digest(payload);
    let encoded = credentials.sign_digest(digest).unwrap();
    assert!(encoded.starts_with("0x"));
    assert_eq!(encoded.len(), 132);
    assert_eq!(
        encoded,
        "0xc0de63cecc96e5ca23d0b852a36a5207c31e217f19caefbb47c194e0b4e3b988\
         317fff4e36a1b5809e3c359c3e1022c7f345137c28551dca592ed5e22e69bf701c"
            .replace(char::is_whitespace, "")
    );

    let bytes = hex::decode(&encoded[2..]).unwrap();
    let signature = Signature::try_from(&bytes[..64]).unwrap();
    let recovery_id = RecoveryId::try_from(bytes[64] - 27).unwrap();
    let key = VerifyingKey::recover_from_prehash(&digest, &signature, recovery_id).unwrap();
    assert_eq!(
        multi_venue_quant_bot::aster::ethereum_address(&key),
        SIGNER.to_ascii_lowercase()
    );
}

#[test]
fn form_encoding_preserves_stable_order_and_matches_urlencoding() {
    let params = vec![
        ("symbol", "BTCUSDT"),
        ("newClientOrderId", "maker quote/1"),
        ("batchOrders", r#"[{"side":"BUY","price":"100.50"}]"#),
        ("note", "a+b&c"),
    ];
    assert_eq!(
        form_encode(params),
        "symbol=BTCUSDT&newClientOrderId=maker+quote%2F1&batchOrders=%5B%7B%22side%22%3A%22BUY%22%2C%22price%22%3A%22100.50%22%7D%5D&note=a%2Bb%26c"
    );
}

fn exchange_info_json() -> &'static str {
    r#"{
      "timezone":"UTC","serverTime":1499827319559,
      "rateLimits":[],
      "symbols":[{
        "symbol":"BTCUSDT","pair":"BTCUSDT","contractType":"PERPETUAL",
        "status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT",
        "pricePrecision":2,"quantityPrecision":3,
        "filters":[
          {"filterType":"PRICE_FILTER","minPrice":"10.00","maxPrice":"1000000.00","tickSize":"0.10"},
          {"filterType":"LOT_SIZE","minQty":"0.001","maxQty":"100.000","stepSize":"0.001"},
          {"filterType":"MIN_NOTIONAL","notional":"5"}
        ],
        "orderTypes":["LIMIT","MARKET"],"timeInForce":["GTC","GTX"]
      }]
    }"#
}

#[test]
fn market_filters_quantize_maker_prices_directionally_and_quantity_down() {
    let info: ExchangeInfo = serde_json::from_str(exchange_info_json()).unwrap();
    let market = AsterMarket::try_from(&info.symbols[0]).unwrap();

    let buy = market
        .quantize_maker("50000.19", "0.0109", OrderSide::Buy)
        .unwrap();
    assert_eq!(buy.price, "50000.1");
    assert_eq!(buy.quantity, "0.01");

    let sell = market
        .quantize_maker("50000.11", "0.0109", OrderSide::Sell)
        .unwrap();
    assert_eq!(sell.price, "50000.2");
    assert_eq!(sell.quantity, "0.01");
}

#[test]
fn market_filters_enforce_min_max_and_min_notional_boundaries() {
    let info: ExchangeInfo = serde_json::from_str(exchange_info_json()).unwrap();
    let market = AsterMarket::try_from(&info.symbols[0]).unwrap();

    assert!(market
        .quantize_maker("10.00", "0.500", OrderSide::Buy)
        .is_ok());
    assert!(market
        .quantize_maker("10.00", "0.499", OrderSide::Buy)
        .is_err());
    assert!(market
        .quantize_reduce_only("10.00", "0.001", OrderSide::Buy)
        .is_ok());
    assert!(market
        .quantize_maker("9.99", "1.000", OrderSide::Buy)
        .is_err());
    assert!(market
        .quantize_maker("1000000.01", "0.001", OrderSide::Sell)
        .is_err());
    assert!(market
        .quantize_maker("100.00", "100.001", OrderSide::Buy)
        .is_err());
}

#[test]
fn market_quantization_uses_filter_minimum_as_grid_origin() {
    let symbol: multi_venue_quant_bot::aster::ExchangeSymbol = serde_json::from_str(
        r#"{
          "symbol":"ODDUSDT","pair":"ODDUSDT","contractType":"PERPETUAL",
          "status":"TRADING","baseAsset":"ODD","quoteAsset":"USDT",
          "pricePrecision":2,"quantityPrecision":3,
          "filters":[
            {"filterType":"PRICE_FILTER","minPrice":"0.05","maxPrice":"100","tickSize":"0.10"},
            {"filterType":"LOT_SIZE","minQty":"0.005","maxQty":"100","stepSize":"0.010"},
            {"filterType":"MIN_NOTIONAL","notional":"0"}
          ]
        }"#,
    )
    .unwrap();
    let market = AsterMarket::try_from(&symbol).unwrap();

    let buy = market
        .quantize_maker("10.12", "1.019", OrderSide::Buy)
        .unwrap();
    assert_eq!(buy.price, "10.05");
    assert_eq!(buy.quantity, "1.015");
    let sell = market
        .quantize_maker("10.12", "1.019", OrderSide::Sell)
        .unwrap();
    assert_eq!(sell.price, "10.15");
}

#[test]
fn http_errors_distinguish_rate_ban_unknown_execution_and_api_error() {
    assert!(matches!(
        decode_api_error(429, r#"{"code":-1003,"msg":"too many requests"}"#),
        AsterError::RateLimited { .. }
    ));
    assert!(matches!(
        decode_api_error(418, "banned"),
        AsterError::IpBanned { .. }
    ));
    assert!(matches!(
        decode_api_error(503, "Service Unavailable"),
        AsterError::UnknownExecution { .. }
    ));
    match decode_api_error(400, r#"{"code":-1121,"msg":"Invalid symbol."}"#) {
        AsterError::Api { status, code, .. } => {
            assert_eq!(status, 400);
            assert_eq!(code, Some(-1121));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn parses_book_ticker_direct_and_combined_payloads() {
    let raw = r#"{"e":"bookTicker","u":400900217,"E":1568014460893,"T":1568014460891,"s":"BNBUSDT","b":"25.35190000","B":"31.21000000","a":"25.36520000","A":"40.66000000"}"#;
    let AsterWsEvent::Bbo(bbo) = AsterWsEvent::parse(raw).unwrap() else {
        panic!("expected bbo");
    };
    assert_eq!(bbo.symbol, "BNBUSDT");
    assert_eq!(bbo.bid_price, "25.35190000");
    assert_eq!(bbo.update_id, 400900217);

    let combined = format!(r#"{{"stream":"bnbusdt@bookTicker","data":{raw}}}"#);
    assert!(matches!(
        AsterWsEvent::parse(&combined).unwrap(),
        AsterWsEvent::Bbo(_)
    ));
}

#[test]
fn parses_order_account_expiry_and_ignores_unknown_events() {
    let order = r#"{"e":"ORDER_TRADE_UPDATE","E":1568879465651,"T":1568879465650,"o":{"s":"BTCUSDT","c":"maker-1","S":"SELL","o":"LIMIT","f":"GTX","q":"0.001","p":"50000","ap":"50001","sp":"0","x":"TRADE","X":"FILLED","i":8886774,"l":"0.001","z":"0.001","L":"50001","n":"0.01","N":"USDT","T":1568879465650,"t":123,"b":"0","a":"0","m":true,"R":false,"wt":"CONTRACT_PRICE","ot":"LIMIT","ps":"BOTH","cp":false,"rp":"1.25"}}"#;
    let AsterWsEvent::Order(update) = AsterWsEvent::parse(order).unwrap() else {
        panic!("expected order update");
    };
    assert_eq!(update.order.symbol, "BTCUSDT");
    assert_eq!(update.order.status, "FILLED");
    assert_eq!(update.order.realized_profit, "1.25");

    let account = r#"{"e":"ACCOUNT_UPDATE","E":1564745798939,"T":1564745798938,"a":{"m":"ORDER","B":[{"a":"USDT","wb":"122624.12345678","cw":"100.12345678","bc":"50.12345678"}],"P":[{"s":"BTCUSDT","pa":"0.001","ep":"50000","cr":"200","up":"1.5","mt":"cross","iw":"0","ps":"BOTH"}]}}"#;
    let AsterWsEvent::Account(update) = AsterWsEvent::parse(account).unwrap() else {
        panic!("expected account update");
    };
    assert_eq!(update.balances[0].asset, "USDT");
    assert_eq!(update.positions[0].position_amount, "0.001");

    assert!(matches!(
        AsterWsEvent::parse(r#"{"e":"listenKeyExpired","E":1576653824250}"#).unwrap(),
        AsterWsEvent::ListenKeyExpired { .. }
    ));
    assert!(matches!(
        AsterWsEvent::parse(r#"{"e":"MARGIN_CALL","E":1}"#).unwrap(),
        AsterWsEvent::Ignored
    ));
}

#[test]
fn parses_partial_depth_updates_for_queue_analysis() {
    let payload = r#"{"e":"depthUpdate","E":1568014460893,"T":1568014460891,"s":"BTCUSDT","U":100,"u":101,"pu":99,"b":[["100.0","2.0"],["99.9","3.0"]],"a":[["100.1","1.5"],["100.2","4.0"]]}"#;
    let AsterWsEvent::Depth(depth) = AsterWsEvent::parse(payload).unwrap() else {
        panic!("expected depth update");
    };
    assert_eq!(depth.symbol, "BTCUSDT");
    assert_eq!(depth.bids[0], ("100.0".to_string(), "2.0".to_string()));
    assert_eq!(depth.asks.len(), 2);
    assert_eq!(depth.final_update_id, 101);
}

#[test]
fn official_rest_dto_shapes_deserialize() {
    let order: multi_venue_quant_bot::aster::Order = serde_json::from_str(
        r#"{"clientOrderId":"maker-1","cumQty":"0","cumQuote":"0","executedQty":"0","orderId":22542179,"avgPrice":"0.00000","origQty":"0.01","price":"50000","reduceOnly":false,"side":"BUY","positionSide":"BOTH","status":"NEW","stopPrice":"0","closePosition":false,"symbol":"BTCUSDT","timeInForce":"GTX","type":"LIMIT","origType":"LIMIT","updateTime":1566818724722,"workingType":"CONTRACT_PRICE","priceProtect":false}"#,
    )
    .unwrap();
    assert_eq!(order.order_id, 22542179);
    assert_eq!(order.client_order_id, "maker-1");

    let account: multi_venue_quant_bot::aster::Account = serde_json::from_str(
        r#"{"feeTier":0,"canTrade":true,"canDeposit":true,"canWithdraw":true,"updateTime":0,"totalInitialMargin":"0","totalMaintMargin":"0","totalWalletBalance":"23.72469206","totalUnrealizedProfit":"0","totalMarginBalance":"23.72469206","totalPositionInitialMargin":"0","totalOpenOrderInitialMargin":"0","totalCrossWalletBalance":"23.72469206","totalCrossUnPnl":"0","availableBalance":"23.72469206","maxWithdrawAmount":"23.72469206","assets":[{"asset":"USDT","walletBalance":"23.72469206","unrealizedProfit":"0","marginBalance":"23.72469206","maintMargin":"0","initialMargin":"0","positionInitialMargin":"0","openOrderInitialMargin":"0","crossWalletBalance":"23.72469206","crossUnPnl":"0","availableBalance":"23.72469206","maxWithdrawAmount":"23.72469206","marginAvailable":true,"updateTime":1625474304765}],"positions":[{"symbol":"BTCUSDT","initialMargin":"0","maintMargin":"0","unrealizedProfit":"0","positionInitialMargin":"0","openOrderInitialMargin":"0","leverage":"20","isolated":false,"entryPrice":"0","maxNotional":"250000","positionSide":"BOTH","positionAmt":"0","updateTime":0}]}"#,
    )
    .unwrap();
    assert_eq!(account.assets[0].asset, "USDT");
    assert_eq!(account.positions[0].symbol, "BTCUSDT");

    let countdown: CountdownResponse =
        serde_json::from_str(r#"{"symbol":"BTCUSDT","countdownTime":"120000"}"#).unwrap();
    assert_eq!(countdown.countdown_time, 120_000);
}

#[test]
fn user_trade_cursor_never_combines_from_id_with_time_range() {
    let params = UserTradesQuery {
        symbol: "BTCUSDT".into(),
        start_time: Some(1),
        end_time: Some(2),
        from_id: Some(3),
        limit: Some(1_000),
    }
    .params();
    assert!(params
        .iter()
        .any(|(key, value)| key == "fromId" && value == "3"));
    assert!(!params.iter().any(|(key, _)| key == "startTime"));
    assert!(!params.iter().any(|(key, _)| key == "endTime"));
}

#[test]
fn maker_orders_request_non_blocking_ack_and_parse_minimal_ack() {
    let request = NewOrderRequest::maker_limit(
        "BTCUSDT",
        OrderSide::Buy,
        "50000",
        "0.001",
        Some("maker-1".into()),
    );
    assert!(request
        .params()
        .iter()
        .any(|(key, value)| key == "newOrderRespType" && value == "ACK"));

    let order: multi_venue_quant_bot::aster::Order =
        serde_json::from_str(r#"{"symbol":"BTCUSDT","orderId":42,"clientOrderId":"maker-1"}"#)
            .unwrap();
    assert_eq!(order.status, "NEW");
}

#[test]
fn modify_order_request_requires_price_quantity_and_one_order_target() {
    let request = ModifyOrderRequest::new("BTCUSDT", Some(42), None, "50001.0", "0.002").unwrap();
    let params = request.params();
    assert!(params
        .iter()
        .any(|(key, value)| key == "orderId" && value == "42"));
    assert!(params
        .iter()
        .any(|(key, value)| key == "price" && value == "50001.0"));
    assert!(ModifyOrderRequest::new("BTCUSDT", None, None, "1", "1").is_err());
}

#[test]
fn income_ids_accept_aster_numeric_transaction_ids() {
    let income: multi_venue_quant_bot::aster::Income = serde_json::from_str(
        r#"{"symbol":"BTCUSDT","incomeType":"COMMISSION","income":"-0.01","asset":"USDT","info":"","time":1780000000000,"tranId":12345,"tradeId":"678"}"#,
    )
    .unwrap();
    assert_eq!(income.tran_id, "12345");
    assert_eq!(income.trade_id, "678");
}
