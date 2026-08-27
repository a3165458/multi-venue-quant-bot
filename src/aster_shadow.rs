use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};

use crate::lighter::types::{OpenOrderRef, Side, SignalAction, TradeSignal};

#[derive(Debug, Clone)]
pub struct ShadowConfig {
    pub penetration_bps: f64,
    pub fill_ratio: f64,
    pub markout_horizons_ms: Vec<u64>,
    pub max_recent_fills: usize,
}

#[derive(Debug, Clone)]
struct ShadowQuote {
    symbol: String,
    side: Side,
    price: f64,
    quantity: f64,
    placed_at_ms: u64,
    fill_armed: bool,
}

#[derive(Debug, Clone)]
struct PendingMarkout {
    symbol: String,
    side: Side,
    fill_price: f64,
    due_at_ms: u64,
    horizon_index: usize,
}

#[derive(Debug, Clone, Default)]
struct MarkoutAccumulator {
    samples: u64,
    sum_bps: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShadowMarkout {
    pub horizon_ms: u64,
    pub samples: u64,
    pub mean_bps: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShadowFill {
    pub timestamp_ms: u64,
    pub symbol: String,
    pub side: String,
    pub price: f64,
    pub quantity: f64,
    pub notional: f64,
    pub quote_age_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShadowSnapshot {
    pub enabled: bool,
    pub collecting: bool,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
    pub runtime_seconds: f64,
    pub bbo_events: u64,
    pub bbo_changes: u64,
    pub mean_event_lag_ms: f64,
    pub max_event_lag_ms: u64,
    pub depth_events: u64,
    pub mean_depth_lag_ms: f64,
    pub max_depth_lag_ms: u64,
    pub visible_queue_samples: u64,
    pub mean_queue_ahead_notional: f64,
    pub max_queue_ahead_notional: f64,
    pub depth_visibility_misses: u64,
    pub strategy_evaluations: u64,
    pub mean_strategy_eval_micros: f64,
    pub max_strategy_eval_micros: u64,
    pub quote_places: u64,
    pub quote_requotes: u64,
    pub quote_cancels: u64,
    pub active_quotes: usize,
    pub estimated_order_requests: u64,
    pub estimated_modify_requests: u64,
    pub modify_request_savings: u64,
    pub estimated_order_requests_per_minute: f64,
    pub virtual_fills: u64,
    pub virtual_quantity: f64,
    pub virtual_volume: f64,
    pub virtual_volume_per_hour: f64,
    pub virtual_positions: HashMap<String, f64>,
    pub markouts: Vec<ShadowMarkout>,
    pub recent_fills: Vec<ShadowFill>,
}

pub struct ShadowMakerMonitor {
    config: ShadowConfig,
    collecting: bool,
    started_at_ms: u64,
    updated_at_ms: u64,
    active_runtime_ms: u64,
    collecting_started_ms: u64,
    quotes: HashMap<String, ShadowQuote>,
    pending_markouts: Vec<PendingMarkout>,
    markouts: Vec<MarkoutAccumulator>,
    recent_fills: VecDeque<ShadowFill>,
    last_mid: HashMap<String, f64>,
    bbo_events: u64,
    bbo_changes: u64,
    event_lag_sum_ms: u128,
    max_event_lag_ms: u64,
    depth_events: u64,
    depth_lag_sum_ms: u128,
    max_depth_lag_ms: u64,
    visible_queue_samples: u64,
    queue_ahead_notional_sum: f64,
    max_queue_ahead_notional: f64,
    depth_visibility_misses: u64,
    strategy_evaluations: u64,
    strategy_eval_sum_micros: u128,
    max_strategy_eval_micros: u64,
    quote_places: u64,
    quote_requotes: u64,
    quote_cancels: u64,
    estimated_order_requests: u64,
    estimated_modify_requests: u64,
    virtual_fills: u64,
    virtual_quantity: f64,
    virtual_volume: f64,
    virtual_positions: HashMap<String, f64>,
}

impl ShadowMakerMonitor {
    pub fn new(mut config: ShadowConfig) -> Result<Self> {
        if !config.penetration_bps.is_finite() || config.penetration_bps < 0.0 {
            bail!("shadow penetration_bps must be finite and non-negative");
        }
        if !config.fill_ratio.is_finite() || config.fill_ratio <= 0.0 || config.fill_ratio > 1.0 {
            bail!("shadow fill_ratio must be in (0, 1]");
        }
        if config.max_recent_fills == 0 {
            bail!("shadow max_recent_fills must be positive");
        }
        config.markout_horizons_ms.sort_unstable();
        config.markout_horizons_ms.dedup();
        if config.markout_horizons_ms.is_empty() || config.markout_horizons_ms.contains(&0) {
            bail!("shadow markout horizons must be non-empty and positive");
        }
        let markouts = vec![MarkoutAccumulator::default(); config.markout_horizons_ms.len()];
        Ok(Self {
            config,
            collecting: true,
            started_at_ms: 0,
            updated_at_ms: 0,
            active_runtime_ms: 0,
            collecting_started_ms: 0,
            quotes: HashMap::new(),
            pending_markouts: Vec::new(),
            markouts,
            recent_fills: VecDeque::new(),
            last_mid: HashMap::new(),
            bbo_events: 0,
            bbo_changes: 0,
            event_lag_sum_ms: 0,
            max_event_lag_ms: 0,
            depth_events: 0,
            depth_lag_sum_ms: 0,
            max_depth_lag_ms: 0,
            visible_queue_samples: 0,
            queue_ahead_notional_sum: 0.0,
            max_queue_ahead_notional: 0.0,
            depth_visibility_misses: 0,
            strategy_evaluations: 0,
            strategy_eval_sum_micros: 0,
            max_strategy_eval_micros: 0,
            quote_places: 0,
            quote_requotes: 0,
            quote_cancels: 0,
            estimated_order_requests: 0,
            estimated_modify_requests: 0,
            virtual_fills: 0,
            virtual_quantity: 0.0,
            virtual_volume: 0.0,
            virtual_positions: HashMap::new(),
        })
    }

    pub fn apply_signal(&mut self, signal: &TradeSignal, now_ms: u64) {
        if !self.collecting {
            return;
        }
        if !signal.post_only && signal.action == SignalAction::Place {
            return;
        }
        let Some(client_id) = signal.client_id.as_ref() else {
            return;
        };
        self.touch(now_ms);
        match signal.action {
            SignalAction::Cancel => {
                if self.quotes.remove(client_id).is_some() {
                    self.quote_cancels += 1;
                    self.estimated_order_requests += 1;
                    self.estimated_modify_requests += 1;
                }
            }
            SignalAction::Place => {
                if let Some(existing) = self.quotes.get_mut(client_id) {
                    if (existing.price - signal.price).abs() <= f64::EPSILON {
                        return;
                    }
                    // Aster PUT /fapi/v3/order keeps the order on the book. Count the
                    // cancel+place counterfactual as two requests and the amend as one.
                    existing.symbol = signal.symbol.clone();
                    existing.side = signal.side;
                    existing.price = signal.price;
                    existing.quantity = signal.quantity;
                    existing.placed_at_ms = now_ms;
                    existing.fill_armed = true;
                    self.quote_requotes += 1;
                    self.estimated_order_requests += 2;
                    self.estimated_modify_requests += 1;
                    return;
                }
                self.quotes.insert(
                    client_id.clone(),
                    ShadowQuote {
                        symbol: signal.symbol.clone(),
                        side: signal.side,
                        price: signal.price,
                        quantity: signal.quantity,
                        placed_at_ms: now_ms,
                        fill_armed: true,
                    },
                );
                self.quote_places += 1;
                self.estimated_order_requests += 1;
                self.estimated_modify_requests += 1;
            }
        }
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
        if !(bid.is_finite() && ask.is_finite() && bid > 0.0 && ask >= bid) {
            return;
        }
        self.touch(received_at_ms);
        self.bbo_events += 1;
        let lag = received_at_ms.saturating_sub(event_time_ms);
        self.event_lag_sum_ms += u128::from(lag);
        self.max_event_lag_ms = self.max_event_lag_ms.max(lag);
        let mid = (bid + ask) / 2.0;
        if self
            .last_mid
            .insert(symbol.to_string(), mid)
            .is_some_and(|previous| (previous - mid).abs() > f64::EPSILON)
        {
            self.bbo_changes += 1;
        }
        self.resolve_markouts(symbol, mid, received_at_ms);

        let penetration = self.config.penetration_bps / 10_000.0;
        let mut fills = Vec::new();
        let mut completed = Vec::new();
        for (client_id, quote) in &mut self.quotes {
            if quote.symbol != symbol {
                continue;
            }
            let crossed = match quote.side {
                Side::Buy => ask <= quote.price * (1.0 - penetration),
                Side::Sell => bid >= quote.price * (1.0 + penetration),
            };
            if !crossed {
                quote.fill_armed = true;
                continue;
            }
            if !quote.fill_armed {
                continue;
            }
            let quantity = quote.quantity * self.config.fill_ratio;
            quote.quantity -= quantity;
            quote.fill_armed = false;
            fills.push((quote.clone(), quantity));
            if quote.quantity <= 1e-12 {
                completed.push(client_id.clone());
            }
        }
        for client_id in completed {
            self.quotes.remove(&client_id);
        }
        for (quote, quantity) in fills {
            self.virtual_fills += 1;
            self.virtual_quantity += quantity;
            self.virtual_volume += quote.price * quantity;
            *self
                .virtual_positions
                .entry(quote.symbol.clone())
                .or_default() += match quote.side {
                Side::Buy => quantity,
                Side::Sell => -quantity,
            };
            self.recent_fills.push_back(ShadowFill {
                timestamp_ms: received_at_ms,
                symbol: quote.symbol.clone(),
                side: side_label(quote.side).to_string(),
                price: quote.price,
                quantity,
                notional: quote.price * quantity,
                quote_age_ms: received_at_ms.saturating_sub(quote.placed_at_ms),
            });
            while self.recent_fills.len() > self.config.max_recent_fills {
                self.recent_fills.pop_front();
            }
            for (horizon_index, horizon_ms) in
                self.config.markout_horizons_ms.iter().copied().enumerate()
            {
                self.pending_markouts.push(PendingMarkout {
                    symbol: quote.symbol.clone(),
                    side: quote.side,
                    fill_price: quote.price,
                    due_at_ms: received_at_ms.saturating_add(horizon_ms),
                    horizon_index,
                });
            }
        }
    }

    pub fn record_strategy_eval(&mut self, elapsed_micros: u64) {
        if !self.collecting {
            return;
        }
        self.strategy_evaluations += 1;
        self.strategy_eval_sum_micros += u128::from(elapsed_micros);
        self.max_strategy_eval_micros = self.max_strategy_eval_micros.max(elapsed_micros);
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
        self.touch(received_at_ms);
        self.depth_events += 1;
        let lag = received_at_ms.saturating_sub(event_time_ms);
        self.depth_lag_sum_ms += u128::from(lag);
        self.max_depth_lag_ms = self.max_depth_lag_ms.max(lag);
        for quote in self.quotes.values().filter(|quote| quote.symbol == symbol) {
            let (visible, queue_ahead) = match quote.side {
                Side::Buy => {
                    let visible = bids
                        .iter()
                        .map(|(price, _)| *price)
                        .reduce(f64::min)
                        .is_some_and(|lowest| quote.price >= lowest);
                    let ahead = bids
                        .iter()
                        .filter(|(price, _)| *price >= quote.price)
                        .map(|(price, quantity)| price * quantity)
                        .sum();
                    (visible, ahead)
                }
                Side::Sell => {
                    let visible = asks
                        .iter()
                        .map(|(price, _)| *price)
                        .reduce(f64::max)
                        .is_some_and(|highest| quote.price <= highest);
                    let ahead = asks
                        .iter()
                        .filter(|(price, _)| *price <= quote.price)
                        .map(|(price, quantity)| price * quantity)
                        .sum();
                    (visible, ahead)
                }
            };
            if visible {
                self.visible_queue_samples += 1;
                self.queue_ahead_notional_sum += queue_ahead;
                self.max_queue_ahead_notional = self.max_queue_ahead_notional.max(queue_ahead);
            } else {
                self.depth_visibility_misses += 1;
            }
        }
    }

    pub fn open_order_refs(&self) -> Vec<OpenOrderRef> {
        if !self.collecting {
            return Vec::new();
        }
        self.quotes
            .iter()
            .map(|(client_id, quote)| OpenOrderRef {
                symbol: quote.symbol.clone(),
                client_id: Some(client_id.clone()),
                side: quote.side,
                price: quote.price,
                quantity: quote.quantity,
                status: "SHADOW".to_string(),
            })
            .collect()
    }

    pub fn has_active_quote(&self, client_id: &str) -> bool {
        self.quotes.contains_key(client_id)
    }

    pub fn pull_quotes(&mut self, symbol: &str, now_ms: u64) {
        if !self.collecting {
            return;
        }
        self.touch(now_ms);
        let ids: Vec<String> = self
            .quotes
            .iter()
            .filter(|(_, quote)| quote.symbol == symbol)
            .map(|(client_id, _)| client_id.clone())
            .collect();
        for client_id in ids {
            if self.quotes.remove(&client_id).is_some() {
                self.quote_cancels += 1;
                self.estimated_order_requests += 1;
                self.estimated_modify_requests += 1;
            }
        }
    }

    pub fn virtual_positions(&self) -> &HashMap<String, f64> {
        &self.virtual_positions
    }

    pub fn clear_symbol(&mut self, symbol: &str, now_ms: u64) {
        self.touch(now_ms);
        self.quotes.retain(|_, quote| quote.symbol != symbol);
        self.pending_markouts
            .retain(|markout| markout.symbol != symbol);
        self.virtual_positions.remove(symbol);
    }

    pub fn set_collecting(&mut self, collecting: bool, now_ms: u64) {
        if self.collecting == collecting {
            return;
        }
        if self.collecting {
            if self.collecting_started_ms > 0 {
                self.active_runtime_ms = self
                    .active_runtime_ms
                    .saturating_add(now_ms.saturating_sub(self.collecting_started_ms));
            }
            self.collecting_started_ms = 0;
            self.quotes.clear();
            self.pending_markouts.clear();
            self.virtual_positions.clear();
        } else {
            if self.started_at_ms == 0 {
                self.started_at_ms = now_ms;
            }
            self.collecting_started_ms = now_ms;
        }
        self.collecting = collecting;
        self.updated_at_ms = self.updated_at_ms.max(now_ms);
    }

    pub fn snapshot(&self, now_ms: u64) -> ShadowSnapshot {
        let active_runtime_ms = self.active_runtime_ms
            + if self.collecting && self.collecting_started_ms > 0 {
                now_ms.saturating_sub(self.collecting_started_ms)
            } else {
                0
            };
        let runtime_seconds = active_runtime_ms as f64 / 1_000.0;
        let runtime_minutes = runtime_seconds / 60.0;
        let runtime_hours = runtime_seconds / 3_600.0;
        ShadowSnapshot {
            enabled: true,
            collecting: self.collecting,
            started_at_ms: self.started_at_ms,
            updated_at_ms: self.updated_at_ms,
            runtime_seconds,
            bbo_events: self.bbo_events,
            bbo_changes: self.bbo_changes,
            mean_event_lag_ms: mean_u128(self.event_lag_sum_ms, self.bbo_events),
            max_event_lag_ms: self.max_event_lag_ms,
            depth_events: self.depth_events,
            mean_depth_lag_ms: mean_u128(self.depth_lag_sum_ms, self.depth_events),
            max_depth_lag_ms: self.max_depth_lag_ms,
            visible_queue_samples: self.visible_queue_samples,
            mean_queue_ahead_notional: if self.visible_queue_samples == 0 {
                0.0
            } else {
                self.queue_ahead_notional_sum / self.visible_queue_samples as f64
            },
            max_queue_ahead_notional: self.max_queue_ahead_notional,
            depth_visibility_misses: self.depth_visibility_misses,
            strategy_evaluations: self.strategy_evaluations,
            mean_strategy_eval_micros: mean_u128(
                self.strategy_eval_sum_micros,
                self.strategy_evaluations,
            ),
            max_strategy_eval_micros: self.max_strategy_eval_micros,
            quote_places: self.quote_places,
            quote_requotes: self.quote_requotes,
            quote_cancels: self.quote_cancels,
            active_quotes: self.quotes.len(),
            estimated_order_requests: self.estimated_order_requests,
            estimated_modify_requests: self.estimated_modify_requests,
            modify_request_savings: self
                .estimated_order_requests
                .saturating_sub(self.estimated_modify_requests),
            estimated_order_requests_per_minute: if runtime_minutes > 0.0 {
                self.estimated_order_requests as f64 / runtime_minutes
            } else {
                0.0
            },
            virtual_fills: self.virtual_fills,
            virtual_quantity: self.virtual_quantity,
            virtual_volume: self.virtual_volume,
            virtual_volume_per_hour: if runtime_hours > 0.0 {
                self.virtual_volume / runtime_hours
            } else {
                0.0
            },
            virtual_positions: self.virtual_positions.clone(),
            markouts: self
                .config
                .markout_horizons_ms
                .iter()
                .copied()
                .zip(&self.markouts)
                .map(|(horizon_ms, accumulator)| ShadowMarkout {
                    horizon_ms,
                    samples: accumulator.samples,
                    mean_bps: if accumulator.samples == 0 {
                        0.0
                    } else {
                        accumulator.sum_bps / accumulator.samples as f64
                    },
                })
                .collect(),
            recent_fills: self.recent_fills.iter().cloned().collect(),
        }
    }

    fn resolve_markouts(&mut self, symbol: &str, mid: f64, now_ms: u64) {
        let mut pending = Vec::with_capacity(self.pending_markouts.len());
        for markout in self.pending_markouts.drain(..) {
            if markout.symbol != symbol || now_ms < markout.due_at_ms {
                pending.push(markout);
                continue;
            }
            let bps = match markout.side {
                Side::Buy => (mid - markout.fill_price) / markout.fill_price * 10_000.0,
                Side::Sell => (markout.fill_price - mid) / markout.fill_price * 10_000.0,
            };
            let accumulator = &mut self.markouts[markout.horizon_index];
            accumulator.samples += 1;
            accumulator.sum_bps += bps;
        }
        self.pending_markouts = pending;
    }

    fn touch(&mut self, now_ms: u64) {
        if self.started_at_ms == 0 {
            self.started_at_ms = now_ms;
        }
        if self.collecting && self.collecting_started_ms == 0 {
            self.collecting_started_ms = now_ms;
        }
        self.updated_at_ms = self.updated_at_ms.max(now_ms);
    }
}

fn mean_u128(sum: u128, count: u64) -> f64 {
    if count == 0 {
        0.0
    } else {
        sum as f64 / count as f64
    }
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}
