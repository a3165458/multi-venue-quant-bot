use anyhow::{bail, Result};
use std::time::Duration;

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
