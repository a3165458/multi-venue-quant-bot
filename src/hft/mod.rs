use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct BboUpdate {
    pub market_id: u32,
    pub symbol: String,
    pub nonce: u64,
    pub exchange_timestamp_ms: u64,
    pub bid_price: f64,
    pub bid_size: f64,
    pub ask_price: f64,
    pub ask_size: f64,
}

pub fn parse_bbo_update(message: &Value) -> Result<BboUpdate> {
    let channel = message["channel"]
        .as_str()
        .context("ticker message missing channel")?;
    let market_id = channel
        .strip_prefix("ticker:")
        .context("message is not a ticker update")?
        .parse::<u32>()
        .context("invalid ticker market id")?;
    let ticker = message
        .get("ticker")
        .context("ticker message missing payload")?;

    let parse_decimal = |value: &Value, field: &str| -> Result<f64> {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
            .with_context(|| format!("invalid {field}"))
    };
    let parse_u64 = |value: &Value, field: &str| -> Result<u64> {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
            .with_context(|| format!("invalid {field}"))
    };

    let bid_price = parse_decimal(&ticker["b"]["price"], "bid price")?;
    let bid_size = parse_decimal(&ticker["b"]["size"], "bid size")?;
    let ask_price = parse_decimal(&ticker["a"]["price"], "ask price")?;
    let ask_size = parse_decimal(&ticker["a"]["size"], "ask size")?;

    if bid_price <= 0.0 || ask_price <= 0.0 {
        bail!("ticker prices must be positive");
    }
    if bid_size < 0.0 || ask_size < 0.0 {
        bail!("ticker sizes cannot be negative");
    }
    if ask_price < bid_price {
        bail!("crossed ticker book");
    }

    Ok(BboUpdate {
        market_id,
        symbol: ticker["s"].as_str().unwrap_or_default().to_string(),
        nonce: parse_u64(&message["nonce"], "ticker nonce")?,
        exchange_timestamp_ms: parse_u64(&message["timestamp"], "ticker timestamp")?,
        bid_price,
        bid_size,
        ask_price,
        ask_size,
    })
}

/// Split market subscriptions into connection-sized shards while preserving
/// exchange market order.
pub fn plan_subscription_shards(
    market_ids: &[u32],
    max_subscriptions_per_connection: usize,
) -> Result<Vec<Vec<u32>>> {
    if max_subscriptions_per_connection == 0 {
        bail!("max subscriptions per connection must be greater than zero");
    }

    Ok(market_ids
        .chunks(max_subscriptions_per_connection)
        .map(|chunk| chunk.to_vec())
        .collect())
}

/// Conservative Standard-account budget.
///
/// Lighter documents 60 requests per rolling minute for Standard accounts.
/// Enforcing a one-second gap and refusing to accumulate idle capacity keeps
/// the client within that budget without allowing bursts.
#[derive(Debug, Default)]
pub struct StandardRateBudget {
    last_action_at: Option<Duration>,
}

impl StandardRateBudget {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn try_acquire_at(&mut self, now: Duration) -> bool {
        let allowed = self
            .last_action_at
            .map(|last| now.saturating_sub(last) >= Duration::from_secs(1))
            .unwrap_or(true);

        if allowed {
            self.last_action_at = Some(now);
        }

        allowed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookHealth {
    Syncing,
    Live,
    Halted,
}

/// Tracks the exchange matching-engine nonce for one market.
///
/// A halted book can only become live after a fresh snapshot. Deltas never
/// self-heal a detected gap.
#[derive(Debug)]
pub struct BookContinuity {
    health: BookHealth,
    last_nonce: Option<u64>,
}

impl Default for BookContinuity {
    fn default() -> Self {
        Self {
            health: BookHealth::Syncing,
            last_nonce: None,
        }
    }
}

impl BookContinuity {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_snapshot(&mut self, nonce: u64) -> BookHealth {
        self.last_nonce = Some(nonce);
        self.health = BookHealth::Live;
        self.health
    }

    pub fn apply_delta(&mut self, begin_nonce: u64, nonce: u64) -> BookHealth {
        if self.health != BookHealth::Live || self.last_nonce != Some(begin_nonce) {
            self.health = BookHealth::Halted;
            return self.health;
        }

        self.last_nonce = Some(nonce);
        self.health
    }

    pub fn last_nonce(&self) -> Option<u64> {
        self.last_nonce
    }
}
