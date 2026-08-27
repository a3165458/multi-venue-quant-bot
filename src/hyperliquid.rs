//! Hyperliquid L1 protocol primitives and REST adapter.
//!
//! Order-placing actions are msgpack-encoded (canonical field order), hashed with
//! keccak256 together with the nonce and vault flag, then signed as an EIP-712
//! "phantom agent" (`Agent { source, connectionId }`, chain id 1337). The JSON
//! request body must therefore serialize action fields in exactly the same order
//! as the msgpack encoding: every action is a concrete struct and is never routed
//! through `serde_json::Value` (which would re-order keys alphabetically).
//!
//! HIP-3 builder-deployed perps are fully supported: coins are addressed by their
//! canonical `dex:NAME` form (for example `io:SNDK` on the EntropyIO dex) and the
//! asset id is resolved as `110000 + builder_dex_index * 10000 + index_in_meta`.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use k256::ecdsa::{SigningKey, VerifyingKey};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use thiserror::Error;

pub const MAINNET_REST_URL: &str = "https://api.hyperliquid.xyz";
pub const MAINNET_WS_URL: &str = "wss://api.hyperliquid.xyz/ws";
pub const TESTNET_REST_URL: &str = "https://api.hyperliquid-testnet.xyz";
pub const TESTNET_WS_URL: &str = "wss://api.hyperliquid-testnet.xyz/ws";

/// Builder-deployed perp dexs allocate asset ids starting at this offset.
pub const BUILDER_DEX_ASSET_OFFSET: u32 = 110_000;
/// Each builder-deployed perp dex owns a contiguous block of this many asset ids.
pub const BUILDER_DEX_ASSET_STRIDE: u32 = 10_000;
/// Perp prices allow at most `6 - szDecimals` decimal places.
pub const MAX_PRICE_DECIMALS: u32 = 6;
/// Non-integer perp prices allow at most five significant figures.
pub const MAX_PRICE_SIGNIFICANT_FIGURES: u32 = 5;
/// Hyperliquid rejects orders below this notional value in USD.
pub const MIN_ORDER_NOTIONAL_USD: f64 = 10.0;

const EIP712_CHAIN_ID: u64 = 1337;
const REST_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum HyperliquidError {
    #[error("invalid Hyperliquid credentials: {0}")]
    Credentials(String),
    #[error("invalid Hyperliquid request: {0}")]
    InvalidRequest(String),
    #[error("Hyperliquid signing failed: {0}")]
    Signing(String),
    #[error("Hyperliquid transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Hyperliquid response was invalid: {0}")]
    InvalidResponse(String),
    #[error("Hyperliquid rate limit (HTTP 429): {message}")]
    RateLimited { message: String },
    #[error("Hyperliquid rejected the action: {message}")]
    ActionRejected { message: String },
    #[error("Hyperliquid API error HTTP {status}: {message}")]
    Api { status: u16, message: String },
    #[error("unknown Hyperliquid market: {0}")]
    UnknownMarket(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HyperliquidEnvironment {
    Mainnet,
    Testnet,
}

impl HyperliquidEnvironment {
    pub const fn rest_url(self) -> &'static str {
        match self {
            Self::Mainnet => MAINNET_REST_URL,
            Self::Testnet => TESTNET_REST_URL,
        }
    }

    pub const fn websocket_url(self) -> &'static str {
        match self {
            Self::Mainnet => MAINNET_WS_URL,
            Self::Testnet => TESTNET_WS_URL,
        }
    }

    /// EIP-712 phantom-agent source: `"a"` on mainnet, `"b"` on testnet.
    pub const fn agent_source(self) -> &'static str {
        match self {
            Self::Mainnet => "a",
            Self::Testnet => "b",
        }
    }
}

#[derive(Clone)]
pub struct HyperliquidCredentials {
    account_address: String,
    signer_address: String,
    signing_key: SigningKey,
}

impl fmt::Debug for HyperliquidCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HyperliquidCredentials")
            .field("account_address", &self.account_address)
            .field("signer_address", &self.signer_address)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

impl HyperliquidCredentials {
    /// `account_address` is the master account queried for balances, orders, and
    /// fills. `private_key` may belong either to that account or to an approved
    /// API/agent wallet that signs on its behalf.
    pub fn new(account_address: &str, private_key: &str) -> Result<Self, HyperliquidError> {
        let account_address = validate_address(account_address, "account address")?;
        let key_hex = private_key.strip_prefix("0x").ok_or_else(|| {
            HyperliquidError::Credentials("signer private key must start with lowercase 0x".into())
        })?;
        if key_hex.len() != 64 || !key_hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(HyperliquidError::Credentials(
                "signer private key must contain exactly 32 hexadecimal bytes".into(),
            ));
        }
        let mut key_bytes = [0_u8; 32];
        hex::decode_to_slice(key_hex, &mut key_bytes).map_err(|error| {
            HyperliquidError::Credentials(format!("signer private key is not valid hex: {error}"))
        })?;
        let signing_key = SigningKey::from_slice(&key_bytes).map_err(|error| {
            HyperliquidError::Credentials(format!("signer private key is invalid: {error}"))
        })?;
        let signer_address = ethereum_address(signing_key.verifying_key());
        Ok(Self {
            account_address,
            signer_address,
            signing_key,
        })
    }

    pub fn account_address(&self) -> &str {
        &self.account_address
    }

    pub fn signer_address(&self) -> &str {
        &self.signer_address
    }
}

fn validate_address(value: &str, label: &str) -> Result<String, HyperliquidError> {
    let body = value.strip_prefix("0x").ok_or_else(|| {
        HyperliquidError::Credentials(format!("{label} must start with lowercase 0x"))
    })?;
    if body.len() != 40 || !body.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HyperliquidError::Credentials(format!(
            "{label} must contain exactly 20 hexadecimal bytes"
        )));
    }
    Ok(format!("0x{}", body.to_ascii_lowercase()))
}

pub fn ethereum_address(key: &VerifyingKey) -> String {
    let encoded = key.to_encoded_point(false);
    let digest = Keccak256::digest(&encoded.as_bytes()[1..]);
    format!("0x{}", hex::encode(&digest[digest.len() - 20..]))
}

/// Serialize a float exactly like the SDK `float_to_wire`: format with eight
/// decimals, reject inputs that would round, then trim trailing zeros.
pub fn float_to_wire(value: f64) -> Result<String, HyperliquidError> {
    if !value.is_finite() {
        return Err(HyperliquidError::InvalidRequest(
            "wire float must be finite".into(),
        ));
    }
    let rounded = format!("{value:.8}");
    let reparsed: f64 = rounded
        .parse()
        .map_err(|_| HyperliquidError::InvalidRequest("wire float failed to round-trip".into()))?;
    if (reparsed - value).abs() >= 1e-12 {
        return Err(HyperliquidError::InvalidRequest(format!(
            "float {value} loses precision on the wire; quantize it first"
        )));
    }
    let mut text = rounded;
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    if text == "-0" {
        text = "0".into();
    }
    Ok(text)
}

/// Millisecond nonce source that never repeats or goes backwards for one signer.
#[derive(Debug)]
pub struct HyperliquidNonce {
    last: AtomicU64,
}

impl Default for HyperliquidNonce {
    fn default() -> Self {
        Self {
            last: AtomicU64::new(0),
        }
    }
}

impl HyperliquidNonce {
    pub fn next(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last
            .fetch_update(AtomicOrdering::SeqCst, AtomicOrdering::SeqCst, |last| {
                Some(now.max(last + 1))
            })
            .map(|last| now.max(last + 1))
            .unwrap_or(now)
    }
}

// ---------------------------------------------------------------------------
// Action wires. Field declaration order is the canonical msgpack/JSON order and
// must never change: it feeds directly into the signed action hash.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tif {
    /// Add-liquidity-only (post-only).
    Alo,
    /// Good-til-canceled.
    Gtc,
    /// Immediate-or-cancel.
    Ioc,
}

impl Tif {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Alo => "Alo",
            Self::Gtc => "Gtc",
            Self::Ioc => "Ioc",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LimitWire {
    pub tif: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderTypeWire {
    pub limit: LimitWire,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderWire {
    pub a: u32,
    pub b: bool,
    pub p: String,
    pub s: String,
    pub r: bool,
    pub t: OrderTypeWire,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub c: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub orders: Vec<OrderWire>,
    pub grouping: &'static str,
}

impl OrderAction {
    pub fn new(orders: Vec<OrderWire>) -> Self {
        Self {
            kind: "order",
            orders,
            grouping: "na",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelWire {
    pub a: u32,
    pub o: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub cancels: Vec<CancelWire>,
}

impl CancelAction {
    pub fn new(cancels: Vec<CancelWire>) -> Self {
        Self {
            kind: "cancel",
            cancels,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelByCloidWire {
    pub asset: u32,
    pub cloid: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelByCloidAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub cancels: Vec<CancelByCloidWire>,
}

impl CancelByCloidAction {
    pub fn new(cancels: Vec<CancelByCloidWire>) -> Self {
        Self {
            kind: "cancelByCloid",
            cancels,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateLeverageAction {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub asset: u32,
    #[serde(rename = "isCross")]
    pub is_cross: bool,
    pub leverage: u32,
}

impl UpdateLeverageAction {
    pub fn new(asset: u32, is_cross: bool, leverage: u32) -> Self {
        Self {
            kind: "updateLeverage",
            asset,
            is_cross,
            leverage,
        }
    }
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SignatureRsv {
    pub r: String,
    pub s: String,
    pub v: u64,
}

#[derive(Debug, Serialize)]
struct ExchangeRequest<'a, A: Serialize> {
    action: &'a A,
    nonce: u64,
    signature: SignatureRsv,
    #[serde(rename = "vaultAddress")]
    vault_address: Option<&'a str>,
    #[serde(rename = "expiresAfter")]
    expires_after: Option<u64>,
}

/// keccak256(msgpack(action) ++ nonce_be ++ vault flag ++ optional expiry).
pub fn action_hash<A: Serialize>(
    action: &A,
    vault_address: Option<&str>,
    nonce: u64,
    expires_after: Option<u64>,
) -> Result<[u8; 32], HyperliquidError> {
    let mut data = rmp_serde::to_vec_named(action)
        .map_err(|error| HyperliquidError::Signing(format!("msgpack encoding failed: {error}")))?;
    data.extend_from_slice(&nonce.to_be_bytes());
    match vault_address {
        None => data.push(0x00),
        Some(address) => {
            data.push(0x01);
            let body = address.strip_prefix("0x").ok_or_else(|| {
                HyperliquidError::Signing("vault address must start with 0x".into())
            })?;
            let bytes = hex::decode(body).map_err(|error| {
                HyperliquidError::Signing(format!("vault address is not valid hex: {error}"))
            })?;
            data.extend_from_slice(&bytes);
        }
    }
    if let Some(expires) = expires_after {
        data.push(0x00);
        data.extend_from_slice(&expires.to_be_bytes());
    }
    Ok(Keccak256::digest(&data).into())
}

fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// EIP-712 digest for the phantom agent `{ source, connectionId }`.
pub fn phantom_agent_digest(connection_id: &[u8; 32], source: &str) -> [u8; 32] {
    let domain_typehash = keccak(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut domain = Vec::with_capacity(160);
    domain.extend_from_slice(&domain_typehash);
    domain.extend_from_slice(&keccak(b"Exchange"));
    domain.extend_from_slice(&keccak(b"1"));
    let mut chain_id = [0_u8; 32];
    chain_id[24..].copy_from_slice(&EIP712_CHAIN_ID.to_be_bytes());
    domain.extend_from_slice(&chain_id);
    domain.extend_from_slice(&[0_u8; 32]);
    let domain_separator = keccak(&domain);

    let agent_typehash = keccak(b"Agent(string source,bytes32 connectionId)");
    let mut agent = Vec::with_capacity(96);
    agent.extend_from_slice(&agent_typehash);
    agent.extend_from_slice(&keccak(source.as_bytes()));
    agent.extend_from_slice(connection_id);
    let struct_hash = keccak(&agent);

    let mut digest_input = Vec::with_capacity(66);
    digest_input.extend_from_slice(&[0x19, 0x01]);
    digest_input.extend_from_slice(&domain_separator);
    digest_input.extend_from_slice(&struct_hash);
    keccak(&digest_input)
}

fn minimal_hex(bytes: &[u8]) -> String {
    let full = hex::encode(bytes);
    let trimmed = full.trim_start_matches('0');
    if trimmed.is_empty() {
        "0x0".into()
    } else {
        format!("0x{trimmed}")
    }
}

pub fn sign_l1_action<A: Serialize>(
    signing_key: &SigningKey,
    action: &A,
    vault_address: Option<&str>,
    nonce: u64,
    expires_after: Option<u64>,
    environment: HyperliquidEnvironment,
) -> Result<SignatureRsv, HyperliquidError> {
    let connection_id = action_hash(action, vault_address, nonce, expires_after)?;
    let digest = phantom_agent_digest(&connection_id, environment.agent_source());
    let (signature, recovery_id) = signing_key
        .sign_prehash_recoverable(&digest)
        .map_err(|error| HyperliquidError::Signing(error.to_string()))?;
    let r = signature.r().to_bytes();
    let s = signature.s().to_bytes();
    Ok(SignatureRsv {
        r: minimal_hex(&r),
        s: minimal_hex(&s),
        v: 27 + u64::from(recovery_id.to_byte()),
    })
}

// ---------------------------------------------------------------------------
// Markets and quantization
// ---------------------------------------------------------------------------

/// Split a canonical coin name into `(dex, coin)`. Coins on the primary dex have
/// no prefix; HIP-3 coins are addressed as `dex:NAME` (for example `io:SNDK`).
pub fn split_coin(coin: &str) -> (&str, &str) {
    match coin.split_once(':') {
        Some((dex, _)) => (dex, coin),
        None => ("", coin),
    }
}

/// Asset id for `index_in_meta` on a dex. `builder_index` is the zero-based
/// position of the dex among builder-deployed dexs (`perpDexs` minus the leading
/// null primary-dex slot); `None` addresses the primary dex.
pub fn asset_id(builder_index: Option<u32>, index_in_meta: u32) -> u32 {
    match builder_index {
        None => index_in_meta,
        Some(index) => BUILDER_DEX_ASSET_OFFSET + index * BUILDER_DEX_ASSET_STRIDE + index_in_meta,
    }
}

#[derive(Debug, Clone)]
pub struct HyperliquidMarket {
    /// Canonical coin name, including any `dex:` prefix (`io:SNDK`).
    pub coin: String,
    /// Dex name; empty string for the primary dex.
    pub dex: String,
    /// Asset id used in signed order actions.
    pub asset: u32,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    pub only_isolated: bool,
}

impl HyperliquidMarket {
    pub fn price_decimals(&self) -> u32 {
        MAX_PRICE_DECIMALS.saturating_sub(self.sz_decimals)
    }

    /// Quantize a price to the exchange grid: at most `6 - szDecimals` decimals
    /// and at most five significant figures (integer prices are always allowed).
    /// `round_up=false` floors (safe for bids), `round_up=true` ceils (asks).
    pub fn quantize_price(&self, value: f64, round_up: bool) -> Result<String, HyperliquidError> {
        if !value.is_finite() || value <= 0.0 {
            return Err(HyperliquidError::InvalidRequest(format!(
                "price must be positive and finite, got {value}"
            )));
        }
        let magnitude = value.log10().floor() as i32;
        let sig_fig_decimals = (MAX_PRICE_SIGNIFICANT_FIGURES as i32 - 1) - magnitude;
        let decimals = sig_fig_decimals.clamp(0, self.price_decimals() as i32) as u32;
        let text = quantize_to_decimals(value, decimals, round_up)?;
        if text == "0" {
            return Err(HyperliquidError::InvalidRequest(format!(
                "price {value} quantizes to zero"
            )));
        }
        Ok(text)
    }

    /// Quantize a size down to `szDecimals` decimals. Fails when the size
    /// truncates to zero.
    pub fn quantize_size(&self, value: f64) -> Result<String, HyperliquidError> {
        if !value.is_finite() || value <= 0.0 {
            return Err(HyperliquidError::InvalidRequest(format!(
                "size must be positive and finite, got {value}"
            )));
        }
        let text = quantize_to_decimals(value, self.sz_decimals, false)?;
        if text == "0" {
            return Err(HyperliquidError::InvalidRequest(format!(
                "size {value} truncates to zero at {} decimals",
                self.sz_decimals
            )));
        }
        Ok(text)
    }
}

fn quantize_to_decimals(
    value: f64,
    decimals: u32,
    round_up: bool,
) -> Result<String, HyperliquidError> {
    let scale = 10_f64.powi(decimals as i32);
    let scaled = value * scale;
    if !scaled.is_finite() || scaled >= u128::MAX as f64 {
        return Err(HyperliquidError::InvalidRequest(format!(
            "value {value} overflows the quantization grid"
        )));
    }
    let epsilon = scaled.abs() * 1e-9 + 1e-9;
    let units = if round_up {
        (scaled - epsilon).ceil()
    } else {
        (scaled + epsilon).floor()
    };
    let units = if units < 0.0 { 0_u128 } else { units as u128 };
    Ok(decimal_text(units, decimals))
}

fn decimal_text(units: u128, decimals: u32) -> String {
    if decimals == 0 {
        return units.to_string();
    }
    let mut digits = format!("{units:0width$}", width = decimals as usize + 1);
    digits.insert(digits.len() - decimals as usize, '.');
    while digits.ends_with('0') {
        digits.pop();
    }
    if digits.ends_with('.') {
        digits.pop();
    }
    digits
}

// ---------------------------------------------------------------------------
// REST client
// ---------------------------------------------------------------------------

pub struct HyperliquidClient {
    http: reqwest::Client,
    rest_url: String,
    environment: HyperliquidEnvironment,
    credentials: Option<HyperliquidCredentials>,
    nonce: HyperliquidNonce,
}

impl HyperliquidClient {
    pub fn public(environment: HyperliquidEnvironment) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(REST_REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client"),
            rest_url: environment.rest_url().to_string(),
            environment,
            credentials: None,
            nonce: HyperliquidNonce::default(),
        }
    }

    pub fn authenticated(
        credentials: HyperliquidCredentials,
        environment: HyperliquidEnvironment,
    ) -> Self {
        let mut client = Self::public(environment);
        client.credentials = Some(credentials);
        client
    }

    pub fn environment(&self) -> HyperliquidEnvironment {
        self.environment
    }

    pub fn account_address(&self) -> Result<&str, HyperliquidError> {
        self.credentials
            .as_ref()
            .map(|credentials| credentials.account_address())
            .ok_or_else(|| {
                HyperliquidError::Credentials("client has no Hyperliquid credentials".into())
            })
    }

    async fn post_info<T: DeserializeOwned>(
        &self,
        body: &serde_json::Value,
    ) -> Result<T, HyperliquidError> {
        let response = self
            .http
            .post(format!("{}/info", self.rest_url))
            .json(body)
            .send()
            .await?;
        Self::decode_response(response).await
    }

    async fn decode_response<T: DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, HyperliquidError> {
        let status = response.status();
        let text = response.text().await?;
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(HyperliquidError::RateLimited { message: text });
        }
        if !status.is_success() {
            return Err(HyperliquidError::Api {
                status: status.as_u16(),
                message: text,
            });
        }
        serde_json::from_str(&text).map_err(|error| {
            HyperliquidError::InvalidResponse(format!("{error}; body: {text:.256}"))
        })
    }

    // -- Info ---------------------------------------------------------------

    /// The first slot is `null` (the primary dex), followed by builder dexs.
    pub async fn perp_dexs(&self) -> Result<Vec<Option<PerpDex>>, HyperliquidError> {
        self.post_info(&serde_json::json!({"type": "perpDexs"}))
            .await
    }

    pub async fn meta(&self, dex: &str) -> Result<PerpMeta, HyperliquidError> {
        self.post_info(&serde_json::json!({"type": "meta", "dex": dex}))
            .await
    }

    /// Resolve configured coins (canonical `dex:NAME` or primary-dex names) into
    /// tradeable markets with asset ids. Fails closed on unknown or delisted
    /// coins so a typo can never trade a different market.
    pub async fn fetch_markets(
        &self,
        coins: &[String],
    ) -> Result<Vec<HyperliquidMarket>, HyperliquidError> {
        let mut dexs: Vec<String> = Vec::new();
        for coin in coins {
            let (dex, _) = split_coin(coin);
            if !dexs.iter().any(|existing| existing == dex) {
                dexs.push(dex.to_string());
            }
        }
        let needs_builder = dexs.iter().any(|dex| !dex.is_empty());
        let builder_indices: HashMap<String, u32> = if needs_builder {
            let listed = self.perp_dexs().await?;
            listed
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(index, dex)| (dex.name, index as u32))
                .collect()
        } else {
            HashMap::new()
        };
        let mut markets = Vec::with_capacity(coins.len());
        for dex in &dexs {
            let builder_index = if dex.is_empty() {
                None
            } else {
                Some(*builder_indices.get(dex).ok_or_else(|| {
                    HyperliquidError::UnknownMarket(format!("perp dex {dex} is not listed"))
                })?)
            };
            let meta = self.meta(dex).await?;
            for coin in coins {
                let (coin_dex, name) = split_coin(coin);
                if coin_dex != dex {
                    continue;
                }
                let (index, entry) = meta
                    .universe
                    .iter()
                    .enumerate()
                    .find(|(_, entry)| entry.name == name)
                    .ok_or_else(|| {
                        HyperliquidError::UnknownMarket(format!(
                            "coin {name} is not in the {} meta",
                            if dex.is_empty() { "primary" } else { dex }
                        ))
                    })?;
                if entry.is_delisted {
                    return Err(HyperliquidError::UnknownMarket(format!(
                        "coin {name} is delisted"
                    )));
                }
                markets.push(HyperliquidMarket {
                    coin: entry.name.clone(),
                    dex: dex.clone(),
                    asset: asset_id(builder_index, index as u32),
                    sz_decimals: entry.sz_decimals,
                    max_leverage: entry.max_leverage,
                    only_isolated: entry.only_isolated,
                });
            }
        }
        Ok(markets)
    }

    pub async fn clearinghouse_state(
        &self,
        user: &str,
        dex: &str,
    ) -> Result<ClearinghouseState, HyperliquidError> {
        self.post_info(&serde_json::json!({
            "type": "clearinghouseState",
            "user": user,
            "dex": dex,
        }))
        .await
    }

    pub async fn user_abstraction(&self, user: &str) -> Result<UserAbstraction, HyperliquidError> {
        self.post_info(&serde_json::json!({
            "type": "userAbstraction",
            "user": user,
        }))
        .await
    }

    pub async fn spot_clearinghouse_state(
        &self,
        user: &str,
    ) -> Result<SpotClearinghouseState, HyperliquidError> {
        self.post_info(&serde_json::json!({
            "type": "spotClearinghouseState",
            "user": user,
        }))
        .await
    }

    pub async fn open_orders(
        &self,
        user: &str,
        dex: &str,
    ) -> Result<Vec<OpenOrder>, HyperliquidError> {
        self.post_info(&serde_json::json!({
            "type": "frontendOpenOrders",
            "user": user,
            "dex": dex,
        }))
        .await
    }

    pub async fn order_status(
        &self,
        user: &str,
        oid: OrderId,
    ) -> Result<OrderStatusResponse, HyperliquidError> {
        let oid_value = match oid {
            OrderId::Oid(oid) => serde_json::json!(oid),
            OrderId::Cloid(ref cloid) => serde_json::json!(cloid),
        };
        self.post_info(&serde_json::json!({
            "type": "orderStatus",
            "user": user,
            "oid": oid_value,
        }))
        .await
    }

    pub async fn user_fills_by_time(
        &self,
        user: &str,
        start_time_ms: u64,
        end_time_ms: Option<u64>,
    ) -> Result<Vec<Fill>, HyperliquidError> {
        let mut body = serde_json::json!({
            "type": "userFillsByTime",
            "user": user,
            "startTime": start_time_ms,
            "aggregateByTime": false,
        });
        if let Some(end) = end_time_ms {
            body["endTime"] = serde_json::json!(end);
        }
        self.post_info(&body).await
    }

    pub async fn user_funding(
        &self,
        user: &str,
        start_time_ms: u64,
    ) -> Result<Vec<UserFundingEntry>, HyperliquidError> {
        self.post_info(&serde_json::json!({
            "type": "userFunding",
            "user": user,
            "startTime": start_time_ms,
        }))
        .await
    }

    pub async fn candle_snapshot(
        &self,
        coin: &str,
        interval: &str,
        start_time_ms: u64,
        end_time_ms: u64,
    ) -> Result<Vec<Candle>, HyperliquidError> {
        self.post_info(&serde_json::json!({
            "type": "candleSnapshot",
            "req": {
                "coin": coin,
                "interval": interval,
                "startTime": start_time_ms,
                "endTime": end_time_ms,
            },
        }))
        .await
    }

    pub async fn l2_book(&self, coin: &str) -> Result<L2Book, HyperliquidError> {
        self.post_info(&serde_json::json!({"type": "l2Book", "coin": coin}))
            .await
    }

    pub async fn user_fees(&self, user: &str) -> Result<UserFees, HyperliquidError> {
        self.post_info(&serde_json::json!({"type": "userFees", "user": user}))
            .await
    }

    pub async fn all_mids(&self, dex: &str) -> Result<HashMap<String, String>, HyperliquidError> {
        self.post_info(&serde_json::json!({"type": "allMids", "dex": dex}))
            .await
    }

    // -- Exchange -----------------------------------------------------------

    async fn post_action<A: Serialize>(
        &self,
        action: &A,
    ) -> Result<ExchangeResponse, HyperliquidError> {
        let credentials = self.credentials.as_ref().ok_or_else(|| {
            HyperliquidError::Credentials("client has no Hyperliquid credentials".into())
        })?;
        let nonce = self.nonce.next();
        let signature = sign_l1_action(
            &credentials.signing_key,
            action,
            None,
            nonce,
            None,
            self.environment,
        )?;
        let request = ExchangeRequest {
            action,
            nonce,
            signature,
            vault_address: None,
            expires_after: None,
        };
        let body = serde_json::to_string(&request)
            .map_err(|error| HyperliquidError::Signing(format!("request encoding: {error}")))?;
        let response = self
            .http
            .post(format!("{}/exchange", self.rest_url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        let envelope: ExchangeResponse = Self::decode_response(response).await?;
        if envelope.status != "ok" {
            let message = match &envelope.response {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            return Err(HyperliquidError::ActionRejected { message });
        }
        Ok(envelope)
    }

    /// Place a batch of orders. The result vector is index-aligned with the
    /// request vector; per-order rejections surface as `OrderOutcome::Error`.
    pub async fn place_orders(
        &self,
        requests: &[NewOrderRequest],
    ) -> Result<Vec<OrderOutcome>, HyperliquidError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let wires = requests
            .iter()
            .map(NewOrderRequest::to_wire)
            .collect::<Result<Vec<_>, _>>()?;
        let action = OrderAction::new(wires);
        let envelope = self.post_action(&action).await?;
        let statuses = envelope.order_statuses()?;
        if statuses.len() != requests.len() {
            return Err(HyperliquidError::InvalidResponse(format!(
                "expected {} order statuses, got {}",
                requests.len(),
                statuses.len()
            )));
        }
        Ok(statuses)
    }

    pub async fn place_order(
        &self,
        request: &NewOrderRequest,
    ) -> Result<OrderOutcome, HyperliquidError> {
        let mut outcomes = self.place_orders(std::slice::from_ref(request)).await?;
        Ok(outcomes.remove(0))
    }

    /// Cancel orders by exchange order id. Result is index-aligned; `Ok(())`
    /// entries were canceled, `Err(message)` entries were rejected.
    pub async fn cancel_orders(
        &self,
        cancels: &[(u32, u64)],
    ) -> Result<Vec<Result<(), String>>, HyperliquidError> {
        if cancels.is_empty() {
            return Ok(Vec::new());
        }
        let action = CancelAction::new(
            cancels
                .iter()
                .map(|&(asset, oid)| CancelWire { a: asset, o: oid })
                .collect(),
        );
        let envelope = self.post_action(&action).await?;
        envelope.cancel_statuses(cancels.len())
    }

    pub async fn cancel_orders_by_cloid(
        &self,
        cancels: &[(u32, String)],
    ) -> Result<Vec<Result<(), String>>, HyperliquidError> {
        if cancels.is_empty() {
            return Ok(Vec::new());
        }
        for (_, cloid) in cancels {
            validate_cloid(cloid)?;
        }
        let action = CancelByCloidAction::new(
            cancels
                .iter()
                .map(|(asset, cloid)| CancelByCloidWire {
                    asset: *asset,
                    cloid: cloid.clone(),
                })
                .collect(),
        );
        let envelope = self.post_action(&action).await?;
        envelope.cancel_statuses(cancels.len())
    }

    pub async fn update_leverage(
        &self,
        asset: u32,
        is_cross: bool,
        leverage: u32,
    ) -> Result<(), HyperliquidError> {
        if leverage == 0 {
            return Err(HyperliquidError::InvalidRequest(
                "leverage must be at least 1".into(),
            ));
        }
        let action = UpdateLeverageAction::new(asset, is_cross, leverage);
        self.post_action(&action).await.map(|_| ())
    }
}

/// A client order id: 16 bytes, canonical lowercase `0x` + 32 hex characters.
pub fn validate_cloid(cloid: &str) -> Result<(), HyperliquidError> {
    let body = cloid.strip_prefix("0x").ok_or_else(|| {
        HyperliquidError::InvalidRequest("cloid must start with lowercase 0x".into())
    })?;
    if body.len() != 32 || !body.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HyperliquidError::InvalidRequest(
            "cloid must contain exactly 16 hexadecimal bytes".into(),
        ));
    }
    Ok(())
}

/// Deterministic 16-byte cloid from an arbitrary strategy key.
pub fn cloid_from_key(key: &str) -> String {
    let digest = Keccak256::digest(key.as_bytes());
    format!("0x{}", hex::encode(&digest[..16]))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderId {
    Oid(u64),
    Cloid(String),
}

#[derive(Debug, Clone)]
pub struct NewOrderRequest {
    pub asset: u32,
    pub is_buy: bool,
    /// Already-quantized price text (see `HyperliquidMarket::quantize_price`).
    pub price: String,
    /// Already-quantized size text (see `HyperliquidMarket::quantize_size`).
    pub size: String,
    pub reduce_only: bool,
    pub tif: Tif,
    pub cloid: Option<String>,
}

impl NewOrderRequest {
    fn to_wire(&self) -> Result<OrderWire, HyperliquidError> {
        if let Some(cloid) = &self.cloid {
            validate_cloid(cloid)?;
        }
        for (label, text) in [("price", &self.price), ("size", &self.size)] {
            let value: f64 = text.parse().map_err(|_| {
                HyperliquidError::InvalidRequest(format!("{label} {text} is not a number"))
            })?;
            if !value.is_finite() || value <= 0.0 {
                return Err(HyperliquidError::InvalidRequest(format!(
                    "{label} must be positive, got {text}"
                )));
            }
            let wire = float_to_wire(value)?;
            if &wire != text {
                return Err(HyperliquidError::InvalidRequest(format!(
                    "{label} {text} is not in canonical wire form (expected {wire})"
                )));
            }
        }
        Ok(OrderWire {
            a: self.asset,
            b: self.is_buy,
            p: self.price.clone(),
            s: self.size.clone(),
            r: self.reduce_only,
            t: OrderTypeWire {
                limit: LimitWire {
                    tif: self.tif.as_str(),
                },
            },
            c: self.cloid.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeResponse {
    pub status: String,
    #[serde(default)]
    pub response: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OrderOutcome {
    Resting {
        oid: u64,
        cloid: Option<String>,
    },
    Filled {
        oid: u64,
        total_sz: String,
        avg_px: String,
    },
    Error(String),
}

impl ExchangeResponse {
    fn statuses(&self) -> Result<&Vec<serde_json::Value>, HyperliquidError> {
        self.response
            .get("data")
            .and_then(|data| data.get("statuses"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                HyperliquidError::InvalidResponse(format!(
                    "exchange response has no statuses: {}",
                    self.response
                ))
            })
    }

    pub fn order_statuses(&self) -> Result<Vec<OrderOutcome>, HyperliquidError> {
        self.statuses()?
            .iter()
            .map(|status| {
                if let Some(error) = status.get("error").and_then(serde_json::Value::as_str) {
                    return Ok(OrderOutcome::Error(error.to_string()));
                }
                if let Some(resting) = status.get("resting") {
                    let oid = resting.get("oid").and_then(serde_json::Value::as_u64);
                    let cloid = resting
                        .get("cloid")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    return oid
                        .map(|oid| OrderOutcome::Resting { oid, cloid })
                        .ok_or_else(|| {
                            HyperliquidError::InvalidResponse("resting status missing oid".into())
                        });
                }
                if let Some(filled) = status.get("filled") {
                    let oid = filled.get("oid").and_then(serde_json::Value::as_u64);
                    let total_sz = filled
                        .get("totalSz")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let avg_px = filled
                        .get("avgPx")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    return oid
                        .map(|oid| OrderOutcome::Filled {
                            oid,
                            total_sz,
                            avg_px,
                        })
                        .ok_or_else(|| {
                            HyperliquidError::InvalidResponse("filled status missing oid".into())
                        });
                }
                Err(HyperliquidError::InvalidResponse(format!(
                    "unrecognized order status: {status}"
                )))
            })
            .collect()
    }

    fn cancel_statuses(
        &self,
        expected: usize,
    ) -> Result<Vec<Result<(), String>>, HyperliquidError> {
        let statuses = self.statuses()?;
        if statuses.len() != expected {
            return Err(HyperliquidError::InvalidResponse(format!(
                "expected {expected} cancel statuses, got {}",
                statuses.len()
            )));
        }
        Ok(statuses
            .iter()
            .map(|status| {
                if status.as_str() == Some("success") {
                    Ok(())
                } else if let Some(error) = status.get("error").and_then(serde_json::Value::as_str)
                {
                    Err(error.to_string())
                } else {
                    Err(format!("unrecognized cancel status: {status}"))
                }
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Info response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct PerpDex {
    pub name: String,
    #[serde(rename = "fullName", default)]
    pub full_name: Option<String>,
    #[serde(default)]
    pub deployer: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PerpMeta {
    pub universe: Vec<PerpAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PerpAsset {
    pub name: String,
    #[serde(rename = "szDecimals")]
    pub sz_decimals: u32,
    #[serde(rename = "maxLeverage")]
    pub max_leverage: u32,
    #[serde(rename = "onlyIsolated", default)]
    pub only_isolated: bool,
    #[serde(rename = "isDelisted", default)]
    pub is_delisted: bool,
    #[serde(rename = "marginMode", default)]
    pub margin_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarginSummary {
    #[serde(rename = "accountValue")]
    pub account_value: String,
    #[serde(rename = "totalNtlPos")]
    pub total_ntl_pos: String,
    #[serde(rename = "totalMarginUsed")]
    pub total_margin_used: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Leverage {
    #[serde(rename = "type")]
    pub kind: String,
    pub value: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PerpPosition {
    pub coin: String,
    /// Signed size: positive long, negative short.
    pub szi: String,
    #[serde(rename = "entryPx", default)]
    pub entry_px: Option<String>,
    #[serde(rename = "positionValue")]
    pub position_value: String,
    #[serde(rename = "unrealizedPnl")]
    pub unrealized_pnl: String,
    #[serde(rename = "liquidationPx", default)]
    pub liquidation_px: Option<String>,
    #[serde(rename = "marginUsed")]
    pub margin_used: String,
    pub leverage: Leverage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetPosition {
    pub position: PerpPosition,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClearinghouseState {
    #[serde(rename = "marginSummary")]
    pub margin_summary: MarginSummary,
    pub withdrawable: String,
    #[serde(rename = "assetPositions")]
    pub asset_positions: Vec<AssetPosition>,
    pub time: u64,
}

/// Account-abstraction mode from `userAbstraction`. Unified / portfolio
/// accounts keep USDC in the spot clearinghouse; isolated HIP-3 dex states
/// are then not a meaningful equity source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UserAbstraction {
    UnifiedAccount,
    PortfolioMargin,
    Disabled,
    Default,
    DexAbstraction,
}

impl UserAbstraction {
    pub fn uses_spot_collateral(self) -> bool {
        matches!(self, Self::UnifiedAccount | Self::PortfolioMargin)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpotBalance {
    pub coin: String,
    pub token: u32,
    pub total: String,
    #[serde(default)]
    pub hold: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpotClearinghouseState {
    #[serde(default)]
    pub balances: Vec<SpotBalance>,
    #[serde(rename = "tokenToAvailableAfterMaintenance", default)]
    pub token_to_available_after_maintenance: Vec<(u32, String)>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenOrder {
    pub coin: String,
    /// `"B"` for bid/buy, `"A"` for ask/sell.
    pub side: String,
    #[serde(rename = "limitPx")]
    pub limit_px: String,
    pub sz: String,
    pub oid: u64,
    pub timestamp: u64,
    #[serde(rename = "origSz")]
    pub orig_sz: String,
    #[serde(default)]
    pub cloid: Option<String>,
    #[serde(rename = "reduceOnly", default)]
    pub reduce_only: bool,
    #[serde(rename = "orderType", default)]
    pub order_type: String,
}

impl OpenOrder {
    pub fn is_buy(&self) -> bool {
        self.side == "B"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderStatusResponse {
    pub status: String,
    #[serde(default)]
    pub order: Option<OrderStatusEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderStatusEntry {
    pub order: OrderStatusOrder,
    pub status: String,
    #[serde(rename = "statusTimestamp")]
    pub status_timestamp: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderStatusOrder {
    pub coin: String,
    pub side: String,
    #[serde(rename = "limitPx")]
    pub limit_px: String,
    pub sz: String,
    pub oid: u64,
    pub timestamp: u64,
    #[serde(rename = "origSz")]
    pub orig_sz: String,
    #[serde(default)]
    pub cloid: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Fill {
    pub coin: String,
    pub px: String,
    pub sz: String,
    /// `"B"` buy, `"A"` sell.
    pub side: String,
    pub time: u64,
    #[serde(rename = "closedPnl")]
    pub closed_pnl: String,
    pub oid: u64,
    pub tid: u64,
    pub crossed: bool,
    pub fee: String,
    #[serde(rename = "feeToken", default)]
    pub fee_token: String,
    #[serde(default)]
    pub cloid: Option<String>,
    #[serde(default)]
    pub dir: String,
}

impl Fill {
    pub fn is_buy(&self) -> bool {
        self.side == "B"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserFundingEntry {
    pub time: u64,
    pub delta: FundingDelta,
    #[serde(default)]
    pub hash: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FundingDelta {
    pub coin: String,
    /// Signed USDC amount credited (positive) or debited (negative).
    pub usdc: String,
    #[serde(default)]
    pub szi: String,
    #[serde(rename = "fundingRate", default)]
    pub funding_rate: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Candle {
    #[serde(rename = "t")]
    pub open_time: u64,
    #[serde(rename = "T")]
    pub close_time: u64,
    #[serde(rename = "s")]
    pub coin: String,
    #[serde(rename = "i")]
    pub interval: String,
    #[serde(rename = "o")]
    pub open: String,
    #[serde(rename = "c")]
    pub close: String,
    #[serde(rename = "h")]
    pub high: String,
    #[serde(rename = "l")]
    pub low: String,
    #[serde(rename = "v")]
    pub volume: String,
    #[serde(rename = "n")]
    pub trades: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct L2Level {
    pub px: String,
    pub sz: String,
    pub n: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct L2Book {
    pub coin: String,
    pub time: u64,
    /// `levels[0]` bids (descending), `levels[1]` asks (ascending).
    pub levels: Vec<Vec<L2Level>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserFees {
    #[serde(rename = "userAddRate")]
    pub user_add_rate: String,
    #[serde(rename = "userCrossRate")]
    pub user_cross_rate: String,
}

// ---------------------------------------------------------------------------
// WebSocket protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subscription {
    Bbo { coin: String },
    L2Book { coin: String },
    OrderUpdates { user: String },
    UserFills { user: String },
}

impl Subscription {
    fn body(&self) -> serde_json::Value {
        match self {
            Self::Bbo { coin } => serde_json::json!({"type": "bbo", "coin": coin}),
            Self::L2Book { coin } => serde_json::json!({"type": "l2Book", "coin": coin}),
            Self::OrderUpdates { user } => {
                serde_json::json!({"type": "orderUpdates", "user": user})
            }
            Self::UserFills { user } => serde_json::json!({"type": "userFills", "user": user}),
        }
    }

    pub fn subscribe_message(&self) -> String {
        serde_json::json!({"method": "subscribe", "subscription": self.body()}).to_string()
    }
}

pub fn ws_ping_message() -> String {
    r#"{"method":"ping"}"#.to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub enum HyperliquidWsEvent {
    Bbo(BboUpdate),
    OrderUpdates(Vec<WsOrderUpdate>),
    UserFills(WsUserFills),
    Pong,
    SubscriptionResponse,
    Error(String),
    Ignored,
}

impl HyperliquidWsEvent {
    pub fn parse(text: &str) -> Result<Self, HyperliquidError> {
        let mut value: serde_json::Value = serde_json::from_str(text)
            .map_err(|error| HyperliquidError::InvalidResponse(error.to_string()))?;
        let channel = value
            .get("channel")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let data = value
            .get_mut("data")
            .map(serde_json::Value::take)
            .unwrap_or(serde_json::Value::Null);
        match channel.as_str() {
            "bbo" => from_ws_value(data).map(Self::Bbo),
            "orderUpdates" => from_ws_value(data).map(Self::OrderUpdates),
            "userFills" => from_ws_value(data).map(Self::UserFills),
            "pong" => Ok(Self::Pong),
            "subscriptionResponse" => Ok(Self::SubscriptionResponse),
            "error" => Ok(Self::Error(match data {
                serde_json::Value::String(text) => text,
                other => other.to_string(),
            })),
            _ => Ok(Self::Ignored),
        }
    }
}

fn from_ws_value<T: DeserializeOwned>(value: serde_json::Value) -> Result<T, HyperliquidError> {
    serde_json::from_value(value)
        .map_err(|error| HyperliquidError::InvalidResponse(error.to_string()))
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BboUpdate {
    pub coin: String,
    pub time: u64,
    /// `[best_bid, best_ask]`; either side can be missing on an empty book.
    pub bbo: Vec<Option<BboLevel>>,
}

impl BboUpdate {
    pub fn bid(&self) -> Option<&BboLevel> {
        self.bbo.first().and_then(Option::as_ref)
    }

    pub fn ask(&self) -> Option<&BboLevel> {
        self.bbo.get(1).and_then(Option::as_ref)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BboLevel {
    pub px: String,
    pub sz: String,
    pub n: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WsOrderUpdate {
    pub order: WsOrderState,
    /// `open`, `filled`, `canceled`, `rejected`, `marginCanceled`, ...
    pub status: String,
    #[serde(rename = "statusTimestamp")]
    pub status_timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WsOrderState {
    pub coin: String,
    /// `"B"` buy, `"A"` sell.
    pub side: String,
    #[serde(rename = "limitPx")]
    pub limit_px: String,
    /// Remaining size.
    pub sz: String,
    pub oid: u64,
    pub timestamp: u64,
    #[serde(rename = "origSz")]
    pub orig_sz: String,
    #[serde(default)]
    pub cloid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WsUserFills {
    #[serde(rename = "isSnapshot", default)]
    pub is_snapshot: bool,
    pub user: String,
    pub fills: Vec<WsFill>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WsFill {
    pub coin: String,
    pub px: String,
    pub sz: String,
    pub side: String,
    pub time: u64,
    #[serde(rename = "closedPnl")]
    pub closed_pnl: String,
    pub oid: u64,
    pub tid: u64,
    pub crossed: bool,
    pub fee: String,
    #[serde(rename = "feeToken", default)]
    pub fee_token: String,
    #[serde(default)]
    pub cloid: Option<String>,
    #[serde(default)]
    pub dir: String,
}

impl WsFill {
    pub fn is_buy(&self) -> bool {
        self.side == "B"
    }
}

pub fn order_update_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "filled"
            | "canceled"
            | "rejected"
            | "marginCanceled"
            | "liquidatedCanceled"
            | "expired"
            | "triggered"
            | "delistedCanceled"
            | "vaultWithdrawalCanceled"
            | "openInterestCapCanceled"
            | "selfTradeCanceled"
            | "reduceOnlyCanceled"
            | "siblingFilledCanceled"
            | "scheduledCancel"
            | "reduceOnlyRejected"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PRIVATE_KEY: &str =
        "0x0123456789012345678901234567890123456789012345678901234567890123";

    fn test_signing_key() -> SigningKey {
        let bytes = hex::decode(TEST_PRIVATE_KEY.strip_prefix("0x").unwrap()).unwrap();
        SigningKey::from_slice(&bytes).unwrap()
    }

    fn golden_order_action(asset: u32, price: &str, size: &str, tif: Tif) -> OrderAction {
        OrderAction::new(vec![OrderWire {
            a: asset,
            b: true,
            p: price.into(),
            s: size.into(),
            r: false,
            t: OrderTypeWire {
                limit: LimitWire { tif: tif.as_str() },
            },
            c: None,
        }])
    }

    #[test]
    fn derives_the_well_known_test_address() {
        let uppercase = HyperliquidCredentials::new(
            "0x14791697260E4c9A71f18484C9f997B308e59325",
            TEST_PRIVATE_KEY,
        )
        .unwrap();
        assert_eq!(
            uppercase.account_address(),
            "0x14791697260e4c9a71f18484c9f997b308e59325"
        );
        let credentials = HyperliquidCredentials::new(
            "0x14791697260e4c9a71f18484c9f997b308e59325",
            TEST_PRIVATE_KEY,
        )
        .unwrap();
        assert_eq!(
            credentials.signer_address(),
            "0x14791697260e4c9a71f18484c9f997b308e59325"
        );
        assert_eq!(
            credentials.account_address(),
            "0x14791697260e4c9a71f18484c9f997b308e59325"
        );
    }

    #[test]
    fn msgpack_action_encoding_matches_the_sdk() {
        let action = golden_order_action(4, "1670.1", "0.0147", Tif::Ioc);
        let encoded = rmp_serde::to_vec_named(&action).unwrap();
        assert_eq!(
            hex::encode(encoded),
            "83a474797065a56f72646572a66f72646572739186a16104a162c3a170a6313637302e31a173a6302e30313437a172c2a17481a56c696d697481a3746966a3496f63a867726f7570696e67a26e61"
        );
    }

    #[test]
    fn user_abstraction_and_spot_state_deserialize() {
        let mode: UserAbstraction = serde_json::from_str("\"unifiedAccount\"").unwrap();
        assert_eq!(mode, UserAbstraction::UnifiedAccount);
        assert!(mode.uses_spot_collateral());
        let spot: SpotClearinghouseState = serde_json::from_value(serde_json::json!({
            "balances": [{
                "coin": "USDC",
                "token": 0,
                "total": "2504.846418",
                "hold": "0.0",
                "entryNtl": "0.0"
            }],
            "tokenToAvailableAfterMaintenance": [[0, "2504.846418"]]
        }))
        .unwrap();
        assert_eq!(spot.balances[0].total, "2504.846418");
        assert_eq!(
            spot.token_to_available_after_maintenance[0].1,
            "2504.846418"
        );
    }

    #[test]
    fn phantom_agent_connection_id_matches_production() {
        let action = golden_order_action(4, "1670.1", "0.0147", Tif::Ioc);
        let hash = action_hash(&action, None, 1_677_777_606_040, None).unwrap();
        assert_eq!(
            hex::encode(hash),
            "0fcbeda5ae3c4950a548021552a4fea2226858c4453571bf3f24ba017eac2908"
        );
    }

    #[test]
    fn l1_order_signature_matches_the_sdk_golden_vector() {
        let action = golden_order_action(1, "100", "100", Tif::Gtc);
        let mainnet = sign_l1_action(
            &test_signing_key(),
            &action,
            None,
            0,
            None,
            HyperliquidEnvironment::Mainnet,
        )
        .unwrap();
        assert_eq!(
            mainnet.r,
            "0xd65369825a9df5d80099e513cce430311d7d26ddf477f5b3a33d2806b100d78e"
        );
        assert_eq!(
            mainnet.s,
            "0x2b54116ff64054968aa237c20ca9ff68000f977c93289157748a3162b6ea940e"
        );
        assert_eq!(mainnet.v, 28);

        let testnet = sign_l1_action(
            &test_signing_key(),
            &action,
            None,
            0,
            None,
            HyperliquidEnvironment::Testnet,
        )
        .unwrap();
        assert_eq!(
            testnet.r,
            "0x82b2ba28e76b3d761093aaded1b1cdad4960b3af30212b343fb2e6cdfa4e3d54"
        );
        assert_eq!(
            testnet.s,
            "0x6b53878fc99d26047f4d7e8c90eb98955a109f44209163f52d8dc4278cbbd9f5"
        );
        assert_eq!(testnet.v, 27);
    }

    #[test]
    fn json_order_field_order_matches_the_msgpack_order() {
        let action = golden_order_action(1, "100", "100", Tif::Gtc);
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(
            json,
            r#"{"type":"order","orders":[{"a":1,"b":true,"p":"100","s":"100","r":false,"t":{"limit":{"tif":"Gtc"}}}],"grouping":"na"}"#
        );
    }

    #[test]
    fn float_to_wire_matches_sdk_semantics() {
        assert_eq!(float_to_wire(100.0).unwrap(), "100");
        assert_eq!(float_to_wire(1670.1).unwrap(), "1670.1");
        assert_eq!(float_to_wire(0.0147).unwrap(), "0.0147");
        assert_eq!(float_to_wire(-0.0).unwrap(), "0");
        assert!(float_to_wire(0.123456789).is_err());
    }

    #[test]
    fn builder_dex_assets_start_at_110000_with_10000_stride() {
        assert_eq!(asset_id(None, 4), 4);
        assert_eq!(asset_id(Some(0), 0), 110_000);
        // EntropyIO ("io") is the tenth builder dex (index 9): io:ANTH sits at
        // universe index 1 and io:SNDK at index 2.
        assert_eq!(asset_id(Some(9), 1), 200_001);
        assert_eq!(asset_id(Some(9), 2), 200_002);
    }

    #[test]
    fn split_coin_keeps_the_canonical_name() {
        assert_eq!(split_coin("io:SNDK"), ("io", "io:SNDK"));
        assert_eq!(split_coin("BTC"), ("", "BTC"));
    }

    fn io_sndk_market() -> HyperliquidMarket {
        HyperliquidMarket {
            coin: "io:SNDK".into(),
            dex: "io".into(),
            asset: 200_002,
            sz_decimals: 4,
            max_leverage: 10,
            only_isolated: true,
        }
    }

    #[test]
    fn price_quantization_respects_decimals_and_significant_figures() {
        let market = io_sndk_market();
        // szDecimals=4 allows 2 decimals, but 1516.83 already has 6 significant
        // figures, so only one decimal survives.
        assert_eq!(market.quantize_price(1516.83, false).unwrap(), "1516.8");
        assert_eq!(market.quantize_price(1516.83, true).unwrap(), "1516.9");
        // Small prices keep both allowed decimals.
        assert_eq!(market.quantize_price(44.6172, false).unwrap(), "44.61");
        assert_eq!(market.quantize_price(44.6172, true).unwrap(), "44.62");
        // Values at or above 10^5 fall back to integers, which are always legal.
        assert_eq!(market.quantize_price(123_456.7, false).unwrap(), "123456");
        assert!(market.quantize_price(0.0, false).is_err());
    }

    #[test]
    fn size_quantization_floors_to_sz_decimals() {
        let market = io_sndk_market();
        assert_eq!(market.quantize_size(0.026_39).unwrap(), "0.0263");
        assert_eq!(market.quantize_size(2.0).unwrap(), "2");
        assert!(market.quantize_size(0.000_04).is_err());
    }

    #[test]
    fn wire_orders_reject_non_canonical_price_text() {
        let request = NewOrderRequest {
            asset: 200_002,
            is_buy: true,
            price: "1516.80".into(),
            size: "0.02".into(),
            reduce_only: false,
            tif: Tif::Alo,
            cloid: None,
        };
        assert!(request.to_wire().is_err());
    }

    #[test]
    fn cloids_are_deterministic_and_canonical() {
        let cloid = cloid_from_key("mq_io:SNDK_buy");
        validate_cloid(&cloid).unwrap();
        assert_eq!(cloid, cloid_from_key("mq_io:SNDK_buy"));
        assert_ne!(cloid, cloid_from_key("mq_io:SNDK_sell"));
        assert!(validate_cloid("0x123").is_err());
    }

    #[test]
    fn parses_bbo_events() {
        let event = HyperliquidWsEvent::parse(
            r#"{"channel":"bbo","data":{"coin":"io:SNDK","time":1787600000000,"bbo":[{"px":"1516.8","sz":"1.39","n":3},{"px":"1516.9","sz":"1.95","n":2}]}}"#,
        )
        .unwrap();
        let HyperliquidWsEvent::Bbo(update) = event else {
            panic!("expected bbo event");
        };
        assert_eq!(update.coin, "io:SNDK");
        assert_eq!(update.bid().unwrap().px, "1516.8");
        assert_eq!(update.ask().unwrap().px, "1516.9");
    }

    #[test]
    fn parses_order_update_events() {
        let event = HyperliquidWsEvent::parse(
            r#"{"channel":"orderUpdates","data":[{"order":{"coin":"io:SNDK","side":"B","limitPx":"1516.8","sz":"0.02","oid":123,"timestamp":1787600000000,"origSz":"0.02","cloid":"0x000102030405060708090a0b0c0d0e0f"},"status":"open","statusTimestamp":1787600000001}]}"#,
        )
        .unwrap();
        let HyperliquidWsEvent::OrderUpdates(updates) = event else {
            panic!("expected orderUpdates event");
        };
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].order.oid, 123);
        assert!(!order_update_is_terminal(&updates[0].status));
        assert!(order_update_is_terminal("canceled"));
    }

    #[test]
    fn parses_user_fill_events() {
        let event = HyperliquidWsEvent::parse(
            r#"{"channel":"userFills","data":{"isSnapshot":false,"user":"0xabc0000000000000000000000000000000000abc","fills":[{"coin":"io:SNDK","px":"1516.9","sz":"0.02","side":"A","time":1787600000002,"startPosition":"0.02","dir":"Close Long","closedPnl":"0.002","hash":"0x1","oid":124,"crossed":false,"fee":"0.0045","feeToken":"USDC","tid":998877}]}}"#,
        )
        .unwrap();
        let HyperliquidWsEvent::UserFills(fills) = event else {
            panic!("expected userFills event");
        };
        assert!(!fills.is_snapshot);
        assert_eq!(fills.fills[0].tid, 998_877);
        assert!(!fills.fills[0].is_buy());
    }

    #[test]
    fn parses_pong_and_subscription_ack() {
        assert_eq!(
            HyperliquidWsEvent::parse(r#"{"channel":"pong"}"#).unwrap(),
            HyperliquidWsEvent::Pong
        );
        assert_eq!(
            HyperliquidWsEvent::parse(
                r#"{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo","coin":"io:SNDK"}}}"#
            )
            .unwrap(),
            HyperliquidWsEvent::SubscriptionResponse
        );
    }

    #[test]
    fn nonces_are_strictly_monotonic() {
        let nonce = HyperliquidNonce::default();
        let first = nonce.next();
        let second = nonce.next();
        assert!(second > first);
    }

    #[test]
    fn user_fees_deserialize_add_and_cross_rates() {
        let fees: UserFees = serde_json::from_str(
            r#"{"userAddRate":"0.00015","userCrossRate":"0.00045","activeReferralDiscount":0.04}"#,
        )
        .expect("userFees");
        assert_eq!(fees.user_add_rate, "0.00015");
        assert_eq!(fees.user_cross_rate, "0.00045");
        let add: f64 = fees.user_add_rate.parse().unwrap();
        assert!(add > 0.0, "T0 maker is not the T4 zero add-rate");
    }
}
