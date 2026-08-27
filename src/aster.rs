//! Aster Pro Futures V3 protocol primitives and REST adapter.
//!
//! Signed requests use the exact form-encoded bytes as the EIP-712 `Message.msg`.
//! Callers retain control of business-parameter ordering; authentication fields are
//! appended in the stable order `nonce`, `signer`, then `signature`.
//! The trading/user-data endpoints implemented here use API_WALLET authentication;
//! `user` is reserved for master-account operations and is intentionally not sent.

use std::cmp::Ordering;
use std::fmt;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use k256::ecdsa::{SigningKey, VerifyingKey};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use sha3::{Digest, Keccak256};
use thiserror::Error;

pub const MAINNET_REST_URL: &str = "https://fapi.asterdex.com";
pub const MAINNET_WS_URL: &str = "wss://fstream.asterdex.com";
pub const EIP712_CHAIN_ID: u64 = 1666;
pub const EIP712_VERIFYING_CONTRACT: &str = "0x0000000000000000000000000000000000000000";
const REST_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub type Params = Vec<(String, String)>;

#[derive(Debug, Error)]
pub enum AsterError {
    #[error("invalid Aster credentials: {0}")]
    Credentials(String),
    #[error("invalid Aster request: {0}")]
    InvalidRequest(String),
    #[error("Aster signing failed: {0}")]
    Signing(String),
    #[error("Aster transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Aster response was invalid: {0}")]
    InvalidResponse(String),
    #[error("Aster rate limit (HTTP 429): {message}")]
    RateLimited { message: String },
    #[error("Aster IP ban (HTTP 418): {message}")]
    IpBanned { message: String },
    #[error("Aster execution status is unknown (HTTP 503): {message}")]
    UnknownExecution { message: String },
    #[error("Aster API error HTTP {status}, code {code:?}: {message}")]
    Api {
        status: u16,
        code: Option<i64>,
        message: String,
    },
}

#[derive(Clone)]
pub struct AsterCredentials {
    signer: String,
    signing_key: SigningKey,
}

impl fmt::Debug for AsterCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AsterCredentials")
            .field("signer", &self.signer)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

impl AsterCredentials {
    pub fn new(signer: &str, private_key: &str) -> Result<Self, AsterError> {
        let signer = validate_address(signer, "signer")?;
        let key_hex = private_key.strip_prefix("0x").ok_or_else(|| {
            AsterError::Credentials("signer private key must start with lowercase 0x".into())
        })?;
        if key_hex.len() != 64 || !key_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(AsterError::Credentials(
                "signer private key must contain exactly 32 hexadecimal bytes".into(),
            ));
        }
        let key_bytes = hex::decode(key_hex)
            .map_err(|_| AsterError::Credentials("invalid signer private key hex".into()))?;
        let signing_key = SigningKey::from_slice(&key_bytes)
            .map_err(|_| AsterError::Credentials("invalid secp256k1 private key".into()))?;
        let derived = ethereum_address(signing_key.verifying_key());
        if derived != signer {
            return Err(AsterError::Credentials(format!(
                "signer address does not match private key (derived {derived})"
            )));
        }
        Ok(Self {
            signer,
            signing_key,
        })
    }

    pub fn signer(&self) -> &str {
        &self.signer
    }

    /// Sign an already computed EIP-712 digest and return `0x` + `r || s || v`.
    pub fn sign_digest(&self, digest: [u8; 32]) -> Result<String, AsterError> {
        let (signature, recovery_id) = self
            .signing_key
            .sign_prehash_recoverable(&digest)
            .map_err(|error| AsterError::Signing(error.to_string()))?;
        let mut encoded = Vec::with_capacity(65);
        encoded.extend_from_slice(&signature.to_bytes());
        encoded.push(27 + recovery_id.to_byte());
        Ok(format!("0x{}", hex::encode(encoded)))
    }

    pub fn sign_message(&self, message: &str) -> Result<String, AsterError> {
        self.sign_digest(eip712_digest(message))
    }
}

fn validate_address(value: &str, label: &str) -> Result<String, AsterError> {
    let digits = value.strip_prefix("0x").ok_or_else(|| {
        AsterError::Credentials(format!("{label} address must start with lowercase 0x"))
    })?;
    if digits.len() != 40 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AsterError::Credentials(format!(
            "{label} address must contain exactly 20 hexadecimal bytes"
        )));
    }
    Ok(format!("0x{}", digits.to_ascii_lowercase()))
}

pub fn ethereum_address(key: &VerifyingKey) -> String {
    let encoded = key.to_encoded_point(false);
    let hash = Keccak256::digest(&encoded.as_bytes()[1..]);
    format!("0x{}", hex::encode(&hash[12..]))
}

fn keccak(input: impl AsRef<[u8]>) -> [u8; 32] {
    Keccak256::digest(input).into()
}

fn uint256_word(value: u64) -> [u8; 32] {
    let mut word = [0_u8; 32];
    word[24..].copy_from_slice(&value.to_be_bytes());
    word
}

/// Compute the Aster mainnet EIP-712 digest for `Message(string msg)`.
pub fn eip712_digest(message: &str) -> [u8; 32] {
    let domain_type = keccak(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut domain = Vec::with_capacity(160);
    domain.extend_from_slice(&domain_type);
    domain.extend_from_slice(&keccak("AsterSignTransaction"));
    domain.extend_from_slice(&keccak("1"));
    domain.extend_from_slice(&uint256_word(EIP712_CHAIN_ID));
    domain.extend_from_slice(&[0_u8; 32]);
    let domain_separator = keccak(domain);

    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(&keccak("Message(string msg)"));
    body.extend_from_slice(&keccak(message));
    let message_hash = keccak(body);

    let mut signable = Vec::with_capacity(66);
    signable.extend_from_slice(&[0x19, 0x01]);
    signable.extend_from_slice(&domain_separator);
    signable.extend_from_slice(&message_hash);
    keccak(signable)
}

#[derive(Debug)]
pub struct AsterNonce {
    last: AtomicU64,
    server_offset_micros: Arc<AtomicI64>,
}

impl Default for AsterNonce {
    fn default() -> Self {
        Self::new()
    }
}

impl AsterNonce {
    pub fn new() -> Self {
        Self::with_offset(Arc::new(AtomicI64::new(0)))
    }

    fn with_offset(server_offset_micros: Arc<AtomicI64>) -> Self {
        Self {
            last: AtomicU64::new(0),
            server_offset_micros,
        }
    }

    pub fn next(&self) -> u64 {
        let now = unix_micros();
        let offset = self.server_offset_micros.load(AtomicOrdering::Relaxed);
        let adjusted = if offset >= 0 {
            now.saturating_add(offset as u64)
        } else {
            now.saturating_sub(offset.unsigned_abs())
        };
        let mut previous = self.last.load(AtomicOrdering::Relaxed);
        loop {
            let candidate = adjusted.max(previous.saturating_add(1));
            match self.last.compare_exchange_weak(
                previous,
                candidate,
                AtomicOrdering::SeqCst,
                AtomicOrdering::Relaxed,
            ) {
                Ok(_) => return candidate,
                Err(actual) => previous = actual,
            }
        }
    }
}

fn unix_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()
        .min(u64::MAX as u128) as u64
}

/// Encode fields exactly as `application/x-www-form-urlencoded`, retaining input order.
pub fn form_encode<I, K, V>(params: I) -> String
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    params
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                encode_form_component(key.as_ref()),
                encode_form_component(value.as_ref())
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn encode_form_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            b' ' => encoded.push('+'),
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                encoded.push('%');
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
    encoded
}

#[derive(Clone)]
pub struct AsterClient {
    http: reqwest::Client,
    rest_base: String,
    websocket_base: String,
    credentials: Option<AsterCredentials>,
    nonce: Arc<AsterNonce>,
    server_offset_micros: Arc<AtomicI64>,
}

impl AsterClient {
    pub fn public() -> Self {
        Self::with_base_urls(MAINNET_REST_URL, MAINNET_WS_URL)
    }

    pub fn with_base_url(rest_base: &str) -> Self {
        Self::with_base_urls(rest_base, MAINNET_WS_URL)
    }

    pub fn with_base_urls(rest_base: &str, websocket_base: &str) -> Self {
        let server_offset_micros = Arc::new(AtomicI64::new(0));
        Self {
            http: reqwest::Client::new(),
            rest_base: rest_base.trim_end_matches('/').to_owned(),
            websocket_base: websocket_base.trim_end_matches('/').to_owned(),
            credentials: None,
            nonce: Arc::new(AsterNonce::with_offset(server_offset_micros.clone())),
            server_offset_micros,
        }
    }

    pub fn authenticated(credentials: AsterCredentials) -> Self {
        Self::authenticated_with_base_urls(credentials, MAINNET_REST_URL, MAINNET_WS_URL)
    }

    pub fn authenticated_with_base_url(credentials: AsterCredentials, rest_base: &str) -> Self {
        Self::authenticated_with_base_urls(credentials, rest_base, MAINNET_WS_URL)
    }

    pub fn authenticated_with_base_urls(
        credentials: AsterCredentials,
        rest_base: &str,
        websocket_base: &str,
    ) -> Self {
        let mut client = Self::with_base_urls(rest_base, websocket_base);
        client.credentials = Some(credentials);
        client
    }

    pub fn server_offset_micros(&self) -> i64 {
        self.server_offset_micros.load(AtomicOrdering::Relaxed)
    }

    /// Synchronize the nonce clock using the midpoint of the local request interval.
    pub async fn sync_server_time(&self) -> Result<i64, AsterError> {
        let before = unix_micros();
        let time: ServerTime = self
            .public_request(Method::GET, "/fapi/v3/time", Vec::new())
            .await?;
        let after = unix_micros();
        let midpoint = before / 2 + after / 2 + (before % 2 + after % 2) / 2;
        let server = time.server_time.saturating_mul(1_000);
        let offset = i128::from(server) - i128::from(midpoint);
        let offset = offset.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
        self.server_offset_micros
            .store(offset, AtomicOrdering::Relaxed);
        Ok(offset)
    }

    pub async fn exchange_info(&self) -> Result<ExchangeInfo, AsterError> {
        self.public_request(Method::GET, "/fapi/v3/exchangeInfo", Vec::new())
            .await
    }

    pub async fn klines(
        &self,
        symbol: &str,
        interval: &str,
        start_time: Option<u64>,
        end_time: Option<u64>,
        limit: Option<u16>,
    ) -> Result<Vec<Kline>, AsterError> {
        let mut params = vec![
            ("symbol".into(), symbol.into()),
            ("interval".into(), interval.into()),
        ];
        push_option(&mut params, "startTime", start_time);
        push_option(&mut params, "endTime", end_time);
        push_option(&mut params, "limit", limit);
        self.public_request(Method::GET, "/fapi/v3/klines", params)
            .await
    }

    pub async fn place_order(&self, request: &NewOrderRequest) -> Result<Order, AsterError> {
        self.signed_post("/fapi/v3/order", request.params()).await
    }

    pub async fn modify_order(&self, request: &ModifyOrderRequest) -> Result<Order, AsterError> {
        self.signed_put("/fapi/v3/order", request.params()).await
    }

    pub async fn query_order(
        &self,
        symbol: &str,
        order_id: Option<u64>,
        client_order_id: Option<&str>,
    ) -> Result<Order, AsterError> {
        let params = order_target_params(symbol, order_id, client_order_id)?;
        self.signed_get("/fapi/v3/order", params).await
    }

    pub async fn cancel_order(
        &self,
        symbol: &str,
        order_id: Option<u64>,
        client_order_id: Option<&str>,
    ) -> Result<Order, AsterError> {
        let params = order_target_params(symbol, order_id, client_order_id)?;
        self.signed_delete("/fapi/v3/order", params).await
    }

    pub async fn cancel_all_orders(&self, symbol: &str) -> Result<ApiAcknowledgement, AsterError> {
        self.signed_delete(
            "/fapi/v3/allOpenOrders",
            vec![("symbol".into(), symbol.into())],
        )
        .await
    }

    pub async fn countdown_cancel_all(
        &self,
        symbol: &str,
        countdown_time_ms: u64,
    ) -> Result<CountdownResponse, AsterError> {
        self.signed_post(
            "/fapi/v3/countdownCancelAll",
            vec![
                ("symbol".into(), symbol.into()),
                ("countdownTime".into(), countdown_time_ms.to_string()),
            ],
        )
        .await
    }

    pub async fn open_orders(&self, symbol: Option<&str>) -> Result<Vec<Order>, AsterError> {
        let params = symbol
            .map(|symbol| vec![("symbol".into(), symbol.into())])
            .unwrap_or_default();
        self.signed_get("/fapi/v3/openOrders", params).await
    }

    pub async fn account_with_join_margin(&self) -> Result<Account, AsterError> {
        self.signed_get("/fapi/v3/accountWithJoinMargin", Vec::new())
            .await
    }

    pub async fn position_risk(
        &self,
        symbol: Option<&str>,
    ) -> Result<Vec<PositionRisk>, AsterError> {
        let params = symbol
            .map(|symbol| vec![("symbol".into(), symbol.into())])
            .unwrap_or_default();
        self.signed_get("/fapi/v3/positionRisk", params).await
    }

    pub async fn position_mode(&self) -> Result<PositionMode, AsterError> {
        self.signed_get("/fapi/v3/positionSide/dual", Vec::new())
            .await
    }

    pub async fn set_position_mode(
        &self,
        dual_side_position: bool,
    ) -> Result<ApiAcknowledgement, AsterError> {
        self.signed_post(
            "/fapi/v3/positionSide/dual",
            vec![("dualSidePosition".into(), dual_side_position.to_string())],
        )
        .await
    }

    pub async fn user_trades(&self, query: &UserTradesQuery) -> Result<Vec<UserTrade>, AsterError> {
        self.signed_get("/fapi/v3/userTrades", query.params()).await
    }

    pub async fn income(&self, query: &IncomeQuery) -> Result<Vec<Income>, AsterError> {
        self.signed_get("/fapi/v3/income", query.params()).await
    }

    pub async fn create_listen_key(&self) -> Result<ListenKey, AsterError> {
        self.signed_post("/fapi/v3/listenKey", Vec::new()).await
    }

    pub async fn keepalive_listen_key(&self) -> Result<ApiAcknowledgement, AsterError> {
        self.signed_put("/fapi/v3/listenKey", Vec::new()).await
    }

    pub async fn close_listen_key(&self) -> Result<ApiAcknowledgement, AsterError> {
        self.signed_delete("/fapi/v3/listenKey", Vec::new()).await
    }

    pub fn market_stream_url(&self, stream: &str) -> Result<String, AsterError> {
        if stream.is_empty()
            || !stream.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'!' | b'_' | b'-' | b'.')
            })
        {
            return Err(AsterError::InvalidRequest(
                "invalid websocket stream name".into(),
            ));
        }
        Ok(format!("{}/ws/{stream}", self.websocket_base))
    }

    pub fn book_ticker_url(&self, symbol: &str) -> Result<String, AsterError> {
        self.market_stream_url(&format!("{}@bookTicker", symbol.to_ascii_lowercase()))
    }

    pub fn user_stream_url(&self, listen_key: &str) -> Result<String, AsterError> {
        if listen_key.is_empty()
            || !listen_key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(AsterError::InvalidRequest("invalid listen key".into()));
        }
        Ok(format!("{}/ws/{listen_key}", self.websocket_base))
    }

    pub async fn signed_get<T: DeserializeOwned>(
        &self,
        path: &str,
        params: Params,
    ) -> Result<T, AsterError> {
        self.signed_request(Method::GET, path, params).await
    }

    pub async fn signed_post<T: DeserializeOwned>(
        &self,
        path: &str,
        params: Params,
    ) -> Result<T, AsterError> {
        self.signed_request(Method::POST, path, params).await
    }

    pub async fn signed_put<T: DeserializeOwned>(
        &self,
        path: &str,
        params: Params,
    ) -> Result<T, AsterError> {
        self.signed_request(Method::PUT, path, params).await
    }

    pub async fn signed_delete<T: DeserializeOwned>(
        &self,
        path: &str,
        params: Params,
    ) -> Result<T, AsterError> {
        self.signed_request(Method::DELETE, path, params).await
    }

    async fn public_request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        params: Params,
    ) -> Result<T, AsterError> {
        let encoded = form_encode(
            params
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        let url = if encoded.is_empty() {
            format!("{}{}", self.rest_base, path)
        } else {
            format!("{}{}?{}", self.rest_base, path, encoded)
        };
        let response = self
            .http
            .request(method, url)
            .timeout(REST_REQUEST_TIMEOUT)
            .send()
            .await?;
        decode_response(response).await
    }

    async fn signed_request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        mut params: Params,
    ) -> Result<T, AsterError> {
        if !path.starts_with("/fapi/v3/") {
            return Err(AsterError::InvalidRequest(
                "signed path must be under /fapi/v3/".into(),
            ));
        }
        let credentials = self.credentials.as_ref().ok_or_else(|| {
            AsterError::InvalidRequest("authenticated Aster client required".into())
        })?;
        params.push(("nonce".into(), self.nonce.next().to_string()));
        params.push(("signer".into(), credentials.signer().into()));
        let signable = form_encode(
            params
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        let signature = credentials.sign_message(&signable)?;
        let encoded = format!(
            "{}&signature={}",
            signable,
            encode_form_component(&signature)
        );
        let url = format!("{}{}", self.rest_base, path);
        let request = if method == Method::GET {
            self.http.request(method, format!("{url}?{encoded}"))
        } else {
            self.http
                .request(method, url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded",
                )
                .body(encoded)
        }
        .timeout(REST_REQUEST_TIMEOUT);
        let response = request.send().await?;
        decode_response(response).await
    }
}

fn push_option<T: ToString>(params: &mut Params, name: &str, value: Option<T>) {
    if let Some(value) = value {
        params.push((name.into(), value.to_string()));
    }
}

fn order_target_params(
    symbol: &str,
    order_id: Option<u64>,
    client_order_id: Option<&str>,
) -> Result<Params, AsterError> {
    if order_id.is_none() && client_order_id.is_none() {
        return Err(AsterError::InvalidRequest(
            "orderId or origClientOrderId is required".into(),
        ));
    }
    let mut params = vec![("symbol".into(), symbol.into())];
    push_option(&mut params, "orderId", order_id);
    if let Some(client_order_id) = client_order_id {
        params.push(("origClientOrderId".into(), client_order_id.into()));
    }
    Ok(params)
}

async fn decode_response<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, AsterError> {
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(decode_api_error(status.as_u16(), &body));
    }
    serde_json::from_str(&body)
        .map_err(|error| AsterError::InvalidResponse(format!("{error}; response body: {body}")))
}

/// Classify an HTTP error without retrying. In particular, HTTP 503 is ambiguous.
pub fn decode_api_error(status: u16, body: &str) -> AsterError {
    let parsed: Option<ApiErrorBody> = serde_json::from_str(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|error| error.msg.clone())
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| body.to_owned());
    match StatusCode::from_u16(status).ok() {
        Some(StatusCode::TOO_MANY_REQUESTS) => AsterError::RateLimited { message },
        Some(StatusCode::IM_A_TEAPOT) => AsterError::IpBanned { message },
        Some(StatusCode::SERVICE_UNAVAILABLE) => AsterError::UnknownExecution { message },
        _ => AsterError::Api {
            status,
            code: parsed.and_then(|error| error.code),
            message,
        },
    }
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    code: Option<i64>,
    msg: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerTime {
    pub server_time: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeInfo {
    pub timezone: String,
    pub server_time: u64,
    #[serde(default)]
    pub rate_limits: Vec<serde_json::Value>,
    pub symbols: Vec<ExchangeSymbol>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExchangeSymbol {
    pub symbol: String,
    pub pair: Option<String>,
    pub contract_type: String,
    pub status: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub price_precision: u32,
    pub quantity_precision: u32,
    pub filters: Vec<SymbolFilter>,
    #[serde(default)]
    pub order_types: Vec<String>,
    #[serde(default)]
    pub time_in_force: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolFilter {
    pub filter_type: String,
    pub min_price: Option<String>,
    pub max_price: Option<String>,
    pub tick_size: Option<String>,
    pub min_qty: Option<String>,
    pub max_qty: Option<String>,
    pub step_size: Option<String>,
    #[serde(alias = "notioanl")]
    pub notional: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Decimal {
    coefficient: u128,
    scale: u32,
}

impl Decimal {
    fn parse(value: &str) -> Result<Self, AsterError> {
        if value.is_empty() || value.starts_with(['-', '+']) {
            return Err(AsterError::InvalidRequest(format!(
                "invalid unsigned decimal: {value}"
            )));
        }
        let mut parts = value.split('.');
        let whole = parts.next().unwrap_or_default();
        let fraction = parts.next().unwrap_or_default();
        if parts.next().is_some()
            || whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.len() > 18
        {
            return Err(AsterError::InvalidRequest(format!(
                "invalid unsigned decimal: {value}"
            )));
        }
        let coefficient = format!("{whole}{fraction}")
            .parse()
            .map_err(|_| AsterError::InvalidRequest("decimal overflow".into()))?;
        Ok(Self {
            coefficient,
            scale: fraction.len() as u32,
        })
    }

    fn positive(value: &str, label: &str) -> Result<Self, AsterError> {
        let decimal = Self::parse(value)?;
        if decimal.coefficient == 0 {
            return Err(AsterError::InvalidRequest(format!(
                "{label} must be positive"
            )));
        }
        Ok(decimal)
    }

    fn quantize_from(
        &self,
        origin: &Self,
        grid: &Self,
        round_up: bool,
    ) -> Result<Self, AsterError> {
        if grid.coefficient == 0 {
            return Err(AsterError::InvalidRequest(
                "market grid must be positive".into(),
            ));
        }
        let scale = self.scale.max(grid.scale).max(origin.scale);
        let value = scale_coefficient(self.coefficient, scale - self.scale)?;
        let origin = scale_coefficient(origin.coefficient, scale - origin.scale)?;
        let step = scale_coefficient(grid.coefficient, scale - grid.scale)?;
        if value < origin {
            return Ok(if round_up {
                Self {
                    coefficient: origin,
                    scale,
                }
            } else {
                Self {
                    coefficient: 0,
                    scale,
                }
            });
        }
        let delta = value - origin;
        let mut units = delta / step;
        if round_up && delta % step != 0 {
            units = units
                .checked_add(1)
                .ok_or_else(|| AsterError::InvalidRequest("decimal overflow".into()))?;
        }
        let coefficient = step
            .checked_mul(units)
            .and_then(|offset| origin.checked_add(offset))
            .ok_or_else(|| AsterError::InvalidRequest("decimal overflow".into()))?;
        Ok(Self { coefficient, scale })
    }

    fn cmp_value(&self, other: &Self) -> Result<Ordering, AsterError> {
        let scale = self.scale.max(other.scale);
        Ok(scale_coefficient(self.coefficient, scale - self.scale)?
            .cmp(&scale_coefficient(other.coefficient, scale - other.scale)?))
    }

    fn display(&self) -> String {
        if self.scale == 0 {
            return self.coefficient.to_string();
        }
        let mut digits = format!(
            "{:0width$}",
            self.coefficient,
            width = self.scale as usize + 1
        );
        digits.insert(digits.len() - self.scale as usize, '.');
        while digits.ends_with('0') {
            digits.pop();
        }
        if digits.ends_with('.') {
            digits.pop();
        }
        digits
    }
}

fn scale_coefficient(value: u128, scale: u32) -> Result<u128, AsterError> {
    value
        .checked_mul(
            10_u128
                .checked_pow(scale)
                .ok_or_else(|| AsterError::InvalidRequest("decimal precision overflow".into()))?,
        )
        .ok_or_else(|| AsterError::InvalidRequest("decimal overflow".into()))
}

#[derive(Debug, Clone)]
pub struct AsterMarket {
    pub symbol: String,
    tick_size: Decimal,
    min_price: Decimal,
    max_price: Decimal,
    step_size: Decimal,
    min_quantity: Decimal,
    max_quantity: Decimal,
    min_notional: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantizedOrder {
    pub price: String,
    pub quantity: String,
}

impl TryFrom<&ExchangeSymbol> for AsterMarket {
    type Error = AsterError;

    fn try_from(symbol: &ExchangeSymbol) -> Result<Self, Self::Error> {
        let filter = |kind: &str| {
            symbol
                .filters
                .iter()
                .find(|filter| filter.filter_type == kind)
                .ok_or_else(|| {
                    AsterError::InvalidResponse(format!("{} is missing {kind}", symbol.symbol))
                })
        };
        let price = filter("PRICE_FILTER")?;
        let lot = filter("LOT_SIZE")?;
        let notional = filter("MIN_NOTIONAL")?;
        Ok(Self {
            symbol: symbol.symbol.clone(),
            tick_size: Decimal::positive(
                price
                    .tick_size
                    .as_deref()
                    .ok_or_else(|| missing_filter(symbol, "tickSize"))?,
                "tickSize",
            )?,
            min_price: Decimal::parse(
                price
                    .min_price
                    .as_deref()
                    .ok_or_else(|| missing_filter(symbol, "minPrice"))?,
            )?,
            max_price: Decimal::parse(
                price
                    .max_price
                    .as_deref()
                    .ok_or_else(|| missing_filter(symbol, "maxPrice"))?,
            )?,
            step_size: Decimal::positive(
                lot.step_size
                    .as_deref()
                    .ok_or_else(|| missing_filter(symbol, "stepSize"))?,
                "stepSize",
            )?,
            min_quantity: Decimal::parse(
                lot.min_qty
                    .as_deref()
                    .ok_or_else(|| missing_filter(symbol, "minQty"))?,
            )?,
            max_quantity: Decimal::parse(
                lot.max_qty
                    .as_deref()
                    .ok_or_else(|| missing_filter(symbol, "maxQty"))?,
            )?,
            min_notional: Decimal::parse(
                notional
                    .notional
                    .as_deref()
                    .ok_or_else(|| missing_filter(symbol, "notional"))?,
            )?,
        })
    }
}

fn missing_filter(symbol: &ExchangeSymbol, field: &str) -> AsterError {
    AsterError::InvalidResponse(format!("{} is missing {field}", symbol.symbol))
}

impl AsterMarket {
    pub fn tick_size_value(&self) -> Result<f64, AsterError> {
        self.tick_size
            .display()
            .parse::<f64>()
            .map_err(|_| AsterError::InvalidResponse("invalid market tickSize".into()))
    }

    pub fn quantize_maker(
        &self,
        price: &str,
        quantity: &str,
        side: OrderSide,
    ) -> Result<QuantizedOrder, AsterError> {
        self.quantize_limit(price, quantity, side, true)
    }

    /// Quantize a reduce-only limit. Aster permits reduce-only orders below
    /// `MIN_NOTIONAL`, which is required to close small residual positions.
    pub fn quantize_reduce_only(
        &self,
        price: &str,
        quantity: &str,
        price_rounding_side: OrderSide,
    ) -> Result<QuantizedOrder, AsterError> {
        self.quantize_limit(price, quantity, price_rounding_side, false)
    }

    fn quantize_limit(
        &self,
        price: &str,
        quantity: &str,
        price_rounding_side: OrderSide,
        enforce_min_notional: bool,
    ) -> Result<QuantizedOrder, AsterError> {
        let price = Decimal::positive(price, "price")?.quantize_from(
            &self.min_price,
            &self.tick_size,
            price_rounding_side == OrderSide::Sell,
        )?;
        let quantity = Decimal::positive(quantity, "quantity")?.quantize_from(
            &self.min_quantity,
            &self.step_size,
            false,
        )?;
        if price.coefficient == 0 || quantity.coefficient == 0 {
            return Err(AsterError::InvalidRequest(
                "quantized price and quantity must be positive".into(),
            ));
        }
        validate_range(&price, &self.min_price, &self.max_price, "price")?;
        validate_range(
            &quantity,
            &self.min_quantity,
            &self.max_quantity,
            "quantity",
        )?;
        let notional_coefficient = price
            .coefficient
            .checked_mul(quantity.coefficient)
            .ok_or_else(|| AsterError::InvalidRequest("notional overflow".into()))?;
        let notional = Decimal {
            coefficient: notional_coefficient,
            scale: price.scale + quantity.scale,
        };
        if enforce_min_notional && notional.cmp_value(&self.min_notional)? == Ordering::Less {
            return Err(AsterError::InvalidRequest(format!(
                "order notional {} is below minimum {}",
                notional.display(),
                self.min_notional.display()
            )));
        }
        Ok(QuantizedOrder {
            price: price.display(),
            quantity: quantity.display(),
        })
    }
}

fn validate_range(
    value: &Decimal,
    minimum: &Decimal,
    maximum: &Decimal,
    label: &str,
) -> Result<(), AsterError> {
    if minimum.coefficient != 0 && value.cmp_value(minimum)? == Ordering::Less {
        return Err(AsterError::InvalidRequest(format!(
            "{label} is below market minimum"
        )));
    }
    if maximum.coefficient != 0 && value.cmp_value(maximum)? == Ordering::Greater {
        return Err(AsterError::InvalidRequest(format!(
            "{label} is above market maximum"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewOrderRequest {
    pub symbol: String,
    pub side: OrderSide,
    pub order_type: String,
    pub position_side: Option<String>,
    pub time_in_force: Option<String>,
    pub quantity: Option<String>,
    pub reduce_only: Option<bool>,
    pub price: Option<String>,
    pub client_order_id: Option<String>,
    pub response_type: Option<String>,
}

impl NewOrderRequest {
    pub fn maker_limit(
        symbol: impl Into<String>,
        side: OrderSide,
        price: impl Into<String>,
        quantity: impl Into<String>,
        client_order_id: Option<String>,
    ) -> Self {
        Self {
            symbol: symbol.into(),
            side,
            order_type: "LIMIT".into(),
            position_side: None,
            time_in_force: Some("GTX".into()),
            quantity: Some(quantity.into()),
            reduce_only: None,
            price: Some(price.into()),
            client_order_id,
            response_type: Some("ACK".into()),
        }
    }

    pub fn params(&self) -> Params {
        let mut params = vec![
            ("symbol".into(), self.symbol.clone()),
            ("side".into(), self.side.as_str().into()),
            ("type".into(), self.order_type.clone()),
        ];
        push_string(&mut params, "positionSide", &self.position_side);
        push_string(&mut params, "timeInForce", &self.time_in_force);
        push_string(&mut params, "quantity", &self.quantity);
        if let Some(reduce_only) = self.reduce_only {
            params.push(("reduceOnly".into(), reduce_only.to_string()));
        }
        push_string(&mut params, "price", &self.price);
        push_string(&mut params, "newClientOrderId", &self.client_order_id);
        push_string(&mut params, "newOrderRespType", &self.response_type);
        params
    }
}

#[derive(Debug, Clone)]
pub struct ModifyOrderRequest {
    pub symbol: String,
    pub order_id: Option<u64>,
    pub client_order_id: Option<String>,
    pub price: String,
    pub quantity: String,
}

impl ModifyOrderRequest {
    pub fn new(
        symbol: impl Into<String>,
        order_id: Option<u64>,
        client_order_id: Option<String>,
        price: impl Into<String>,
        quantity: impl Into<String>,
    ) -> Result<Self, AsterError> {
        if order_id.is_none() && client_order_id.is_none() {
            return Err(AsterError::InvalidRequest(
                "orderId or origClientOrderId is required for modification".into(),
            ));
        }
        let price = price.into();
        let quantity = quantity.into();
        Decimal::positive(&price, "modify price")?;
        Decimal::positive(&quantity, "modify quantity")?;
        Ok(Self {
            symbol: symbol.into(),
            order_id,
            client_order_id,
            price,
            quantity,
        })
    }

    pub fn params(&self) -> Params {
        let mut params = vec![("symbol".into(), self.symbol.clone())];
        push_option(&mut params, "orderId", self.order_id);
        if let Some(client_order_id) = &self.client_order_id {
            params.push(("origClientOrderId".into(), client_order_id.clone()));
        }
        params.push(("quantity".into(), self.quantity.clone()));
        params.push(("price".into(), self.price.clone()));
        params
    }
}

fn push_string(params: &mut Params, name: &str, value: &Option<String>) {
    if let Some(value) = value {
        params.push((name.into(), value.clone()));
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub symbol: String,
    pub order_id: u64,
    pub client_order_id: String,
    #[serde(default)]
    pub price: String,
    #[serde(default)]
    pub avg_price: String,
    #[serde(default)]
    pub orig_qty: String,
    #[serde(default)]
    pub executed_qty: String,
    #[serde(default)]
    pub cum_qty: String,
    #[serde(default)]
    pub cum_quote: String,
    #[serde(default = "new_order_status")]
    pub status: String,
    #[serde(default)]
    pub time_in_force: String,
    #[serde(rename = "type", default)]
    pub order_type: String,
    #[serde(default)]
    pub side: String,
    #[serde(default)]
    pub position_side: String,
    #[serde(default)]
    pub reduce_only: bool,
    #[serde(default)]
    pub close_position: bool,
    #[serde(default)]
    pub stop_price: String,
    #[serde(default)]
    pub update_time: u64,
    #[serde(default)]
    pub working_type: String,
    #[serde(default)]
    pub price_protect: bool,
}

fn new_order_status() -> String {
    "NEW".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiAcknowledgement {
    #[serde(default)]
    pub code: serde_json::Value,
    #[serde(default)]
    pub msg: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountdownResponse {
    pub symbol: String,
    #[serde(deserialize_with = "deserialize_u64_or_string")]
    pub countdown_time: u64,
}

fn deserialize_u64_or_string<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("expected an unsigned integer")),
        serde_json::Value::String(text) => text
            .parse::<u64>()
            .map_err(|_| serde::de::Error::custom("expected an unsigned integer string")),
        _ => Err(serde::de::Error::custom(
            "expected an unsigned integer or string",
        )),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    #[serde(default)]
    pub fee_tier: i32,
    pub can_trade: bool,
    pub can_deposit: bool,
    pub can_withdraw: bool,
    #[serde(default)]
    pub update_time: u64,
    pub total_wallet_balance: String,
    pub total_unrealized_profit: String,
    pub total_margin_balance: String,
    pub available_balance: String,
    pub max_withdraw_amount: String,
    #[serde(default)]
    pub assets: Vec<AccountAsset>,
    #[serde(default)]
    pub positions: Vec<AccountPosition>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountAsset {
    pub asset: String,
    pub wallet_balance: String,
    pub unrealized_profit: String,
    pub margin_balance: String,
    pub available_balance: String,
    #[serde(default)]
    pub max_withdraw_amount: String,
    #[serde(default)]
    pub cross_wallet_balance: String,
    #[serde(default)]
    pub cross_un_pnl: String,
    #[serde(default)]
    pub margin_available: bool,
    #[serde(default)]
    pub update_time: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountPosition {
    pub symbol: String,
    pub position_amt: String,
    pub entry_price: String,
    pub unrealized_profit: String,
    pub leverage: String,
    pub isolated: bool,
    pub position_side: String,
    #[serde(default)]
    pub initial_margin: String,
    #[serde(default)]
    pub maint_margin: String,
    #[serde(default)]
    pub update_time: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionRisk {
    pub symbol: String,
    pub position_amt: String,
    pub entry_price: String,
    pub mark_price: String,
    pub un_realized_profit: String,
    pub liquidation_price: String,
    pub leverage: String,
    pub margin_type: String,
    pub position_side: String,
    #[serde(default)]
    pub update_time: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionMode {
    pub dual_side_position: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UserTradesQuery {
    pub symbol: String,
    pub start_time: Option<u64>,
    pub end_time: Option<u64>,
    pub from_id: Option<u64>,
    pub limit: Option<u16>,
}

impl UserTradesQuery {
    pub fn params(&self) -> Params {
        let mut params = vec![("symbol".into(), self.symbol.clone())];
        if let Some(from_id) = self.from_id {
            params.push(("fromId".into(), from_id.to_string()));
        } else {
            push_option(&mut params, "startTime", self.start_time);
            push_option(&mut params, "endTime", self.end_time);
        }
        push_option(&mut params, "limit", self.limit);
        params
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserTrade {
    pub symbol: String,
    pub id: u64,
    pub order_id: u64,
    pub price: String,
    pub qty: String,
    pub quote_qty: String,
    pub commission: String,
    pub commission_asset: String,
    pub realized_pnl: String,
    pub side: String,
    pub position_side: String,
    pub time: u64,
    pub buyer: bool,
    pub maker: bool,
}

#[derive(Debug, Clone, Default)]
pub struct IncomeQuery {
    pub symbol: Option<String>,
    pub income_type: Option<String>,
    pub start_time: Option<u64>,
    pub end_time: Option<u64>,
    pub limit: Option<u16>,
}

impl IncomeQuery {
    pub fn params(&self) -> Params {
        let mut params = Vec::new();
        push_string(&mut params, "symbol", &self.symbol);
        push_string(&mut params, "incomeType", &self.income_type);
        push_option(&mut params, "startTime", self.start_time);
        push_option(&mut params, "endTime", self.end_time);
        push_option(&mut params, "limit", self.limit);
        params
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Income {
    pub symbol: String,
    pub income_type: String,
    pub income: String,
    pub asset: String,
    pub info: String,
    pub time: u64,
    #[serde(deserialize_with = "deserialize_string_or_number")]
    pub tran_id: String,
    #[serde(deserialize_with = "deserialize_string_or_number")]
    pub trade_id: String,
}

fn deserialize_string_or_number<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(text) => Ok(text),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        serde_json::Value::Null => Ok(String::new()),
        _ => Err(serde::de::Error::custom("expected a string or number")),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenKey {
    pub listen_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Kline {
    pub open_time: u64,
    pub open: String,
    pub high: String,
    pub low: String,
    pub close: String,
    pub volume: String,
    pub close_time: u64,
    pub quote_volume: String,
    pub trades: u64,
}

impl<'de> Deserialize<'de> for Kline {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
        let number = |index: usize| {
            values
                .get(index)
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| serde::de::Error::custom(format!("invalid kline field {index}")))
        };
        let string = |index: usize| {
            values
                .get(index)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| serde::de::Error::custom(format!("invalid kline field {index}")))
        };
        Ok(Self {
            open_time: number(0)?,
            open: string(1)?,
            high: string(2)?,
            low: string(3)?,
            close: string(4)?,
            volume: string(5)?,
            close_time: number(6)?,
            quote_volume: string(7)?,
            trades: number(8)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AsterWsEvent {
    Bbo(BookTicker),
    Depth(Box<DepthUpdate>),
    Order(Box<OrderTradeUpdate>),
    Account(Box<AccountUpdate>),
    ListenKeyExpired { event_time: u64 },
    Ignored,
}

impl AsterWsEvent {
    pub fn parse(text: &str) -> Result<Self, AsterError> {
        let mut value: serde_json::Value = serde_json::from_str(text)
            .map_err(|error| AsterError::InvalidResponse(error.to_string()))?;
        if let Some(data) = value.get_mut("data") {
            value = data.take();
        }
        let event_type = value
            .get("e")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        match event_type {
            "bookTicker" => from_ws_value(value).map(Self::Bbo),
            "depthUpdate" => from_ws_value(value).map(|event| Self::Depth(Box::new(event))),
            "ORDER_TRADE_UPDATE" => from_ws_value(value).map(|event| Self::Order(Box::new(event))),
            "ACCOUNT_UPDATE" => {
                let envelope: AccountUpdateEnvelope = from_ws_value(value)?;
                Ok(Self::Account(Box::new(AccountUpdate {
                    event_time: envelope.event_time,
                    transaction_time: envelope.transaction_time,
                    reason: envelope.account.reason,
                    balances: envelope.account.balances,
                    positions: envelope.account.positions,
                })))
            }
            "listenKeyExpired" => {
                let event_time = value
                    .get("E")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        AsterError::InvalidResponse("listenKeyExpired is missing event time".into())
                    })?;
                Ok(Self::ListenKeyExpired { event_time })
            }
            _ => Ok(Self::Ignored),
        }
    }
}

fn from_ws_value<T: DeserializeOwned>(value: serde_json::Value) -> Result<T, AsterError> {
    serde_json::from_value(value).map_err(|error| AsterError::InvalidResponse(error.to_string()))
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BookTicker {
    #[serde(rename = "u")]
    pub update_id: u64,
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "T")]
    pub transaction_time: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "b")]
    pub bid_price: String,
    #[serde(rename = "B")]
    pub bid_quantity: String,
    #[serde(rename = "a")]
    pub ask_price: String,
    #[serde(rename = "A")]
    pub ask_quantity: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DepthUpdate {
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "T")]
    pub transaction_time: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "U")]
    pub first_update_id: u64,
    #[serde(rename = "u")]
    pub final_update_id: u64,
    #[serde(rename = "pu")]
    pub previous_update_id: u64,
    #[serde(rename = "b", default)]
    pub bids: Vec<(String, String)>,
    #[serde(rename = "a", default)]
    pub asks: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OrderTradeUpdate {
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "T")]
    pub transaction_time: u64,
    #[serde(rename = "o")]
    pub order: OrderUpdate,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OrderUpdate {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "c")]
    pub client_order_id: String,
    #[serde(rename = "S")]
    pub side: String,
    #[serde(rename = "o")]
    pub order_type: String,
    #[serde(rename = "f")]
    pub time_in_force: String,
    #[serde(rename = "q")]
    pub original_quantity: String,
    #[serde(rename = "p")]
    pub original_price: String,
    #[serde(rename = "ap")]
    pub average_price: String,
    #[serde(rename = "x")]
    pub execution_type: String,
    #[serde(rename = "X")]
    pub status: String,
    #[serde(rename = "i")]
    pub order_id: u64,
    #[serde(rename = "l")]
    pub last_filled_quantity: String,
    #[serde(rename = "z")]
    pub accumulated_filled_quantity: String,
    #[serde(rename = "L")]
    pub last_filled_price: String,
    #[serde(rename = "n", default)]
    pub commission: Option<String>,
    #[serde(rename = "N", default)]
    pub commission_asset: Option<String>,
    #[serde(rename = "T")]
    pub trade_time: u64,
    #[serde(rename = "t", default)]
    pub trade_id: i64,
    #[serde(rename = "m")]
    pub maker: bool,
    #[serde(rename = "R")]
    pub reduce_only: bool,
    #[serde(rename = "ps")]
    pub position_side: String,
    #[serde(rename = "rp", default)]
    pub realized_profit: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountUpdate {
    pub event_time: u64,
    pub transaction_time: u64,
    pub reason: String,
    pub balances: Vec<WsBalance>,
    pub positions: Vec<WsPosition>,
}

#[derive(Debug, Deserialize)]
struct AccountUpdateEnvelope {
    #[serde(rename = "E")]
    event_time: u64,
    #[serde(rename = "T")]
    transaction_time: u64,
    #[serde(rename = "a")]
    account: AccountUpdateBody,
}

#[derive(Debug, Deserialize)]
struct AccountUpdateBody {
    #[serde(rename = "m")]
    reason: String,
    #[serde(rename = "B", default)]
    balances: Vec<WsBalance>,
    #[serde(rename = "P", default)]
    positions: Vec<WsPosition>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WsBalance {
    #[serde(rename = "a")]
    pub asset: String,
    #[serde(rename = "wb")]
    pub wallet_balance: String,
    #[serde(rename = "cw")]
    pub cross_wallet_balance: String,
    #[serde(rename = "bc")]
    pub balance_change: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WsPosition {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "pa")]
    pub position_amount: String,
    #[serde(rename = "ep")]
    pub entry_price: String,
    #[serde(rename = "cr")]
    pub accumulated_realized: String,
    #[serde(rename = "up")]
    pub unrealized_pnl: String,
    #[serde(rename = "mt")]
    pub margin_type: String,
    #[serde(rename = "iw")]
    pub isolated_wallet: String,
    #[serde(rename = "ps")]
    pub position_side: String,
}
