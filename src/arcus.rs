//! Arcus REST API adapter.
//!
//! Arcus uses Ed25519 API keys. This module deliberately accepts a signer callback instead of
//! owning private-key material, so applications can keep keys in their existing secret store or
//! hardware-backed signer.

use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

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
