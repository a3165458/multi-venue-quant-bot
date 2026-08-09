//! Arcus REST API adapter.
//!
//! Arcus uses Ed25519 API keys. This module deliberately accepts a signer callback instead of
//! owning private-key material, so applications can keep keys in their existing secret store or
//! hardware-backed signer.

use ed25519_dalek::{Signer as _, SigningKey};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};

pub const MAINNET_REST_URL: &str = "https://api.arcus.xyz";
pub const MAINNET_WEBSOCKET_URL: &str = "wss://api.arcus.xyz/v1/ws";
pub const TESTNET_REST_URL: &str = "https://api.testnet.arcus.xyz";
pub const TESTNET_WEBSOCKET_URL: &str = "wss://api.testnet.arcus.xyz/v1/ws";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArcusEnvironment {
    Mainnet,
    Testnet,
}

impl ArcusEnvironment {
    pub const fn rest_url(self) -> &'static str {
        match self {
            Self::Mainnet => MAINNET_REST_URL,
            Self::Testnet => TESTNET_REST_URL,
        }
    }

    pub const fn websocket_url(self) -> &'static str {
        match self {
            Self::Mainnet => MAINNET_WEBSOCKET_URL,
            Self::Testnet => TESTNET_WEBSOCKET_URL,
        }
    }
}

#[derive(Debug, Error)]
pub enum ArcusError {
    #[error("invalid Arcus request: {0}")]
    InvalidRequest(String),
    #[error("Arcus signing failed: {0}")]
    Signing(String),
    #[error("Arcus HTTP request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Arcus API returned HTTP {status}: {message}")]
    Api { status: StatusCode, message: String },
}

pub struct ArcusKeypair(SigningKey);

impl ArcusKeypair {
    pub fn from_secret_hex(secret: &str) -> Result<Self, ArcusError> {
        validate_hex(secret, 64, "Arcus secret key")?;
        let bytes = hex::decode(secret)
            .map_err(|_| ArcusError::InvalidRequest("invalid Arcus secret key hex".into()))?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| {
            ArcusError::InvalidRequest("Arcus secret key must contain 32 bytes".into())
        })?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.0.verifying_key().as_bytes())
    }

    pub fn sign_hex(&self, message: &[u8]) -> String {
        hex::encode(self.0.sign(message).to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecimalGrid {
    coefficient: u64,
    scale: u32,
}

impl DecimalGrid {
    pub fn new(value: &str) -> Result<Self, ArcusError> {
        let (coefficient, scale) = parse_decimal(value)?;
        if coefficient == 0 {
            return Err(ArcusError::InvalidRequest(
                "decimal grid must be positive".into(),
            ));
        }
        Ok(Self { coefficient, scale })
    }

    pub fn units(&self, value: &str) -> Result<u64, ArcusError> {
        let (coefficient, scale) = parse_decimal(value)?;
        let common_scale = self.scale.max(scale);
        let value_scaled = coefficient
            .checked_mul(pow10(common_scale - scale)?)
            .ok_or_else(|| ArcusError::InvalidRequest("decimal value overflow".into()))?;
        let grid_scaled = self
            .coefficient
            .checked_mul(pow10(common_scale - self.scale)?)
            .ok_or_else(|| ArcusError::InvalidRequest("decimal grid overflow".into()))?;
        if value_scaled == 0 || value_scaled % grid_scaled != 0 {
            return Err(ArcusError::InvalidRequest(format!(
                "{value} is not an exact multiple of the market grid"
            )));
        }
        Ok(value_scaled / grid_scaled)
    }

    pub fn decimal(&self, units: u64) -> String {
        format_decimal(self.coefficient.saturating_mul(units), self.scale)
    }

    pub fn nearest(&self, value: f64) -> Result<(u64, String), ArcusError> {
        self.quantize(value, true)
    }

    pub fn floor(&self, value: f64) -> Result<(u64, String), ArcusError> {
        self.quantize(value, false)
    }

    fn quantize(&self, value: f64, nearest: bool) -> Result<(u64, String), ArcusError> {
        if !value.is_finite() || value <= 0.0 {
            return Err(ArcusError::InvalidRequest(
                "order value must be finite and positive".into(),
            ));
        }
        let grid = self.coefficient as f64 / 10_f64.powi(self.scale as i32);
        let raw = value / grid;
        let units = if nearest { raw.round() } else { raw.floor() };
        if units < 1.0 || units > u64::MAX as f64 {
            return Err(ArcusError::InvalidRequest(
                "order value is outside the supported range".into(),
            ));
        }
        let units = units as u64;
        Ok((units, self.decimal(units)))
    }
}

fn pow10(power: u32) -> Result<u64, ArcusError> {
    10_u64
        .checked_pow(power)
        .ok_or_else(|| ArcusError::InvalidRequest("decimal precision is too large".into()))
}

fn parse_decimal(value: &str) -> Result<(u64, u32), ArcusError> {
    if value.is_empty() || value.starts_with('-') || value.starts_with('+') {
        return Err(ArcusError::InvalidRequest(
            "expected an unsigned decimal string".into(),
        ));
    }
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 18
    {
        return Err(ArcusError::InvalidRequest("invalid decimal string".into()));
    }
    let scale = fraction.len() as u32;
    let digits = format!("{whole}{fraction}");
    let coefficient = digits
        .parse::<u64>()
        .map_err(|_| ArcusError::InvalidRequest("decimal value overflow".into()))?;
    Ok((coefficient, scale))
}

fn format_decimal(coefficient: u64, scale: u32) -> String {
    if scale == 0 {
        return coefficient.to_string();
    }
    let mut digits = format!("{:0width$}", coefficient, width = scale as usize + 1);
    digits.insert(digits.len() - scale as usize, '.');
    while digits.ends_with('0') {
        digits.pop();
    }
    if digits.ends_with('.') {
        digits.pop();
    }
    digits
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArcusMarket {
    pub market_id: u16,
    pub symbol: String,
    pub tick_size: DecimalGrid,
    pub step_size: DecimalGrid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArcusOrderValues {
    pub market_id: u16,
    pub side: OrderSide,
    pub price_ticks: u64,
    pub quantity_quantums: u64,
    pub price: String,
    pub quantity: String,
}

impl ArcusMarket {
    pub fn order_values(
        &self,
        price: &str,
        quantity: &str,
        side: OrderSide,
    ) -> Result<ArcusOrderValues, ArcusError> {
        Ok(ArcusOrderValues {
            market_id: self.market_id,
            side,
            price_ticks: self.tick_size.units(price)?,
            quantity_quantums: self.step_size.units(quantity)?,
            price: price.to_string(),
            quantity: quantity.to_string(),
        })
    }

    pub fn quantize_order(
        &self,
        price: f64,
        quantity: f64,
        side: OrderSide,
    ) -> Result<ArcusOrderValues, ArcusError> {
        let (price_ticks, price) = self.tick_size.nearest(price)?;
        let (quantity_quantums, quantity) = self.step_size.floor(quantity)?;
        Ok(ArcusOrderValues {
            market_id: self.market_id,
            side,
            price_ticks,
            quantity_quantums,
            price,
            quantity,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArcusWsEvent {
    Bbo {
        symbol: String,
        bid: f64,
        ask: f64,
        bid_size: f64,
        ask_size: f64,
        sequence: u64,
    },
    Disconnected,
    Ignored,
}

impl ArcusWsEvent {
    pub fn parse(text: &str) -> Result<Self, ArcusError> {
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|error| ArcusError::InvalidRequest(error.to_string()))?;
        if value.get("type").and_then(|v| v.as_str()) != Some("channel_data") {
            return Ok(Self::Ignored);
        }
        if value.get("channel").and_then(|v| v.as_str()) != Some("bbo") {
            return Ok(Self::Ignored);
        }
        let symbol = value
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ArcusError::InvalidRequest("BBO event is missing id".into()))?;
        let contents = value
            .get("contents")
            .ok_or_else(|| ArcusError::InvalidRequest("BBO event is missing contents".into()))?;
        let price = |side: &str| -> Result<f64, ArcusError> {
            contents
                .get(side)
                .and_then(|v| v.get("price"))
                .and_then(|v| v.as_str())
                .and_then(|v| v.parse().ok())
                .filter(|v: &f64| v.is_finite() && *v > 0.0)
                .ok_or_else(|| ArcusError::InvalidRequest(format!("invalid BBO {side}")))
        };
        let size = |side: &str| -> Result<f64, ArcusError> {
            contents
                .get(side)
                .and_then(|v| v.get("size"))
                .and_then(|v| v.as_str())
                .and_then(|v| v.parse().ok())
                .filter(|v: &f64| v.is_finite() && *v > 0.0)
                .ok_or_else(|| ArcusError::InvalidRequest(format!("invalid BBO {side} size")))
        };
        let sequence = contents
            .get("lastSequenceId")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| ArcusError::InvalidRequest("BBO event is missing sequence".into()))?;
        Ok(Self::Bbo {
            symbol: symbol.to_string(),
            bid: price("bestBid")?,
            ask: price("bestAsk")?,
            bid_size: size("bestBid")?,
            ask_size: size("bestAsk")?,
            sequence,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPosition {
    pub market_id: u16,
    pub market_display_name: String,
    pub side: String,
    pub size: String,
    #[serde(rename = "averageEntryPrice", alias = "entryPrice")]
    pub entry_price: String,
    pub unrealized_pnl: String,
    pub leverage: String,
}

impl MarketPosition {
    pub fn signed_size(&self) -> Result<f64, ArcusError> {
        let size = self
            .size
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .ok_or_else(|| ArcusError::InvalidRequest("invalid position size".into()))?;
        match self.side.as_str() {
            "LONG" | "BUY" => Ok(size.abs()),
            "SHORT" | "SELL" => Ok(-size.abs()),
            _ => Err(ArcusError::InvalidRequest("invalid position side".into())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    const fn signing_value(self) -> u8 {
        match self {
            Self::Buy => 0,
            Self::Sell => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    #[serde(rename = "GTT")]
    Gtt,
    #[serde(rename = "FOK")]
    Fok,
    #[serde(rename = "IOC")]
    Ioc,
    #[serde(rename = "ALO")]
    Alo,
}

impl TimeInForce {
    const fn signing_value(self) -> u8 {
        match self {
            Self::Gtt => 0,
            Self::Fok => 1,
            Self::Ioc => 2,
            Self::Alo => 3,
        }
    }
}

/// Engine-native values used to produce Arcus' typed order signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceOrder {
    pub address: String,
    pub account_index: u8,
    pub market_id: u16,
    pub side: OrderSide,
    pub price_ticks: u64,
    pub quantity_quantums: u64,
    pub good_til_time_ns: u64,
    pub time_in_force: TimeInForce,
    pub reduce_only: bool,
    pub client_id: Option<String>,
}

impl PlaceOrder {
    pub fn canonical_payload(&self, timestamp_ns: u64) -> Result<String, ArcusError> {
        validate_address(&self.address)?;
        if self.account_index > 9 {
            return Err(ArcusError::InvalidRequest(
                "account_index must be between 0 and 9".into(),
            ));
        }
        if self.price_ticks == 0 || self.quantity_quantums == 0 {
            return Err(ArcusError::InvalidRequest(
                "price_ticks and quantity_quantums must be positive".into(),
            ));
        }
        if timestamp_ns == 0 || self.good_til_time_ns == 0 {
            return Err(ArcusError::InvalidRequest(
                "timestamps must be positive".into(),
            ));
        }
        if let Some(client_id) = &self.client_id {
            validate_client_id(client_id)?;
        }

        let address = self.address.to_ascii_lowercase();
        let client = self
            .client_id
            .as_ref()
            .map(|id| format!(",\"c\":\"{}\"", id))
            .unwrap_or_default();
        Ok(format!(
            r#"{{"ad":"{}","ai":{}{},"ct":{},"g":{},"m":{},"op":1,"p":{},"q":{},"r":{},"s":{},"t":{},"v":1}}"#,
            address,
            self.account_index,
            client,
            timestamp_ns,
            self.good_til_time_ns,
            self.market_id,
            self.price_ticks,
            self.quantity_quantums,
            u8::from(self.reduce_only),
            self.side.signing_value(),
            self.time_in_force.signing_value(),
        ))
    }

    pub fn validate_rest_request(&self, request: &PlaceOrderRequest) -> Result<(), ArcusError> {
        let good_til_time_us = request.good_til_time.parse::<u64>().map_err(|_| {
            ArcusError::InvalidRequest("good_til_time must be epoch microseconds".into())
        })?;
        let matching = self.address.eq_ignore_ascii_case(&request.address)
            && self.market_id == request.market_id
            && self.account_index == request.account_index
            && self.side == request.order_side
            && self.time_in_force == request.time_in_force
            && self.reduce_only == request.reduce_only
            && self.client_id == request.client_id
            && good_til_time_us.checked_mul(1_000) == Some(self.good_til_time_ns);
        if !matching {
            return Err(ArcusError::InvalidRequest(
                "signed values do not match the REST request".into(),
            ));
        }
        if !matches!(request.order_type.as_str(), "LIMIT" | "MARKET") {
            return Err(ArcusError::InvalidRequest(
                "order_type must be LIMIT or MARKET".into(),
            ));
        }
        Ok(())
    }
}

fn validate_address(address: &str) -> Result<(), ArcusError> {
    let hex = address
        .strip_prefix("0x")
        .or_else(|| address.strip_prefix("0X"))
        .ok_or_else(|| ArcusError::InvalidRequest("address must start with 0x".into()))?;
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ArcusError::InvalidRequest(
            "address must contain exactly 40 hexadecimal digits".into(),
        ));
    }
    Ok(())
}

fn validate_client_id(client_id: &str) -> Result<(), ArcusError> {
    if client_id.is_empty()
        || client_id.len() > 36
        || !client_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ArcusError::InvalidRequest(
            "client_id must be 1-36 ASCII letters, digits, '-' or '_'".into(),
        ));
    }
    Ok(())
}

fn validate_market_name(market: &str) -> Result<(), ArcusError> {
    if market.is_empty()
        || market.len() > 64
        || !market
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ArcusError::InvalidRequest(
            "market must be 1-64 ASCII letters, digits, '-' or '_'".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Market {
    pub market_display_name: String,
    pub market_id: u16,
    pub status: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub tick_size: String,
    pub step_size: String,
    #[serde(rename = "type")]
    pub market_type: String,
    pub category: String,
    pub min_order_size: String,
    pub max_order_size: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketsResponse {
    pub markets: Vec<Market>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BboLevel {
    pub price: String,
    pub size: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bbo {
    pub best_bid: Option<BboLevel>,
    pub best_ask: Option<BboLevel>,
    pub last_sequence_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub account_index: u8,
    pub address: String,
    pub equity: String,
    pub free_collateral: String,
    pub positions: serde_json::Map<String, serde_json::Value>,
    pub sequence_number: u64,
}

impl Account {
    pub fn market_positions(&self) -> Result<Vec<MarketPosition>, ArcusError> {
        self.positions
            .values()
            .cloned()
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(|error| ArcusError::InvalidRequest(error.to_string()))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArcusOpenOrder {
    pub order_id: String,
    pub client_id: Option<String>,
    pub market_id: u16,
    pub market_display_name: String,
    pub side: OrderSide,
    pub price: String,
    pub original_size: String,
    pub filled_size: String,
    pub remaining_size: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct OpenOrdersResponse {
    orders: Vec<ArcusOpenOrder>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArcusCandle {
    pub market_display_name: String,
    pub market_id: u16,
    pub timeframe: String,
    pub open_time: i64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
}

#[derive(Debug, Deserialize)]
struct CandlesResponse {
    candles: Vec<ArcusCandle>,
}

#[derive(Debug, Deserialize)]
pub struct CancelAllAcknowledgement {
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceOrderRequest {
    pub address: String,
    pub market_id: u16,
    pub account_index: u8,
    pub order_side: OrderSide,
    pub order_type: String,
    pub quantity: String,
    pub price: String,
    pub time_in_force: TimeInForce,
    pub good_til_time: String,
    pub timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub reduce_only: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderAcknowledgement {
    pub order_id: String,
    pub status: String,
    pub client_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelTarget {
    OrderId(String),
    ClientId(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelOrder {
    pub address: String,
    pub account_index: u8,
    pub market_id: u16,
    pub target: CancelTarget,
}

impl CancelOrder {
    pub fn canonical_payload(&self, timestamp_ns: u64) -> Result<String, ArcusError> {
        validate_address(&self.address)?;
        if self.account_index > 9 || timestamp_ns == 0 {
            return Err(ArcusError::InvalidRequest(
                "invalid account index or timestamp".into(),
            ));
        }
        let target = match &self.target {
            CancelTarget::OrderId(id)
                if !id.is_empty()
                    && id.len() <= 128
                    && id.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                format!(",\"ct\":{timestamp_ns},\"id\":\"{id}\"")
            }
            CancelTarget::ClientId(id) => {
                validate_client_id(id)?;
                format!(",\"c\":\"{id}\",\"ct\":{timestamp_ns}")
            }
            _ => {
                return Err(ArcusError::InvalidRequest(
                    "order_id must be 1-128 characters".into(),
                ))
            }
        };
        Ok(format!(
            r#"{{"ad":"{}","ai":{}{},"m":{},"op":2,"v":1}}"#,
            self.address.to_ascii_lowercase(),
            self.account_index,
            target,
            self.market_id
        ))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelOrderRequest {
    pub address: String,
    pub market_id: u16,
    pub account_index: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub timestamp: u64,
}

type Signer = dyn Fn(&[u8]) -> Result<String, String> + Send + Sync;

#[derive(Clone)]
pub struct ArcusClient {
    http: Client,
    base_url: String,
    api_key: Option<String>,
    signer: Option<Arc<Signer>>,
}

impl ArcusClient {
    pub fn public(environment: ArcusEnvironment) -> Self {
        Self::with_base_url(environment.rest_url())
    }

    pub fn with_base_url(base_url: &str) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key: None,
            signer: None,
        }
    }

    pub fn authenticated<F>(
        environment: ArcusEnvironment,
        api_key: impl Into<String>,
        signer: F,
    ) -> Result<Self, ArcusError>
    where
        F: Fn(&[u8]) -> Result<String, String> + Send + Sync + 'static,
    {
        let api_key = api_key.into();
        validate_hex(&api_key, 64, "API key")?;
        Ok(Self {
            http: Client::new(),
            base_url: environment.rest_url().to_owned(),
            api_key: Some(api_key.to_ascii_lowercase()),
            signer: Some(Arc::new(signer)),
        })
    }

    pub fn authenticated_with_keypair(
        environment: ArcusEnvironment,
        keypair: ArcusKeypair,
    ) -> Result<Self, ArcusError> {
        let keypair = Arc::new(keypair);
        let api_key = keypair.public_key_hex();
        Self::authenticated(environment, api_key, move |message| {
            Ok(keypair.sign_hex(message))
        })
    }

    pub async fn markets(&self) -> Result<Vec<Market>, ArcusError> {
        Ok(self
            .send_json::<MarketsResponse>(Method::GET, "/v1/markets", None)
            .await?
            .markets)
    }

    pub async fn bbo(&self, market: &str) -> Result<Bbo, ArcusError> {
        validate_market_name(market)?;
        self.send_json(Method::GET, &format!("/v1/bbo/{market}"), None)
            .await
    }

    pub async fn account(&self, address: &str, account_index: u8) -> Result<Account, ArcusError> {
        validate_address(address)?;
        if account_index > 9 {
            return Err(ArcusError::InvalidRequest(
                "account_index must be between 0 and 9".into(),
            ));
        }
        let path = format!("/v1/account?address={address}&accountIndex={account_index}");
        self.send_json(Method::GET, &path, None).await
    }

    pub async fn open_orders(
        &self,
        address: &str,
        account_index: u8,
    ) -> Result<Vec<ArcusOpenOrder>, ArcusError> {
        validate_address(address)?;
        if account_index > 9 {
            return Err(ArcusError::InvalidRequest(
                "account_index must be between 0 and 9".into(),
            ));
        }
        let path =
            format!("/v1/openOrders?address={address}&accountIndex={account_index}&limit=1000");
        Ok(self
            .send_json::<OpenOrdersResponse>(Method::GET, &path, None)
            .await?
            .orders)
    }

    pub async fn candles(
        &self,
        market: &str,
        timeframe: &str,
        countback: u16,
    ) -> Result<Vec<ArcusCandle>, ArcusError> {
        validate_market_name(market)?;
        const TIMEFRAMES: &[&str] = &[
            "1m", "3m", "5m", "15m", "30m", "1h", "2h", "4h", "6h", "8h", "12h", "1d", "3d", "1w",
        ];
        if !TIMEFRAMES.contains(&timeframe) || !(1..=1500).contains(&countback) {
            return Err(ArcusError::InvalidRequest(
                "invalid candle timeframe or countback".into(),
            ));
        }
        let to = chrono::Utc::now().timestamp_micros();
        let path = format!(
            "/v1/candles?market={market}&timeframe={timeframe}&to={to}&countback={countback}"
        );
        Ok(self
            .send_json::<CandlesResponse>(Method::GET, &path, None)
            .await?
            .candles)
    }

    pub async fn place_order(
        &self,
        signed: &PlaceOrder,
        request: &PlaceOrderRequest,
    ) -> Result<OrderAcknowledgement, ArcusError> {
        signed.validate_rest_request(request)?;
        let message = signed.canonical_payload(request.timestamp)?;
        let signer = self.signer.as_ref().ok_or_else(|| {
            ArcusError::InvalidRequest("authenticated client required for order placement".into())
        })?;
        let signature = signer(message.as_bytes()).map_err(ArcusError::Signing)?;
        validate_hex(&signature, 128, "signature")?;
        let body =
            serde_json::to_value(request).map_err(|e| ArcusError::InvalidRequest(e.to_string()))?;
        let path = format!("/v1/placeOrder?address={}", request.address);
        self.send_signed_json(Method::POST, &path, body, request.timestamp, &signature)
            .await
    }

    pub async fn cancel_order(
        &self,
        signed: &CancelOrder,
        request: &CancelOrderRequest,
    ) -> Result<OrderAcknowledgement, ArcusError> {
        let request_target = match (&request.order_id, &request.client_id) {
            (Some(id), None) => CancelTarget::OrderId(id.clone()),
            (None, Some(id)) => CancelTarget::ClientId(id.clone()),
            _ => {
                return Err(ArcusError::InvalidRequest(
                    "provide exactly one of order_id or client_id".into(),
                ))
            }
        };
        if !signed.address.eq_ignore_ascii_case(&request.address)
            || signed.account_index != request.account_index
            || signed.market_id != request.market_id
            || signed.target != request_target
        {
            return Err(ArcusError::InvalidRequest(
                "signed values do not match the REST request".into(),
            ));
        }
        let message = signed.canonical_payload(request.timestamp)?;
        let signer = self.signer.as_ref().ok_or_else(|| {
            ArcusError::InvalidRequest("authenticated client required for cancellation".into())
        })?;
        let signature = signer(message.as_bytes()).map_err(ArcusError::Signing)?;
        validate_hex(&signature, 128, "signature")?;
        let body =
            serde_json::to_value(request).map_err(|e| ArcusError::InvalidRequest(e.to_string()))?;
        let path = format!("/v1/cancelOrder?address={}", request.address);
        self.send_signed_json(Method::POST, &path, body, request.timestamp, &signature)
            .await
    }

    pub async fn cancel_all_orders(
        &self,
        address: &str,
        account_index: u8,
    ) -> Result<CancelAllAcknowledgement, ArcusError> {
        validate_address(address)?;
        if account_index > 9 {
            return Err(ArcusError::InvalidRequest(
                "account_index must be between 0 and 9".into(),
            ));
        }
        let timestamp = chrono::Utc::now()
            .timestamp_nanos_opt()
            .ok_or_else(|| ArcusError::InvalidRequest("system clock is out of range".into()))?
            as u64;
        let body = serde_json::json!({
            "accountIndex": account_index,
            "address": address,
        });
        let canonical = format!(
            r#"{{"accountIndex":{},"address":"{}"}}"#,
            account_index, address
        );
        let message = format!("{timestamp}cancelAllOrders{canonical}");
        let signer = self.signer.as_ref().ok_or_else(|| {
            ArcusError::InvalidRequest("authenticated client required for cancellation".into())
        })?;
        let signature = signer(message.as_bytes()).map_err(ArcusError::Signing)?;
        validate_hex(&signature, 128, "signature")?;
        let path = format!("/v1/cancelAllOrders?address={address}");
        self.send_signed_json(Method::POST, &path, body, timestamp, &signature)
            .await
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, ArcusError> {
        let mut request = self
            .http
            .request(method, format!("{}{}", self.base_url, path));
        if let Some(api_key) = &self.api_key {
            request = request.header("X-API-Key", api_key);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        decode_response(request.send().await?).await
    }

    async fn send_signed_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: serde_json::Value,
        timestamp: u64,
        signature: &str,
    ) -> Result<T, ArcusError> {
        let api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| ArcusError::InvalidRequest("authenticated client required".into()))?;
        let response = self
            .http
            .request(method, format!("{}{}", self.base_url, path))
            .header("X-API-Key", api_key)
            .header("X-Timestamp", timestamp.to_string())
            .header("X-Signature", signature)
            .json(&body)
            .send()
            .await?;
        decode_response(response).await
    }
}

pub struct ArcusWebSocket {
    url: String,
    events: broadcast::Sender<ArcusWsEvent>,
    commands: tokio::sync::mpsc::Sender<String>,
    command_rx: tokio::sync::Mutex<Option<tokio::sync::mpsc::Receiver<String>>>,
}

impl ArcusWebSocket {
    pub fn new(url: impl Into<String>) -> Self {
        let (events, _) = broadcast::channel(4096);
        let (commands, command_rx) = tokio::sync::mpsc::channel(256);
        Self {
            url: url.into(),
            events,
            commands,
            command_rx: tokio::sync::Mutex::new(Some(command_rx)),
        }
    }

    pub async fn connect(&self) -> Result<(), ArcusError> {
        let mut guard = self.command_rx.lock().await;
        let mut commands = guard.take().ok_or_else(|| {
            ArcusError::InvalidRequest("Arcus WebSocket is already connected".into())
        })?;
        let url = self.url.clone();
        let events = self.events.clone();
        let (socket, _) = connect_async(&url)
            .await
            .map_err(|error| ArcusError::Signing(format!("WebSocket connect failed: {error}")))?;
        let (mut writer, mut reader) = socket.split();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    command = commands.recv() => match command {
                        Some(command) => {
                            if writer.send(Message::Text(command)).await.is_err() { break; }
                        }
                        None => break,
                    },
                    message = reader.next() => match message {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(event) = ArcusWsEvent::parse(&text) {
                                if event != ArcusWsEvent::Ignored { let _ = events.send(event); }
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            if writer.send(Message::Pong(payload)).await.is_err() { break; }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        _ => {}
                    }
                }
            }
            let _ = events.send(ArcusWsEvent::Disconnected);
        });
        Ok(())
    }

    pub async fn subscribe_bbo(&self, market: &str) -> Result<(), ArcusError> {
        validate_market_name(market)?;
        let message = serde_json::json!({
            "type": "subscribe",
            "channel": "bbo",
            "id": market,
        });
        self.commands
            .send(message.to_string())
            .await
            .map_err(|_| ArcusError::InvalidRequest("WebSocket is disconnected".into()))
    }

    pub fn receiver(&self) -> broadcast::Receiver<ArcusWsEvent> {
        self.events.subscribe()
    }
}

fn validate_hex(value: &str, length: usize, label: &str) -> Result<(), ArcusError> {
    if value.len() != length || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ArcusError::InvalidRequest(format!(
            "{label} must be exactly {length} hexadecimal characters"
        )));
    }
    Ok(())
}

async fn decode_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, ArcusError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response.json().await?);
    }
    let message = response.text().await.unwrap_or_default();
    Err(ArcusError::Api { status, message })
}
