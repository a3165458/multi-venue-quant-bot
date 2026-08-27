use anyhow::{bail, Result};
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::aster_shadow::{ShadowConfig, ShadowMakerMonitor, ShadowSnapshot};
use crate::lighter::types::{OrderType, Side, SignalAction, TradeSignal};

#[derive(Debug, Clone, Deserialize)]
pub struct HftProfileConfig {
    pub name: String,
    pub offset_bps: f64,
    pub requote_threshold_ticks: u64,
    pub cooldown_ms: u64,
}

#[derive(Debug, Clone)]
pub struct HftLabConfig {
    pub tick_size: f64,
    pub quote_notional: f64,
    pub penetration_bps: f64,
    pub fill_ratio: f64,
    pub toxicity_1s_bps: f64,
    pub toxicity_min_samples: u64,
    pub profiles: Vec<HftProfileConfig>,
}

#[derive(Debug, Clone, Default)]
struct QuoteState {
    last_target: Option<f64>,
    last_action_ms: u64,
}

struct ProfileRuntime {
    config: HftProfileConfig,
    monitor: ShadowMakerMonitor,
    buy: QuoteState,
    sell: QuoteState,
    toxic: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HftProfileSnapshot {
    pub name: String,
    pub offset_bps: f64,
    pub requote_threshold_ticks: u64,
    pub cooldown_ms: u64,
    pub toxic: bool,
    pub buy_price: Option<f64>,
    pub sell_price: Option<f64>,
    pub metrics: ShadowSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub struct HftLabSnapshot {
    pub enabled: bool,
    pub collecting: bool,
    pub updated_at_ms: u64,
    pub recommended_profile: Option<String>,
    pub recommendation_reason: String,
    pub profiles: Vec<HftProfileSnapshot>,
}

pub struct HftShadowLab {
    config: HftLabConfig,
    collecting: bool,
    profiles: Vec<ProfileRuntime>,
    updated_at_ms: u64,
}

impl HftShadowLab {
    pub fn new(config: HftLabConfig) -> Result<Self> {
        if !config.tick_size.is_finite() || config.tick_size <= 0.0 {
            bail!("HFT shadow tick_size must be positive");
        }
        if !config.quote_notional.is_finite() || config.quote_notional <= 0.0 {
            bail!("HFT shadow quote_notional must be positive");
        }
        if config.profiles.is_empty() {
            bail!("HFT shadow requires at least one profile");
        }
        if !config.toxicity_1s_bps.is_finite() || config.toxicity_1s_bps > 0.0 {
            bail!("HFT shadow toxicity_1s_bps must be finite and <= 0");
        }
        if config.toxicity_min_samples == 0 {
            bail!("HFT shadow toxicity_min_samples must be positive");
        }
        let mut names = HashSet::new();
        let mut profiles = Vec::with_capacity(config.profiles.len());
        for profile in &config.profiles {
            if profile.name.trim().is_empty() || !names.insert(profile.name.clone()) {
                bail!("HFT shadow profile names must be non-empty and unique");
            }
            if !profile.offset_bps.is_finite() || profile.offset_bps < 0.0 {
                bail!("HFT shadow offset_bps must be non-negative");
            }
            if profile.requote_threshold_ticks == 0 || profile.cooldown_ms == 0 {
                bail!("HFT shadow requote threshold and cooldown must be positive");
            }
            profiles.push(ProfileRuntime {
                config: profile.clone(),
                monitor: ShadowMakerMonitor::new(ShadowConfig {
                    penetration_bps: config.penetration_bps,
                    fill_ratio: config.fill_ratio,
                    markout_horizons_ms: vec![1_000, 5_000, 30_000],
                    max_recent_fills: 50,
                })?,
                buy: QuoteState::default(),
                sell: QuoteState::default(),
                toxic: false,
            });
        }
        Ok(Self {
            config,
            collecting: true,
            profiles,
            updated_at_ms: 0,
        })
    }

    pub fn observe_bbo(
        &mut self,
        symbol: &str,
        bid: f64,
        ask: f64,
        event_time_ms: u64,
        received_at_ms: u64,
    ) {
        if !self.collecting {
            return;
        }
        // Locked or crossed books are fail-closed: pull virtual quotes instead of
        // leaving a stale join sitting through the spread.
        if !(bid > 0.0 && ask > bid) {
            self.pull_all(symbol, received_at_ms);
            return;
        }
        self.updated_at_ms = self.updated_at_ms.max(received_at_ms);
        let tick_size = self.config.tick_size;
        let quote_notional = self.config.quote_notional;
        let toxicity_floor = self.config.toxicity_1s_bps;
        let toxicity_samples = self.config.toxicity_min_samples;
        for (index, profile) in self.profiles.iter_mut().enumerate() {
            profile
                .monitor
                .observe_bbo(symbol, bid, ask, event_time_ms, received_at_ms);
            if !profile.toxic
                && profile_is_toxic(
                    &profile.monitor.snapshot(received_at_ms),
                    toxicity_floor,
                    toxicity_samples,
                )
            {
                profile.toxic = true;
            }
            if profile.toxic {
                pull_profile(profile, symbol, received_at_ms);
                continue;
            }
            let offset = profile.config.offset_bps / 10_000.0;
            let buy_price = floor_to_tick(bid * (1.0 - offset), tick_size);
            let sell_price = ceil_to_tick(ask * (1.0 + offset), tick_size);
            if buy_price > 0.0 && buy_price < ask {
                update_side(
                    &mut profile.monitor,
                    &profile.config,
                    &mut profile.buy,
                    index,
                    symbol,
                    Side::Buy,
                    buy_price,
                    quote_notional / buy_price,
                    tick_size,
                    received_at_ms,
                );
            } else {
                withdraw_side(
                    &mut profile.monitor,
                    &mut profile.buy,
                    index,
                    symbol,
                    Side::Buy,
                    received_at_ms,
                );
            }
            if sell_price > 0.0 && sell_price > bid {
                update_side(
                    &mut profile.monitor,
                    &profile.config,
                    &mut profile.sell,
                    index,
                    symbol,
                    Side::Sell,
                    sell_price,
                    quote_notional / sell_price,
                    tick_size,
                    received_at_ms,
                );
            } else {
                withdraw_side(
                    &mut profile.monitor,
                    &mut profile.sell,
                    index,
                    symbol,
                    Side::Sell,
                    received_at_ms,
                );
            }
        }
    }

    fn pull_all(&mut self, symbol: &str, now_ms: u64) {
        self.updated_at_ms = self.updated_at_ms.max(now_ms);
        for profile in &mut self.profiles {
            pull_profile(profile, symbol, now_ms);
        }
    }

    pub fn observe_depth(
        &mut self,
        symbol: &str,
        bids: &[(f64, f64)],
        asks: &[(f64, f64)],
        event_time_ms: u64,
        received_at_ms: u64,
    ) {
        if !self.collecting {
            return;
        }
        self.updated_at_ms = self.updated_at_ms.max(received_at_ms);
        for profile in &mut self.profiles {
            profile
                .monitor
                .observe_depth(symbol, bids, asks, event_time_ms, received_at_ms);
        }
    }

    pub fn set_collecting(&mut self, collecting: bool, now_ms: u64) {
        if self.collecting == collecting {
            return;
        }
        self.collecting = collecting;
        self.updated_at_ms = self.updated_at_ms.max(now_ms);
        for profile in &mut self.profiles {
            profile.monitor.set_collecting(collecting, now_ms);
            profile.buy = QuoteState::default();
            profile.sell = QuoteState::default();
            profile.toxic = false;
        }
    }

    pub fn clear_symbol(&mut self, symbol: &str, now_ms: u64) {
        for profile in &mut self.profiles {
            profile.monitor.clear_symbol(symbol, now_ms);
            profile.buy = QuoteState::default();
            profile.sell = QuoteState::default();
        }
    }

    pub fn snapshot(&self, now_ms: u64) -> HftLabSnapshot {
        let profiles: Vec<HftProfileSnapshot> = self
            .profiles
            .iter()
            .map(|profile| HftProfileSnapshot {
                name: profile.config.name.clone(),
                offset_bps: profile.config.offset_bps,
                requote_threshold_ticks: profile.config.requote_threshold_ticks,
                cooldown_ms: profile.config.cooldown_ms,
                toxic: profile.toxic,
                buy_price: profile.buy.last_target,
                sell_price: profile.sell.last_target,
                metrics: profile.monitor.snapshot(now_ms),
            })
            .collect();
        let (recommended_profile, recommendation_reason) = recommend_profile(&profiles);
        HftLabSnapshot {
            enabled: true,
            collecting: self.collecting,
            updated_at_ms: self.updated_at_ms,
            recommended_profile,
            recommendation_reason,
            profiles,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn update_side(
    monitor: &mut ShadowMakerMonitor,
    config: &HftProfileConfig,
    state: &mut QuoteState,
    profile_index: usize,
    symbol: &str,
    side: Side,
    price: f64,
    quantity: f64,
    tick_size: f64,
    now_ms: u64,
) {
    let client_id = format!(
        "hft{}_{}_{}",
        profile_index,
        symbol,
        if side == Side::Buy { "buy" } else { "sell" }
    );
    if !monitor.has_active_quote(&client_id) {
        monitor.apply_signal(
            &quote_signal(symbol, side, price, quantity, &client_id, now_ms),
            now_ms,
        );
        state.last_target = Some(price);
        state.last_action_ms = now_ms;
        return;
    }
    let previous = state.last_target.unwrap_or(price);
    let moved_ticks = ((price - previous).abs() / tick_size).round() as u64;
    if moved_ticks < config.requote_threshold_ticks
        || now_ms.saturating_sub(state.last_action_ms) < config.cooldown_ms
    {
        return;
    }
    monitor.apply_signal(
        &quote_signal(symbol, side, price, quantity, &client_id, now_ms),
        now_ms,
    );
    state.last_target = Some(price);
    state.last_action_ms = now_ms;
}

fn withdraw_side(
    monitor: &mut ShadowMakerMonitor,
    state: &mut QuoteState,
    profile_index: usize,
    symbol: &str,
    side: Side,
    now_ms: u64,
) {
    let client_id = format!(
        "hft{}_{}_{}",
        profile_index,
        symbol,
        if side == Side::Buy { "buy" } else { "sell" }
    );
    if monitor.has_active_quote(&client_id) {
        monitor.apply_signal(
            &TradeSignal {
                action: SignalAction::Cancel,
                symbol: symbol.to_string(),
                market_id: 0,
                side,
                price: state.last_target.unwrap_or(0.0),
                quantity: 0.0,
                order_type: OrderType::Limit,
                reason: "HFT shadow safety pull".to_string(),
                timestamp: Utc
                    .timestamp_millis_opt(now_ms as i64)
                    .single()
                    .unwrap_or_else(Utc::now),
                expected_edge_bps: None,
                risk_reducing: true,
                post_only: false,
                client_id: Some(client_id),
            },
            now_ms,
        );
    }
    *state = QuoteState::default();
}

fn pull_profile(profile: &mut ProfileRuntime, symbol: &str, now_ms: u64) {
    profile.monitor.pull_quotes(symbol, now_ms);
    profile.buy = QuoteState::default();
    profile.sell = QuoteState::default();
}

fn quote_signal(
    symbol: &str,
    side: Side,
    price: f64,
    quantity: f64,
    client_id: &str,
    now_ms: u64,
) -> TradeSignal {
    TradeSignal {
        action: SignalAction::Place,
        symbol: symbol.to_string(),
        market_id: 0,
        side,
        price,
        quantity,
        order_type: OrderType::Limit,
        reason: "HFT shadow profile".to_string(),
        timestamp: Utc
            .timestamp_millis_opt(now_ms as i64)
            .single()
            .unwrap_or_else(Utc::now),
        expected_edge_bps: None,
        risk_reducing: false,
        post_only: true,
        client_id: Some(client_id.to_string()),
    }
}

fn recommend_profile(profiles: &[HftProfileSnapshot]) -> (Option<String>, String) {
    let mut ranked: Vec<&HftProfileSnapshot> = profiles
        .iter()
        .filter(|profile| !profile.toxic && profile.metrics.virtual_fills > 0)
        .collect();
    if ranked.is_empty() {
        let reason = if profiles.iter().any(|profile| profile.toxic) {
            "all profiles with fills are toxic; keeping quotes pulled"
        } else {
            "waiting for virtual fills before ranking profiles"
        };
        return (None, reason.to_string());
    }
    ranked.sort_by(|left, right| {
        markout_rank_bps(right)
            .partial_cmp(&markout_rank_bps(left))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                right
                    .metrics
                    .virtual_volume_per_hour
                    .partial_cmp(&left.metrics.virtual_volume_per_hour)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(
                left.metrics
                    .estimated_order_requests_per_minute
                    .partial_cmp(&right.metrics.estimated_order_requests_per_minute)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(left.name.cmp(&right.name))
    });
    let winner = ranked[0];
    (
        Some(winner.name.clone()),
        format!(
            "{} leads on 5s markout {:.2}bps with ${:.0}/h virtual volume",
            winner.name,
            markout_rank_bps(winner),
            winner.metrics.virtual_volume_per_hour
        ),
    )
}

fn profile_is_toxic(metrics: &ShadowSnapshot, floor_bps: f64, min_samples: u64) -> bool {
    metrics.markouts.iter().any(|markout| {
        markout.horizon_ms == 1_000
            && markout.samples >= min_samples
            && markout.mean_bps <= floor_bps
    })
}

fn markout_rank_bps(profile: &HftProfileSnapshot) -> f64 {
    markout_mean(&profile.metrics.markouts, 5_000)
        .or_else(|| markout_mean(&profile.metrics.markouts, 1_000))
        .unwrap_or(0.0)
}

fn markout_mean(markouts: &[crate::aster_shadow::ShadowMarkout], horizon_ms: u64) -> Option<f64> {
    markouts
        .iter()
        .find(|markout| markout.horizon_ms == horizon_ms && markout.samples > 0)
        .map(|markout| markout.mean_bps)
}

fn floor_to_tick(value: f64, tick: f64) -> f64 {
    ((value / tick) + 1e-9).floor() * tick
}

fn ceil_to_tick(value: f64, tick: f64) -> f64 {
    ((value / tick) - 1e-9).ceil() * tick
}
