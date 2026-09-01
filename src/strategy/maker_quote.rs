//! Maker quote strategy — continuous two-sided resting quotes, post-only.
//!
//! Two quote modes:
//! - `mid_spread` (default): one buy and one sell, symmetric around mid at a
//!   configurable spread.
//! - `join_best`: rest exactly on the live best bid and best ask (no
//!   improvement, no backing off). This is the rebate-capture path for
//!   thin HIP-3 books such as entropy.io.
//!
//! All emitted signals carry `post_only: true` so the execution layer
//! submits them with ALO (add-liquidity-only): the order rests on the book
//! and is rejected by the exchange if it would take liquidity.
//!
//! Quote lifecycle (intent side — the execution layer reconciles):
//! - A quote is (re-)emitted only when the desired price moved by at least
//!   `requote_threshold_bps` from the last placed price, and at most once per
//!   `requote_cooldown_secs` (avoids REST churn on noisy BBO ticks).
//! - A quote is re-emitted immediately (no cooldown) when it is gone from the
//!   exchange (detected via `snapshot.open_orders` reconciliation, or a
//!   position delta when that view is unavailable, e.g. in backtests).
//! - Each side has a stable `client_id` (`mq_<symbol>_<side>`) so the
//!   execution layer can deduplicate (same price already resting → no-op) and
//!   replace (cancel old, place new).
//!
//! Inventory management: quote size on the inventory-accumulating side is
//! linearly scaled down between `soft_cap_notional` and `hard_cap_notional`
//! (in USD notional of net position), and blocked entirely at the hard cap.
//! The inventory-reducing side is never blocked, so the strategy always
//! provides an exit path. Default path is two-sided maker after a fill
//! (`flatten_only = false`): both ALO quotes stay up, the adding side sits
//! on BBO, the reducing side may sit one tick inside when the book is wide.
//! `flatten_only` cancels the adding side and exits reduce-only ALO
//! (far → mid, then cancel-and-replace ALO). Never IOC.
//!
//! Trend filter (optional): when `|mid - ema| / ema` exceeds
//! `trend_block_bps`, the counter-trend side stops quoting (e.g. no buy
//! quotes in a strong downtrend) while the with-trend side keeps quoting.
//!
//! Tight-book gate: when `min_book_spread_bps > 0` and the live BBO is
//! narrower than that, resting quotes are pulled. HIP-3 join_best fills
//! inside a 1–2 bps book are rebate-rich and edge-poor. `wide_book_size_mult`
//! scales quote notional up to that multiple as the book widens from
//! `min_book_spread_bps` to `2 × min_book_spread_bps`.

use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, Timelike, Utc, Weekday};
use chrono_tz::America::New_York;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use tracing::{debug, info};

use super::Strategy;
use crate::lighter::types::*;

/// 单边报价的稳定 client_id 前缀（拼接后满足 Arcus 1-36 字符限制）
const CLIENT_ID_PREFIX: &str = "mq";

/// Where the resting quotes sit relative to the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteMode {
    /// Mid ± half of `spread_bps` (optionally skewed / vol-widened).
    MidSpread,
    /// Join the current best bid and best ask. Never improve, never cross.
    JoinBest,
}

impl QuoteMode {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "mid_spread" | "spread" | "mid" => Ok(Self::MidSpread),
            "join_best" | "join" | "bbo" | "inside" => Ok(Self::JoinBest),
            other => bail!("quote_mode must be mid_spread or join_best (got {other})"),
        }
    }
}

/// Maker Quote Trading Strategy
pub struct MakerQuoteStrategy {
    /// 基础双边总价差（bps）。单边报价相对 mid 偏移 half = spread_bps/2。
    /// 自适应模式开启时，实际价差 = spread_bps + vol_multiplier * 近期波动（bps）。
    spread_bps: f64,
    /// 单边报价名义金额（USD）
    per_quote_notional: f64,
    /// mid 移动超过该阈值（bps）才重报价
    requote_threshold_bps: f64,
    /// 两次重报价之间的最短间隔（秒）
    requote_cooldown_secs: i64,
    /// 库存软上限（USD 名义）——超过后同向报价缩量/偏斜
    soft_cap_notional: f64,
    /// 库存硬上限（USD 名义）——达到后封锁同向报价
    hard_cap_notional: f64,
    /// 是否启用 EMA 趋势过滤
    trend_filter: bool,
    /// EMA 周期
    ema_period: usize,
    /// 强趋势阈值（bps）：|mid-ema|/ema 超过后封锁逆势侧
    trend_block_bps: f64,
    /// 缩量后低于该名义金额（USD）的报价直接丢弃
    min_quote_notional: f64,
    /// 自适应价差：波动窗口（根K线数）。0 = 关闭（固定 spread_bps）。
    vol_window: usize,
    /// 自适应价差：波动乘数 k，实际价差 = spread_bps + k * 波动(bps)。
    vol_multiplier: f64,
    /// 库存价格偏斜上限（bps）。0 = 关闭（沿用数量线性缩量）。
    /// 开启后软区间内数量保持满额，改为把累库侧价格推远、减库侧价格拉近。
    max_skew_bps: f64,
    /// 全部市场单边报价风险预算（USD）。0 = 关闭（每市场用 per_quote_notional）。
    /// 开启后每市场单边名义 = min(per_quote_notional, budget / 报价市场数)。
    total_quote_budget: f64,
    /// Minimum event-time interval between EMA/volatility samples.
    feature_interval_secs: i64,
    /// Per-event mid-price jump threshold.
    jump_circuit_breaker_bps: f64,
    /// Minimum quoted BBO width. 0 = disabled (always quote).
    min_book_spread_bps: f64,
    /// Maximum quoted BBO width before all maker quotes are withdrawn.
    max_book_spread_bps: f64,
    /// Size multiplier at `2 × min_book_spread_bps` (1 = off).
    wide_book_size_mult: f64,
    /// Pull both quotes when BBO notional imbalance exceeds this ratio. 0 = off.
    max_bbo_imbalance: f64,
    /// When true, two-sided quotes only while flat. A fill switches to
    /// reduce-only flatten (far ALO → mid ALO, then cancel-and-replace ALO)
    /// and never adds.
    flatten_only: bool,
    /// If the live spread is wider than this many ticks, improve 1 tick
    /// inside mid instead of joining the far touch. 0 = always join BBO.
    join_inside_ticks: i64,
    /// Seconds after a fill before the flatten quote improves to mid.
    flatten_mid_secs: i64,
    /// ALO cancel-and-replace timeout after a flatten quote sits unfilled.
    /// Kept as flatten_ioc_secs for config compatibility. Never used as IOC/TIF.
    flatten_ioc_secs: i64,
    /// Event-time cooldown after a market circuit trip.
    circuit_breaker_cooldown_secs: i64,
    cash_open_guard: bool,
    cash_open_guard_before_minutes: i64,
    cash_open_guard_after_minutes: i64,
    quote_mode: QuoteMode,
    states: Mutex<HashMap<String, MakerState>>,
}

/// 单边报价的本地镜像（strategy 侧的去重依据）
#[derive(Debug, Clone)]
struct Quote {
    client_id: String,
    price: f64,
    quantity: f64,
    last_action: DateTime<Utc>,
    /// True only after the execution layer exposes the order in an authoritative snapshot.
    confirmed: bool,
    /// Set when an authoritative snapshot shows a previously confirmed quote
    /// has left the book. `desired_quote` must still honor requote cooldown
    /// even at the same price, otherwise join-best HIP-3 fills immediately
    /// re-place and burn the L1 request budget.
    gone: bool,
}

#[derive(Debug)]
struct MakerState {
    price_history: Vec<f64>,
    ema: f64,
    buy: Option<Quote>,
    sell: Option<Quote>,
    last_feature_bucket: i64,
    last_position: f64,
    circuit_until: Option<DateTime<Utc>>,
    inventory_since: Option<DateTime<Utc>>,
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn cancel_quotes_immediately(
    signals: &mut Vec<TradeSignal>,
    state: &mut MakerState,
    symbol: &str,
    market_id: u32,
    timestamp: DateTime<Utc>,
    reason: &str,
) {
    for (side, quote) in [
        (Side::Buy, state.buy.take()),
        (Side::Sell, state.sell.take()),
    ] {
        let Some(quote) = quote else {
            continue;
        };
        signals.push(TradeSignal {
            action: SignalAction::Cancel,
            symbol: symbol.to_string(),
            market_id,
            side,
            price: quote.price,
            quantity: quote.quantity,
            order_type: OrderType::Limit,
            reason: format!("Cancel maker {}: {reason}", side_label(side)),
            timestamp,
            expected_edge_bps: None,
            risk_reducing: true,
            post_only: false,
            client_id: Some(quote.client_id),
        });
    }
}

impl MakerQuoteStrategy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        spread_bps: f64,
        per_quote_notional: f64,
        requote_threshold_bps: f64,
        requote_cooldown_secs: i64,
        soft_cap_notional: f64,
        hard_cap_notional: f64,
        trend_filter: bool,
        ema_period: usize,
        trend_block_bps: f64,
        min_quote_notional: f64,
    ) -> Result<Self> {
        if !spread_bps.is_finite() || spread_bps <= 0.0 {
            bail!("spread_bps 必须 > 0（收到 {spread_bps}）");
        }
        if !per_quote_notional.is_finite() || per_quote_notional <= 0.0 {
            bail!("per_quote_notional 必须 > 0（收到 {per_quote_notional}）");
        }
        if !requote_threshold_bps.is_finite() || requote_threshold_bps < 0.0 {
            bail!("requote_threshold_bps 不能为负（收到 {requote_threshold_bps}）");
        }
        if requote_cooldown_secs < 0 {
            bail!("requote_cooldown_secs 不能为负（收到 {requote_cooldown_secs}）");
        }
        if !soft_cap_notional.is_finite()
            || !hard_cap_notional.is_finite()
            || hard_cap_notional <= 0.0
            || soft_cap_notional <= 0.0
            || hard_cap_notional <= soft_cap_notional
        {
            bail!("要求 hard_cap_notional > soft_cap_notional > 0（收到 soft={soft_cap_notional}, hard={hard_cap_notional}）");
        }
        if !trend_block_bps.is_finite() || trend_block_bps < 0.0 {
            bail!("trend_block_bps 不能为负（收到 {trend_block_bps}）");
        }
        if !min_quote_notional.is_finite() || min_quote_notional <= 0.0 {
            bail!("min_quote_notional 必须 > 0（收到 {min_quote_notional}）");
        }
        let ema_period = ema_period.max(1);
        Ok(Self {
            spread_bps,
            per_quote_notional,
            requote_threshold_bps,
            requote_cooldown_secs,
            soft_cap_notional,
            hard_cap_notional,
            trend_filter,
            ema_period,
            trend_block_bps,
            min_quote_notional,
            vol_window: 0,
            vol_multiplier: 0.0,
            max_skew_bps: 0.0,
            total_quote_budget: 0.0,
            feature_interval_secs: 1,
            jump_circuit_breaker_bps: 0.0,
            min_book_spread_bps: 0.0,
            max_book_spread_bps: 0.0,
            wide_book_size_mult: 1.0,
            max_bbo_imbalance: 0.0,
            flatten_only: false,
            join_inside_ticks: 0,
            flatten_mid_secs: 6,
            flatten_ioc_secs: 90,
            circuit_breaker_cooldown_secs: 0,
            cash_open_guard: false,
            cash_open_guard_before_minutes: 0,
            cash_open_guard_after_minutes: 0,
            quote_mode: QuoteMode::MidSpread,
            states: Mutex::new(HashMap::new()),
        })
    }

    pub fn with_quote_mode(mut self, quote_mode: QuoteMode) -> Result<Self> {
        self.quote_mode = quote_mode;
        Ok(self)
    }

    /// 开启自适应价差：实际价差 = spread_bps + vol_multiplier * 近期波动(bps)。
    /// `vol_window` 为滚动波动窗口（根K线数），0/1 = 关闭。
    pub fn with_adaptive_spread(mut self, vol_window: usize, vol_multiplier: f64) -> Result<Self> {
        if vol_window > 200 {
            bail!("vol_window 不能超过 200（收到 {vol_window}）");
        }
        if !vol_multiplier.is_finite() || vol_multiplier < 0.0 {
            bail!("vol_multiplier 必须 >= 0（收到 {vol_multiplier}）");
        }
        self.vol_window = if vol_window >= 2 { vol_window } else { 0 };
        self.vol_multiplier = vol_multiplier;
        Ok(self)
    }

    /// 开启库存价格偏斜：软区间内数量保持满额，价格按库存比例偏斜——
    /// 累库侧推远、减库侧拉近，主动把库存带回零。硬上限仍封锁累库侧。
    pub fn with_inventory_skew(mut self, max_skew_bps: f64) -> Result<Self> {
        if !max_skew_bps.is_finite() || max_skew_bps < 0.0 {
            bail!("max_skew_bps 必须 >= 0（收到 {max_skew_bps}）");
        }
        self.max_skew_bps = max_skew_bps;
        Ok(self)
    }

    /// 开启全市场单边风险预算：每市场单边名义 = min(per_quote_notional, budget/市场数)。
    /// 0 = 关闭。
    pub fn with_quote_budget(mut self, total_quote_budget: f64) -> Result<Self> {
        if !total_quote_budget.is_finite() || total_quote_budget < 0.0 {
            bail!("total_quote_budget 必须 >= 0（收到 {total_quote_budget}）");
        }
        self.total_quote_budget = total_quote_budget;
        Ok(self)
    }

    /// Sample EMA/volatility once per event-time bucket.
    pub fn with_feature_interval(mut self, feature_interval_secs: i64) -> Result<Self> {
        if feature_interval_secs <= 0 {
            bail!("feature_interval_secs 必须 > 0（收到 {feature_interval_secs}）");
        }
        self.feature_interval_secs = feature_interval_secs;
        Ok(self)
    }

    /// Enable per-market jump and wide-book circuit breakers.
    pub fn with_market_circuit_breaker(
        mut self,
        jump_circuit_breaker_bps: f64,
        max_book_spread_bps: f64,
        cooldown_secs: i64,
    ) -> Result<Self> {
        if !jump_circuit_breaker_bps.is_finite() || jump_circuit_breaker_bps <= 0.0 {
            bail!("jump_circuit_breaker_bps 必须 > 0（收到 {jump_circuit_breaker_bps}）");
        }
        if !max_book_spread_bps.is_finite() || max_book_spread_bps <= 0.0 {
            bail!("max_book_spread_bps 必须 > 0（收到 {max_book_spread_bps}）");
        }

        if cooldown_secs <= 0 {
            bail!("circuit_breaker_cooldown_secs 必须 > 0（收到 {cooldown_secs}）");
        }
        self.jump_circuit_breaker_bps = jump_circuit_breaker_bps;
        self.max_book_spread_bps = max_book_spread_bps;
        self.circuit_breaker_cooldown_secs = cooldown_secs;
        Ok(self)
    }

    /// Skip (and cancel) quotes when the live BBO is tighter than this.
    /// 0 disables the gate. Must stay below `max_book_spread_bps` when the
    /// wide-book circuit breaker is on.
    pub fn with_min_book_spread(mut self, min_book_spread_bps: f64) -> Result<Self> {
        if !min_book_spread_bps.is_finite() || min_book_spread_bps < 0.0 {
            bail!("min_book_spread_bps 必须 >= 0（收到 {min_book_spread_bps}）");
        }
        if self.max_book_spread_bps > 0.0
            && min_book_spread_bps > 0.0
            && min_book_spread_bps >= self.max_book_spread_bps
        {
            bail!(
                "min_book_spread_bps 必须 < max_book_spread_bps（收到 min={min_book_spread_bps}, max={})",
                self.max_book_spread_bps
            );
        }
        self.min_book_spread_bps = min_book_spread_bps;
        Ok(self)
    }

    /// Scale quote notional up to this multiple as the book widens from
    /// `min_book_spread_bps` to twice that. 1 disables scaling. No-op when
    /// the min-spread gate is off.
    pub fn with_wide_book_size_mult(mut self, wide_book_size_mult: f64) -> Result<Self> {
        if !wide_book_size_mult.is_finite() || !(1.0..=4.0).contains(&wide_book_size_mult) {
            bail!("wide_book_size_mult 必须在 1..=4（收到 {wide_book_size_mult}）");
        }
        self.wide_book_size_mult = wide_book_size_mult;
        Ok(self)
    }

    /// Cancel quotes when best-bid notional and best-ask notional differ by
    /// more than this ratio (e.g. 6 = skip a 6:1 one-sided book). 0 disables.
    pub fn with_max_bbo_imbalance(mut self, max_bbo_imbalance: f64) -> Result<Self> {
        if !max_bbo_imbalance.is_finite() || max_bbo_imbalance < 0.0 {
            bail!("max_bbo_imbalance 必须 >= 0（收到 {max_bbo_imbalance}）");
        }
        self.max_bbo_imbalance = max_bbo_imbalance;
        Ok(self)
    }

    /// Empty-book two-sided quotes; a fill switches to reduce-only flatten.
    pub fn with_flatten_cycle(
        mut self,
        flatten_only: bool,
        join_inside_ticks: i64,
        flatten_mid_secs: i64,
        flatten_ioc_secs: i64,
    ) -> Result<Self> {
        if join_inside_ticks < 0 {
            bail!("join_inside_ticks 必须 >= 0（收到 {join_inside_ticks}）");
        }
        if flatten_mid_secs < 0 || flatten_ioc_secs < 0 || flatten_ioc_secs < flatten_mid_secs {
            bail!(
                "flatten timers must satisfy 0 <= flatten_mid_secs <= flatten_ioc_secs (got mid={flatten_mid_secs}, ioc={flatten_ioc_secs})"
            );
        }
        self.flatten_only = flatten_only;
        self.join_inside_ticks = join_inside_ticks;
        self.flatten_mid_secs = flatten_mid_secs;
        self.flatten_ioc_secs = flatten_ioc_secs;
        Ok(self)
    }

    fn inferred_tick(bid: f64, ask: f64) -> f64 {
        for exp in 0..=8 {
            let tick = 10f64.powi(-exp);
            let fits = |price: f64| {
                let steps = (price / tick).round();
                (steps * tick - price).abs() <= tick * 1e-6 + 1e-12
            };
            if fits(bid) && fits(ask) {
                return tick.max(1e-8);
            }
        }
        ((ask - bid).abs() / 2.0).max(1e-8)
    }

    fn floor_tick(price: f64, tick: f64) -> f64 {
        if tick <= 0.0 {
            return price;
        }
        (price / tick + 1e-12).floor() * tick
    }

    fn ceil_tick(price: f64, tick: f64) -> f64 {
        if tick <= 0.0 {
            return price;
        }
        (price / tick - 1e-12).ceil() * tick
    }

    fn join_prices(&self, bid: f64, ask: f64, mid: f64, position: f64) -> (f64, f64) {
        if self.join_inside_ticks <= 0 || ask <= bid {
            return (bid, ask);
        }
        let tick = Self::inferred_tick(bid, ask);
        let spread_ticks = (ask - bid) / tick;
        if spread_ticks <= self.join_inside_ticks as f64 + 1e-9 {
            return (bid, ask);
        }
        let inside_buy = Self::floor_tick(mid - tick, tick).max(bid);
        let inside_sell = Self::ceil_tick(mid + tick, tick).min(ask);
        let (mut buy, mut sell) = if inside_buy >= inside_sell {
            (bid, ask)
        } else {
            (inside_buy, inside_sell)
        };
        // Two-sided MM: after a fill keep both ALO quotes. Do not improve the
        // inventory-adding side (stay on BBO); the reducing side may sit one
        // tick inside. Never cross.
        if !self.flatten_only && position.abs() > 1e-12 {
            if position > 0.0 {
                buy = bid;
                sell = inside_sell.max(bid + tick);
            } else {
                sell = ask;
                buy = inside_buy.min(ask - tick);
            }
            if buy >= sell {
                return (bid, ask);
            }
        }
        (buy, sell)
    }

    /// ALO / post-only must rest. A buy at or through the ask (or a sell at
    /// or through the bid) would take immediately and get rejected, then the
    /// live loop retries and burns the HIP-3 L1 request budget.
    fn alo_would_take(price: f64, side: Side, bid: f64, ask: f64) -> bool {
        match side {
            Side::Buy => price >= ask,
            Side::Sell => price <= bid,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn flatten_inventory(
        &self,
        signals: &mut Vec<TradeSignal>,
        state: &mut MakerState,
        symbol: &str,
        ob: &OrderBook,
        position: f64,
        mid: f64,
        now: DateTime<Utc>,
        quote_notional: f64,
    ) {
        let (Some(bid), Some(ask)) = (ob.bids.first(), ob.asks.first()) else {
            cancel_quotes_immediately(
                signals,
                state,
                symbol,
                ob.market_id,
                now,
                "flatten: missing BBO",
            );
            return;
        };
        if !(bid.price > 0.0 && ask.price > bid.price) {
            cancel_quotes_immediately(
                signals,
                state,
                symbol,
                ob.market_id,
                now,
                "flatten: invalid BBO",
            );
            return;
        }
        let long = position > 0.0;
        let reduce_side = if long { Side::Sell } else { Side::Buy };
        let add_side = if long { Side::Buy } else { Side::Sell };
        if let Some(quote) = match add_side {
            Side::Buy => state.buy.take(),
            Side::Sell => state.sell.take(),
        } {
            signals.push(TradeSignal {
                action: SignalAction::Cancel,
                symbol: symbol.to_string(),
                market_id: ob.market_id,
                side: add_side,
                price: quote.price,
                quantity: quote.quantity,
                order_type: OrderType::Limit,
                reason: format!("Cancel maker {}: flatten-only", side_label(add_side)),
                timestamp: now,
                expected_edge_bps: None,
                risk_reducing: true,
                post_only: false,
                client_id: Some(quote.client_id),
            });
        }

        let tick = Self::inferred_tick(bid.price, ask.price);
        let age = state
            .inventory_since
            .map(|since| now.signed_duration_since(since))
            .unwrap_or_else(Duration::zero);
        // Never IOC-flatten. 08-27 HIP-3 taker closes were -21.8 bps vs maker
        // -4.4 bps; taking the touch to unwind is worse than waiting on ALO.
        // flatten_ioc_secs is the ALO cancel-and-replace timeout (was IOC take).
        let improve = age >= Duration::seconds(self.flatten_mid_secs);
        let (price, stage) = if improve {
            let improved = if long {
                Self::ceil_tick(mid.max(bid.price + tick).min(ask.price), tick)
                    .clamp(bid.price + tick, ask.price)
            } else {
                Self::floor_tick(mid.min(ask.price - tick).max(bid.price), tick)
                    .clamp(bid.price, ask.price - tick)
            };
            (improved, "mid ALO")
        } else {
            (if long { ask.price } else { bid.price }, "far ALO")
        };
        let post_only = true;
        if !(price.is_finite() && price > 0.0) {
            return;
        }
        if Self::alo_would_take(price, reduce_side, bid.price, ask.price) {
            return;
        }
        let qty = if position.abs() * mid <= quote_notional.max(self.min_quote_notional) {
            position.abs()
        } else {
            (quote_notional / price).min(position.abs())
        };
        let current = match reduce_side {
            Side::Buy => state.buy.as_ref(),
            Side::Sell => state.sell.as_ref(),
        };
        let entering = state.inventory_since == Some(now);
        let quote_age = current
            .map(|q| now.signed_duration_since(q.last_action))
            .unwrap_or_else(Duration::zero);
        let stale = current.is_some()
            && self.flatten_ioc_secs > 0
            && quote_age >= Duration::seconds(self.flatten_ioc_secs);
        if stale {
            if let Some(quote) = match reduce_side {
                Side::Buy => state.buy.take(),
                Side::Sell => state.sell.take(),
            } {
                signals.push(TradeSignal {
                    action: SignalAction::Cancel,
                    symbol: symbol.to_string(),
                    market_id: ob.market_id,
                    side: reduce_side,
                    price: quote.price,
                    quantity: quote.quantity,
                    order_type: OrderType::Limit,
                    reason: format!(
                        "Cancel flatten {}: stale ALO refresh after {}s",
                        side_label(reduce_side),
                        quote_age.num_seconds()
                    ),
                    timestamp: now,
                    expected_edge_bps: None,
                    risk_reducing: true,
                    post_only: true,
                    client_id: Some(quote.client_id),
                });
            }
        }
        let current = match reduce_side {
            Side::Buy => state.buy.as_ref(),
            Side::Sell => state.sell.as_ref(),
        };
        let should_emit = if stale || entering {
            current.is_none_or(|q| {
                entering || (q.price - price).abs() / price * 10_000.0 > 0.1
            })
        } else {
            self.desired_quote(price, qty, Some(1.0), now, current)
                .is_some()
        };
        if !should_emit {
            return;
        }
        let client_id = Self::client_id(symbol, reduce_side);
        let edge_bps = if long {
            (price - mid) / mid * 10_000.0
        } else {
            (mid - price) / mid * 10_000.0
        };
        signals.push(TradeSignal {
            action: SignalAction::Place,
            symbol: symbol.to_string(),
            market_id: ob.market_id,
            side: reduce_side,
            price,
            quantity: qty,
            order_type: OrderType::Limit,
            reason: format!(
                "Flatten {} @ {:.4} ({stage}, age {}s)",
                side_label(reduce_side),
                price,
                age.num_seconds()
            ),
            timestamp: now,
            expected_edge_bps: Some(edge_bps),
            risk_reducing: true,
            post_only,
            client_id: Some(client_id.clone()),
        });
        let quote = Quote {
            client_id,
            price,
            quantity: qty,
            last_action: now,
            confirmed: false,
            gone: false,
        };
        match reduce_side {
            Side::Buy => state.buy = Some(quote),
            Side::Sell => state.sell = Some(quote),
        }
    }

    fn bbo_imbalance_ratio(ob: &OrderBook) -> f64 {
        let (Some(bid), Some(ask)) = (ob.bids.first(), ob.asks.first()) else {
            return f64::INFINITY;
        };
        let bid_n = bid.price * bid.quantity;
        let ask_n = ask.price * ask.quantity;
        let thin = bid_n.min(ask_n);
        if thin <= 0.0 {
            return f64::INFINITY;
        }
        bid_n.max(ask_n) / thin
    }

    /// Heavy bid → skip joining the bid (buy). Heavy ask → skip sell.
    /// The thin side still quotes so volume is not zeroed by a two-sided pull.
    fn bbo_heavy_side(ob: &OrderBook, max_imbalance: f64) -> Option<Side> {
        if max_imbalance <= 0.0 {
            return None;
        }
        if Self::bbo_imbalance_ratio(ob) <= max_imbalance {
            return None;
        }
        let (Some(bid), Some(ask)) = (ob.bids.first(), ob.asks.first()) else {
            return None;
        };
        let bid_n = bid.price * bid.quantity;
        let ask_n = ask.price * ask.quantity;
        if bid_n >= ask_n {
            Some(Side::Buy)
        } else {
            Some(Side::Sell)
        }
    }

    fn book_spread_bps(ob: &OrderBook, mid: f64) -> f64 {
        match (ob.bids.first(), ob.asks.first()) {
            (Some(bid), Some(ask)) if bid.price > 0.0 && ask.price > bid.price && mid > 0.0 => {
                (ask.price - bid.price) / mid * 10_000.0
            }
            _ => f64::INFINITY,
        }
    }

    fn spread_size_mult(&self, book_spread_bps: f64) -> f64 {
        if self.min_book_spread_bps <= 0.0 || self.wide_book_size_mult <= 1.0 {
            return 1.0;
        }
        if !book_spread_bps.is_finite() {
            return 1.0;
        }
        let t = (book_spread_bps / self.min_book_spread_bps - 1.0).clamp(0.0, 1.0);
        1.0 + t * (self.wide_book_size_mult - 1.0)
    }

    /// Pause maker entries around the 09:30 America/New_York cash open.
    pub fn with_cash_open_guard(
        mut self,
        enabled: bool,
        before_minutes: i64,
        after_minutes: i64,
    ) -> Result<Self> {
        if !(0..=180).contains(&before_minutes) || !(0..=180).contains(&after_minutes) {
            bail!("cash-open guard minutes must be in 0..=180");
        }
        self.cash_open_guard = enabled;
        self.cash_open_guard_before_minutes = before_minutes;
        self.cash_open_guard_after_minutes = after_minutes;
        Ok(self)
    }

    fn cash_open_guard_active(&self, now: DateTime<Utc>) -> bool {
        if !self.cash_open_guard {
            return false;
        }
        let local = now.with_timezone(&New_York);
        if matches!(local.weekday(), Weekday::Sat | Weekday::Sun) {
            return false;
        }
        let minute = i64::from(local.hour()) * 60 + i64::from(local.minute());
        let open = 9 * 60 + 30;
        minute >= open - self.cash_open_guard_before_minutes
            && minute <= open + self.cash_open_guard_after_minutes
    }

    fn client_id(symbol: &str, side: Side) -> String {
        format!("{CLIENT_ID_PREFIX}_{symbol}_{}", side_label(side))
    }

    /// 库存缩放：返回同向开仓的缩放系数；`None` = 封锁该方向。
    /// `inventory_notional` 为净持仓名义（正=多）。
    /// 偏斜模式开启时软区间内保持满额（价格偏斜代替数量缩量），硬上限仍封锁。
    fn inventory_scale(&self, inventory_notional: f64) -> Option<f64> {
        if inventory_notional >= self.hard_cap_notional {
            None
        } else if inventory_notional <= self.soft_cap_notional || self.max_skew_bps > 0.0 {
            Some(1.0)
        } else {
            let span = self.hard_cap_notional - self.soft_cap_notional;
            Some((1.0 - (inventory_notional - self.soft_cap_notional) / span).max(0.0))
        }
    }

    fn resting_client_ids(snapshot: &MarketSnapshot, symbol: &str) -> HashSet<String> {
        snapshot
            .open_orders
            .iter()
            .filter(|o| o.symbol == symbol)
            .filter_map(|o| o.client_id.clone())
            .collect()
    }

    /// 近期已实现波动（bps/根）：最后 `vol_window` 根收盘价对数收益的标准差。
    fn realized_vol_bps(&self, history: &[f64]) -> f64 {
        let n = self.vol_window.min(history.len());
        if n < 2 {
            return 0.0;
        }
        let window = &history[history.len() - n..];
        let mut sum = 0.0;
        let mut sum_sq = 0.0;
        let mut prev = window[0];
        let mut count = 0;
        for &price in &window[1..] {
            let r = (price / prev).ln();
            prev = price;
            sum += r;
            sum_sq += r * r;
            count += 1;
        }
        if count == 0 {
            return 0.0;
        }
        let mean = sum / count as f64;
        let var = (sum_sq / count as f64 - mean * mean).max(0.0);
        var.sqrt() * 10_000.0
    }

    /// 当前生效的双边价差（bps）：自适应模式 = 基础 + 波动乘数 * 波动。
    fn effective_spread_bps(&self, vol_bps: f64) -> f64 {
        if self.vol_window > 0 {
            (self.spread_bps + self.vol_multiplier * vol_bps).max(1e-6)
        } else {
            self.spread_bps
        }
    }

    /// 计算某侧期望报价。返回 `Some((price, quantity, should_emit))`：
    /// - `None`：缩量后低于 dust，不报价；
    /// - `should_emit=false`：现价挂单仍在阈值内或处于冷却期，保留；
    /// - `should_emit=true`：新开或需重报价。
    ///
    /// 其中 `desired_price` 与 `base_qty` 由调用方（含自适应价差/偏斜/预算）算好传入。
    fn desired_quote(
        &self,
        desired_price: f64,
        base_qty: f64,
        scale: Option<f64>,
        now: DateTime<Utc>,
        current: Option<&Quote>,
    ) -> Option<(f64, f64)> {
        let qty = base_qty * scale.unwrap_or(1.0);
        if qty * desired_price < self.min_quote_notional {
            return None;
        }

        match current {
            None => Some((desired_price, qty)),
            Some(q) => {
                if now.signed_duration_since(q.last_action)
                    < Duration::seconds(self.requote_cooldown_secs)
                {
                    debug!(
                        "重报价冷却中，保留 {:.2}（期望 {:.2}）",
                        q.price, desired_price
                    );
                    return None;
                }
                if !q.gone {
                    let drift_bps = (desired_price - q.price).abs() / q.price * 10_000.0;
                    if drift_bps <= self.requote_threshold_bps {
                        return None;
                    }
                }
                Some((desired_price, qty))
            }
        }
    }
}

#[async_trait]
impl Strategy for MakerQuoteStrategy {
    fn name(&self) -> &str {
        "maker_quote"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        let mut all_signals = Vec::new();

        // 风险预算：全部报价市场单边名义合计不得超过 total_quote_budget。
        // 每市场单边名义 = min(per_quote_notional, budget / 市场数)。
        let n_markets = snapshot.order_books.len();
        let eff_per_market_notional = if self.total_quote_budget > 0.0 && n_markets > 0 {
            (self.total_quote_budget / n_markets as f64).min(self.per_quote_notional)
        } else {
            self.per_quote_notional
        };

        let mut order_books = snapshot.order_books.iter().collect::<Vec<_>>();
        order_books.sort_unstable_by_key(|(symbol, _)| symbol.as_str());
        for (symbol, ob) in order_books {
            // 冷却期以行情时间（订单簿时间戳）计：回测/测试确定性，实盘随行情推进
            let now = ob.timestamp;
            let mid = match ob.mid_price() {
                Some(p) if p > 0.0 => p,
                _ => continue,
            };

            let mut states = self.states.lock();
            let feature_bucket = now.timestamp() / self.feature_interval_secs;
            let state = states.entry(symbol.clone()).or_insert_with(|| {
                info!("Maker quote init: mid {:.2} for {}", mid, symbol);
                MakerState {
                    price_history: vec![mid],
                    ema: mid,
                    buy: None,
                    sell: None,
                    last_feature_bucket: feature_bucket,
                    last_position: snapshot.positions.get(symbol).copied().unwrap_or(0.0),
                    circuit_until: None,
                    inventory_since: None,
                }
            });
            let previous_mid = state.price_history.last().copied().unwrap_or(mid);

            if feature_bucket > state.last_feature_bucket {
                state.price_history.push(mid);
                if state.price_history.len() > 200 {
                    state.price_history.remove(0);
                }
                let alpha = 2.0 / (self.ema_period + 1) as f64;
                state.ema = alpha * mid + (1.0 - alpha) * state.ema;
                state.last_feature_bucket = feature_bucket;
            }

            // 持仓更新与成交探测：实盘用 open_orders 视图对账；回测用持仓变动
            let position = snapshot.positions.get(symbol).copied().unwrap_or(0.0);
            let pos_delta = position - state.last_position;
            state.last_position = position;
            if snapshot.open_orders_authoritative {
                let resting = Self::resting_client_ids(snapshot, symbol);
                for side in [Side::Buy, Side::Sell] {
                    let slot = match side {
                        Side::Buy => &mut state.buy,
                        Side::Sell => &mut state.sell,
                    };
                    if let Some(q) = slot.as_mut() {
                        if resting.contains(&q.client_id) {
                            q.confirmed = true;
                            continue;
                        }

                        let confirmation_timed_out = now.signed_duration_since(q.last_action)
                            >= Duration::seconds(self.requote_cooldown_secs);
                        if q.confirmed {
                            info!(
                                "{} {} 挂单已离场，冷却 {}s 后再挂",
                                symbol,
                                side_label(side),
                                self.requote_cooldown_secs
                            );
                            q.confirmed = false;
                            q.gone = true;
                        } else if confirmation_timed_out {
                            *slot = None;
                        }
                    }
                }
            } else if pos_delta.abs() > f64::EPSILON && !self.flatten_only {
                // 无 open_orders 视图（回测）→ 持仓变动视为某侧成交
                debug!("{} 持仓变动 {:.6} → 双侧重新报价", symbol, pos_delta);
                state.buy = None;
                state.sell = None;
            }

            if position.abs() <= 1e-12 {
                state.inventory_since = None;
            } else if state.inventory_since.is_none() {
                state.inventory_since = Some(now);
            }

            if self.flatten_only && position.abs() > 1e-12 {
                self.flatten_inventory(
                    &mut all_signals,
                    state,
                    symbol,
                    ob,
                    position,
                    mid,
                    now,
                    eff_per_market_notional,
                );
                continue;
            }

            if self.cash_open_guard_active(now) {
                cancel_quotes_immediately(
                    &mut all_signals,
                    state,
                    symbol,
                    ob.market_id,
                    ob.timestamp,
                    "cash-open guard",
                );
                continue;
            }

            let book_spread_bps = Self::book_spread_bps(ob, mid);
            if self.min_book_spread_bps > 0.0 && book_spread_bps < self.min_book_spread_bps {
                cancel_quotes_immediately(
                    &mut all_signals,
                    state,
                    symbol,
                    ob.market_id,
                    ob.timestamp,
                    "book tighter than min_book_spread",
                );
                continue;
            }
            let heavy_bbo = Self::bbo_heavy_side(ob, self.max_bbo_imbalance);
            let block_buy_obi = heavy_bbo == Some(Side::Buy);
            let block_sell_obi = heavy_bbo == Some(Side::Sell);

            if state
                .circuit_until
                .is_some_and(|circuit_until| now < circuit_until)
            {
                continue;
            }
            state.circuit_until = None;
            if self.jump_circuit_breaker_bps > 0.0 {
                let move_bps = (mid - previous_mid).abs() / previous_mid * 10_000.0;
                if move_bps >= self.jump_circuit_breaker_bps
                    || book_spread_bps >= self.max_book_spread_bps
                {
                    state.circuit_until =
                        Some(now + Duration::seconds(self.circuit_breaker_cooldown_secs));
                    cancel_quotes_immediately(
                        &mut all_signals,
                        state,
                        symbol,
                        ob.market_id,
                        ob.timestamp,
                        "market circuit breaker",
                    );
                    continue;
                }
            }

            // 库存与趋势
            let pos_notional = position.abs() * mid;
            let signed_pos_notional = position * mid;
            // 买单增加净多头；卖单增加净空头。
            let buy_scale = self.inventory_scale(signed_pos_notional);
            let sell_scale = self.inventory_scale(-signed_pos_notional);

            let trend_pct = if self.trend_filter {
                (mid - state.ema) / state.ema
            } else {
                0.0
            };
            let has_enough_history = state.price_history.len() >= self.ema_period.min(10);
            let block_buy_trend = self.trend_filter
                && has_enough_history
                && trend_pct < -(self.trend_block_bps / 10_000.0);
            let block_sell_trend = self.trend_filter
                && has_enough_history
                && trend_pct > self.trend_block_bps / 10_000.0;

            let vol_bps = self.realized_vol_bps(&state.price_history);
            let (buy_price, sell_price, spread_bps, skew_bps) = match self.quote_mode {
                QuoteMode::JoinBest => {
                    let (Some(bid), Some(ask)) = (ob.bids.first(), ob.asks.first()) else {
                        continue;
                    };
                    if !(bid.price > 0.0 && ask.price > bid.price) {
                        continue;
                    }
                    let (buy_price, sell_price) =
                        self.join_prices(bid.price, ask.price, mid, position);
                    (buy_price, sell_price, book_spread_bps, 0.0)
                }
                QuoteMode::MidSpread => {
                    let spread_bps = self.effective_spread_bps(vol_bps);
                    let half_spread_bps = spread_bps / 2.0;
                    let skew_bps = if self.max_skew_bps > 0.0 && pos_notional > 0.0 {
                        let ratio = (pos_notional / self.soft_cap_notional).min(1.0);
                        let raw = ratio * self.max_skew_bps;
                        if position >= 0.0 {
                            raw
                        } else {
                            -raw
                        }
                    } else {
                        0.0
                    };
                    // 减库侧偏移量最小保留 10% 的半价差，避免偏斜过大把价格推过 mid（ALO 会被拒）。
                    let min_offset_bps = (half_spread_bps * 0.1).max(0.1);
                    let buy_offset_bps = (half_spread_bps + skew_bps).max(min_offset_bps);
                    let sell_offset_bps = (half_spread_bps - skew_bps).max(min_offset_bps);
                    (
                        mid * (1.0 - buy_offset_bps / 10_000.0),
                        mid * (1.0 + sell_offset_bps / 10_000.0),
                        spread_bps,
                        skew_bps,
                    )
                }
            };
            let size_mult = self.spread_size_mult(book_spread_bps);
            let buy_qty = (eff_per_market_notional * size_mult) / buy_price;
            let sell_qty = (eff_per_market_notional * size_mult) / sell_price;
            let bid_px = ob.bids.first().map(|level| level.price).unwrap_or(0.0);
            let ask_px = ob.asks.first().map(|level| level.price).unwrap_or(0.0);
            let buy_would_take = Self::alo_would_take(buy_price, Side::Buy, bid_px, ask_px);
            let sell_would_take = Self::alo_would_take(sell_price, Side::Sell, bid_px, ask_px);

            // 出价侧
            if let Some(scale) = buy_scale.filter(|_| !block_buy_trend && !buy_would_take && !block_buy_obi) {
                if let Some((price, qty)) =
                    self.desired_quote(buy_price, buy_qty, Some(scale), now, state.buy.as_ref())
                {
                    let client_id = Self::client_id(symbol, Side::Buy);
                    let risk_reducing = position < 0.0 && qty <= position.abs() + f64::EPSILON;
                    let edge_bps = (mid - price) / mid * 10_000.0;
                    all_signals.push(TradeSignal {
                        action: SignalAction::Place,
                        symbol: symbol.clone(),
                        market_id: ob.market_id,
                        side: Side::Buy,
                        price,
                        quantity: qty,
                        order_type: OrderType::Limit,
                        reason: format!(
                            "Maker Buy @ {:.2} (spread {:.1}bps, skew {:+.1}bps, vol {:.1}bps)",
                            price, spread_bps, skew_bps, vol_bps
                        ),
                        timestamp: ob.timestamp,
                        expected_edge_bps: Some(edge_bps),
                        risk_reducing,
                        post_only: true,
                        client_id: Some(client_id.clone()),
                    });
                    state.buy = Some(Quote {
                        client_id,
                        price,
                        quantity: qty,
                        last_action: now,
                        confirmed: false,
                        gone: false,
                    });
                }
            } else {
                let reason = if buy_would_take {
                    "ALO would take"
                } else if block_buy_obi {
                    "BBO notional imbalance"
                } else if block_buy_trend {
                    "strong downtrend"
                } else {
                    "long inventory hard cap"
                };
                if let Some(quote) = state.buy.as_mut() {
                    if now.signed_duration_since(quote.last_action)
                        >= Duration::seconds(self.requote_cooldown_secs)
                    {
                        all_signals.push(TradeSignal {
                            action: SignalAction::Cancel,
                            symbol: symbol.clone(),
                            market_id: ob.market_id,
                            side: Side::Buy,
                            price: quote.price,
                            quantity: quote.quantity,
                            order_type: OrderType::Limit,
                            reason: format!("Cancel maker buy: {reason}"),
                            timestamp: ob.timestamp,
                            expected_edge_bps: None,
                            risk_reducing: true,
                            post_only: false,
                            client_id: Some(quote.client_id.clone()),
                        });
                        quote.last_action = now;
                    }
                }
            }

            // 卖价侧
            if let Some(scale) = sell_scale.filter(|_| !block_sell_trend && !sell_would_take && !block_sell_obi) {
                if let Some((price, qty)) =
                    self.desired_quote(sell_price, sell_qty, Some(scale), now, state.sell.as_ref())
                {
                    let client_id = Self::client_id(symbol, Side::Sell);
                    let risk_reducing = position > 0.0 && qty <= position + f64::EPSILON;
                    let edge_bps = (price - mid) / mid * 10_000.0;
                    all_signals.push(TradeSignal {
                        action: SignalAction::Place,
                        symbol: symbol.clone(),
                        market_id: ob.market_id,
                        side: Side::Sell,
                        price,
                        quantity: qty,
                        order_type: OrderType::Limit,
                        reason: format!(
                            "Maker Sell @ {:.2} (spread {:.1}bps, skew {:+.1}bps, vol {:.1}bps)",
                            price, spread_bps, skew_bps, vol_bps
                        ),
                        timestamp: ob.timestamp,
                        expected_edge_bps: Some(edge_bps),
                        risk_reducing,
                        post_only: true,
                        client_id: Some(client_id.clone()),
                    });
                    state.sell = Some(Quote {
                        client_id,
                        price,
                        quantity: qty,
                        last_action: now,
                        confirmed: false,
                        gone: false,
                    });
                }
            } else {
                let reason = if sell_would_take {
                    "ALO would take"
                } else if block_sell_obi {
                    "BBO notional imbalance"
                } else if block_sell_trend {
                    "strong uptrend"
                } else {
                    "short inventory hard cap"
                };
                if let Some(quote) = state.sell.as_mut() {
                    if now.signed_duration_since(quote.last_action)
                        >= Duration::seconds(self.requote_cooldown_secs)
                    {
                        all_signals.push(TradeSignal {
                            action: SignalAction::Cancel,
                            symbol: symbol.clone(),
                            market_id: ob.market_id,
                            side: Side::Sell,
                            price: quote.price,
                            quantity: quote.quantity,
                            order_type: OrderType::Limit,
                            reason: format!("Cancel maker sell: {reason}"),
                            timestamp: ob.timestamp,
                            expected_edge_bps: None,
                            risk_reducing: true,
                            post_only: false,
                            client_id: Some(quote.client_id.clone()),
                        });
                        quote.last_action = now;
                    }
                }
            }

            debug!(
                "{} mid={:.2} ema={:.2} trend={:+.3}% pos={:+.4} buy={:?} sell={:?}",
                symbol,
                mid,
                state.ema,
                trend_pct * 100.0,
                position,
                state.buy.as_ref().map(|q| (q.price, q.quantity)),
                state.sell.as_ref().map(|q| (q.price, q.quantity)),
            );
        }

        if all_signals.is_empty() {
            Ok(None)
        } else {
            Ok(Some(all_signals))
        }
    }

    fn reset(&mut self) {
        let mut states = self.states.lock();
        states.clear();
    }

    fn clear_filled_state(&self) {
        let mut states = self.states.lock();
        for (symbol, state) in states.iter_mut() {
            state.buy = None;
            state.sell = None;
            info!("Maker quote state cleared for {}", symbol);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn book(symbol: &str, ts: i64, mid: f64) -> MarketSnapshot {
        let mut snap = MarketSnapshot::default();
        snap.order_books.insert(
            symbol.to_string(),
            OrderBook {
                symbol: symbol.to_string(),
                market_id: 1,
                bids: vec![PriceLevel {
                    price: mid - 0.5,
                    quantity: 1.0,
                }],
                asks: vec![PriceLevel {
                    price: mid + 0.5,
                    quantity: 1.0,
                }],
                timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
            },
        );
        snap
    }

    fn with_live_orders(mut snapshot: MarketSnapshot) -> MarketSnapshot {
        snapshot.open_orders_authoritative = true;
        for side in [Side::Buy, Side::Sell] {
            snapshot.open_orders.push(OpenOrderRef {
                symbol: "BTC-USD".to_string(),
                client_id: Some(format!("mq_BTC-USD_{}", side_label(side))),
                side,
                price: 100.0,
                quantity: 1.0,
                status: "OPEN".into(),
            });
        }
        snapshot
    }

    fn strategy() -> MakerQuoteStrategy {
        MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid strategy")
    }

    #[tokio::test]
    async fn emits_both_sides_post_only_on_first_eval() {
        let s = strategy();
        let snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        let signals = s.evaluate(&snap).await.expect("eval").expect("signals");
        assert_eq!(signals.len(), 2);
        let buy = signals.iter().find(|x| x.side == Side::Buy).expect("buy");
        let sell = signals.iter().find(|x| x.side == Side::Sell).expect("sell");
        assert!(buy.post_only && sell.post_only);
        assert!(buy.client_id.is_some() && sell.client_id.is_some());
        assert!(buy.price < 60_000.0);
        assert!(sell.price > 60_000.0);
        assert!((buy.price - sell.price).abs() > 1.0);
        assert!((buy.price * buy.quantity - 200.0).abs() < 1.0);
        assert!((sell.price * sell.quantity - 200.0).abs() < 1.0);
    }

    #[tokio::test]
    async fn multi_market_signal_order_is_independent_of_hashmap_insertion_order() {
        let mut forward = MarketSnapshot::default();
        for (symbol, mid) in [("AAA-USD", 100.0), ("ZZZ-USD", 200.0)] {
            forward
                .order_books
                .extend(book(symbol, 1_700_000_000, mid).order_books);
        }
        let mut reverse = MarketSnapshot::default();
        for (symbol, mid) in [("ZZZ-USD", 200.0), ("AAA-USD", 100.0)] {
            reverse
                .order_books
                .extend(book(symbol, 1_700_000_000, mid).order_books);
        }

        let forward_signals = strategy()
            .evaluate(&forward)
            .await
            .expect("forward eval")
            .expect("forward signals");
        let reverse_signals = strategy()
            .evaluate(&reverse)
            .await
            .expect("reverse eval")
            .expect("reverse signals");
        let signature = |signals: Vec<TradeSignal>| {
            signals
                .into_iter()
                .map(|signal| {
                    (
                        signal.symbol,
                        signal.side,
                        signal.price.to_bits(),
                        signal.quantity.to_bits(),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(signature(forward_signals), signature(reverse_signals));
    }

    #[tokio::test]
    async fn same_price_no_reemit_within_threshold() {
        let s = strategy();
        let snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        let _ = s.evaluate(&snap).await.expect("eval1");
        let snap2 = book("BTC-USD", 1_700_000_060, 60_010.0);
        let r = s.evaluate(&snap2).await.expect("eval2");
        assert!(r.is_none(), "阈值内不应重报价: {:?}", r);
    }

    #[tokio::test]
    async fn reemits_after_fill_detected_via_position_delta() {
        let s = strategy();
        let snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        let _ = s.evaluate(&snap).await.expect("eval1");
        let mut snap2 = book("BTC-USD", 1_700_000_060, 60_000.0);
        snap2.positions.insert("BTC-USD".to_string(), 0.0033);
        let r = s.evaluate(&snap2).await.expect("eval2").expect("signals");
        assert_eq!(r.len(), 2);
    }

    #[tokio::test]
    async fn open_orders_view_reconciles_filled_quote() {
        let s = strategy();
        let snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        let _ = s.evaluate(&snap).await.expect("eval1");
        let mut snap2 = book("BTC-USD", 1_700_000_060, 60_000.0);
        snap2.open_orders_authoritative = true;
        let sell_id = "mq_BTC-USD_sell".to_string();
        snap2.open_orders.push(OpenOrderRef {
            symbol: "BTC-USD".to_string(),
            client_id: Some(sell_id),
            side: Side::Sell,
            price: 60_018.0,
            quantity: 200.0 / 60_018.0,
            status: "OPEN".into(),
        });
        let r = s.evaluate(&snap2).await.expect("eval2").expect("signals");
        assert_eq!(
            r.len(),
            1,
            "应只重报价买单: {:?}",
            r.iter().map(|x| x.side).collect::<Vec<_>>()
        );
        assert_eq!(r[0].side, Side::Buy);
    }

    #[tokio::test]
    async fn hard_cap_blocks_accumulating_side_only() {
        let s = strategy();
        let mut snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        snap.positions.insert("BTC-USD".to_string(), 0.04);
        let r = s.evaluate(&snap).await.expect("eval").expect("signals");
        assert!(
            r.iter().all(|x| x.side == Side::Sell),
            "应只剩卖单: {:?}",
            r.iter().map(|x| x.side).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn soft_cap_scales_down_accumulating_side() {
        let s = strategy();
        let mut snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        snap.positions
            .insert("BTC-USD".to_string(), 800.0 / 60_000.0);
        let r = s.evaluate(&snap).await.expect("eval").expect("signals");
        let buy = r.iter().find(|x| x.side == Side::Buy).expect("buy");
        assert!(
            buy.quantity * buy.price < 150.0,
            "买单应缩量: {:.2}",
            buy.quantity * buy.price
        );
    }

    #[tokio::test]
    async fn trend_filter_blocks_counter_trend_side() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, true, 5, 6.0, 5.0)
            .expect("valid");
        for i in 0..12 {
            let price = 61_000.0 - (i as f64) * 100.0;
            let snap = book("BTC-USD", 1_700_000_000 + i, price);
            let _ = s.evaluate(&snap).await.expect("eval");
        }
        let mut snap = book("BTC-USD", 1_700_000_000 + 20, 59_000.0);
        snap.positions.insert("BTC-USD".to_string(), 0.0);
        let r = s.evaluate(&snap).await.expect("eval").expect("signals");
        assert!(
            r.iter()
                .all(|x| x.side == Side::Sell || x.action == SignalAction::Cancel),
            "强下跌趋势不应产生新买单: {:?}",
            r.iter().map(|x| (x.side, x.action)).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn risk_reducing_flag_on_inventory_reducing_quote() {
        let s = strategy();
        let mut snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        snap.positions
            .insert("BTC-USD".to_string(), 500.0 / 60_000.0);
        let r = s.evaluate(&snap).await.expect("eval").expect("signals");
        let sell = r.iter().find(|x| x.side == Side::Sell).expect("sell");
        assert!(sell.risk_reducing, "减仓侧应标记 risk_reducing");
        let buy = r.iter().find(|x| x.side == Side::Buy).expect("buy");
        assert!(!buy.risk_reducing, "加仓侧不应标记 risk_reducing");
    }

    #[test]
    fn rejects_invalid_config() {
        assert!(
            MakerQuoteStrategy::new(0.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
                .is_err()
        );
        assert!(
            MakerQuoteStrategy::new(6.0, 0.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0).is_err()
        );
        assert!(
            MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 1000.0, 600.0, false, 20, 6.0, 5.0)
                .is_err()
        );
        assert!(
            MakerQuoteStrategy::new(6.0, 200.0, 2.0, -1, 600.0, 1000.0, false, 20, 6.0, 5.0)
                .is_err()
        );
        assert!(MakerQuoteStrategy::new(
            f64::NAN,
            200.0,
            2.0,
            5,
            600.0,
            1000.0,
            false,
            20,
            6.0,
            5.0,
        )
        .is_err());
        assert!(MakerQuoteStrategy::new(
            6.0,
            200.0,
            2.0,
            5,
            600.0,
            1000.0,
            false,
            20,
            6.0,
            f64::NAN,
        )
        .is_err());
    }

    #[test]
    fn rejects_invalid_builder_options() {
        let base = || {
            MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
                .expect("valid")
        };
        assert!(
            base().with_adaptive_spread(201, 1.0).is_err(),
            "vol_window > 200"
        );
        assert!(
            base().with_adaptive_spread(10, -1.0).is_err(),
            "vol_multiplier < 0"
        );
        assert!(base().with_inventory_skew(-1.0).is_err(), "skew < 0");
        assert!(base().with_quote_budget(-1.0).is_err(), "budget < 0");
        assert!(
            base().with_min_book_spread(-1.0).is_err(),
            "min_book_spread < 0"
        );
        assert!(
            base()
                .with_market_circuit_breaker(20.0, 10.0, 60)
                .unwrap()
                .with_min_book_spread(10.0)
                .is_err(),
            "min_book_spread >= max_book_spread"
        );
        assert!(
            base().with_wide_book_size_mult(0.5).is_err(),
            "wide_book_size_mult < 1"
        );
        assert!(
            base().with_max_bbo_imbalance(-1.0).is_err(),
            "max_bbo_imbalance < 0"
        );
        assert!(
            base().with_flatten_cycle(true, 2, 20, 10).is_err(),
            "ioc before mid"
        );
        assert!(QuoteMode::parse("nope").is_err());
        assert_eq!(QuoteMode::parse("join_best").unwrap(), QuoteMode::JoinBest);
        assert_eq!(QuoteMode::parse("mid").unwrap(), QuoteMode::MidSpread);
    }

    #[tokio::test]
    async fn adaptive_spread_widens_with_volatility() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_adaptive_spread(20, 1.0)
            .expect("valid");
        let mut last: Option<f64> = None;
        for i in 0..25 {
            let wobble = ((i % 3) as f64 - 1.0) * 150.0;
            let price = 60_000.0 + wobble + (i as f64) * 2.0;
            let snap = book("BTC-USD", 1_700_000_000 + i, price);
            let r = s.evaluate(&snap).await.expect("eval");
            if let Some(signals) = r {
                last = Some(
                    signals
                        .iter()
                        .map(|x| (x.price - price).abs() / price * 10_000.0)
                        .fold(0.0_f64, f64::max),
                );
            }
        }
        let offset = last.expect("signals produced");
        assert!(
            offset > 3.0 + 8.0,
            "高波动下单向偏移应显著大于基础 half=3bps，实测 {offset:.1}bps"
        );
    }

    #[test]
    fn low_volatility_keeps_base_spread() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_adaptive_spread(20, 100.0)
            .expect("valid");
        let history: Vec<f64> = (0..40).map(|i| 60_000.0 + (i as f64) * 0.01).collect();
        let vol = s.realized_vol_bps(&history);
        assert!(vol < 0.01, "平稳行情波动应≈0，实测 {vol:.4}bps");
        let spread = s.effective_spread_bps(vol);
        assert!(
            (spread - 6.0).abs() < 0.1,
            "价差应≈基础 6bps，实测 {spread:.2}bps"
        );
    }

    #[tokio::test]
    async fn inventory_skew_pulls_reducing_side_toward_mid() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_inventory_skew(3.0)
            .expect("valid");
        let mut snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        snap.positions
            .insert("BTC-USD".to_string(), 300.0 / 60_000.0);
        let r = s.evaluate(&snap).await.expect("eval").expect("signals");
        let buy = r.iter().find(|x| x.side == Side::Buy).expect("buy");
        let sell = r.iter().find(|x| x.side == Side::Sell).expect("sell");
        let buy_off = (60_000.0 - buy.price) / 60_000.0 * 10_000.0;
        let sell_off = (sell.price - 60_000.0) / 60_000.0 * 10_000.0;
        assert!(buy_off > 3.0, "累库侧(买)应推远：{buy_off:.2}bps");
        assert!(sell_off < 3.0, "减库侧(卖)应拉近：{sell_off:.2}bps");
        assert!(sell_off > 0.0, "减库侧仍应在 mid 之上：{sell_off:.2}bps");
        assert!((buy.price * buy.quantity - 200.0).abs() < 1.0);
    }

    #[tokio::test]
    async fn inventory_skew_short_side_mirrors_long() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_inventory_skew(3.0)
            .expect("valid");
        let mut snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        snap.positions
            .insert("BTC-USD".to_string(), -300.0 / 60_000.0);
        let r = s.evaluate(&snap).await.expect("eval").expect("signals");
        let buy = r.iter().find(|x| x.side == Side::Buy).expect("buy");
        let sell = r.iter().find(|x| x.side == Side::Sell).expect("sell");
        let buy_off = (60_000.0 - buy.price) / 60_000.0 * 10_000.0;
        let sell_off = (sell.price - 60_000.0) / 60_000.0 * 10_000.0;
        assert!(buy_off < 3.0, "减库侧(买)应拉近：{buy_off:.2}bps");
        assert!(sell_off > 3.0, "累库侧(卖)应推远：{sell_off:.2}bps");
    }

    #[tokio::test]
    async fn skew_never_crosses_mid_even_at_max() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_inventory_skew(100.0)
            .expect("valid");
        let mut snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        snap.positions
            .insert("BTC-USD".to_string(), 600.0 / 60_000.0);
        let r = s.evaluate(&snap).await.expect("eval").expect("signals");
        let sell = r.iter().find(|x| x.side == Side::Sell).expect("sell");
        assert!(sell.price > 60_000.0, "卖价不得越过 mid：{:.2}", sell.price);
        let buy = r.iter().find(|x| x.side == Side::Buy).expect("buy");
        assert!(buy.price < 60_000.0, "买价不得越过 mid：{:.2}", buy.price);
    }

    #[tokio::test]
    async fn quote_budget_scales_per_market_notional() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_quote_budget(100.0)
            .expect("valid");
        let mut snap = MarketSnapshot::default();
        for (sym, mid) in [("BTC-USD", 60_000.0), ("ETH-USD", 1_900.0)] {
            snap.order_books.insert(
                sym.to_string(),
                OrderBook {
                    symbol: sym.to_string(),
                    market_id: 1,
                    bids: vec![PriceLevel {
                        price: mid - 0.5,
                        quantity: 1.0,
                    }],
                    asks: vec![PriceLevel {
                        price: mid + 0.5,
                        quantity: 1.0,
                    }],
                    timestamp: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                },
            );
        }
        let r = s.evaluate(&snap).await.expect("eval").expect("signals");
        assert_eq!(r.len(), 4, "双市场各出两侧");
        for sig in &r {
            let notional = sig.price * sig.quantity;
            assert!(
                (notional - 50.0).abs() < 0.5,
                "预算分摊后单边名义应为 50，实测 {notional:.2}"
            );
        }
    }

    #[tokio::test]
    async fn quote_budget_never_exceeds_per_market_cap() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_quote_budget(1000.0)
            .expect("valid");
        let snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        let r = s.evaluate(&snap).await.expect("eval").expect("signals");
        for sig in &r {
            let notional = sig.price * sig.quantity;
            assert!(
                (notional - 200.0).abs() < 1.0,
                "仍应保持 per_quote_notional=200，实测 {notional:.2}"
            );
        }
    }

    #[tokio::test]
    async fn skew_keeps_full_quantity_not_linear_scaling() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_inventory_skew(3.0)
            .expect("valid");
        let mut snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        snap.positions
            .insert("BTC-USD".to_string(), 800.0 / 60_000.0);
        let r = s.evaluate(&snap).await.expect("eval").expect("signals");
        let buy = r.iter().find(|x| x.side == Side::Buy).expect("buy");
        assert!(
            (buy.price * buy.quantity - 200.0).abs() < 1.0,
            "偏斜模式数量保持满额，实测 {:.2}",
            buy.price * buy.quantity
        );
    }

    #[tokio::test]
    async fn inventory_skew_still_enforces_hard_cap_for_longs() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_inventory_skew(3.0)
            .expect("valid");
        let mut snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        snap.positions
            .insert("BTC-USD".to_string(), 1_200.0 / 60_000.0);
        let signals = s.evaluate(&snap).await.expect("eval").expect("signals");
        assert!(
            signals.iter().all(|signal| signal.side == Side::Sell),
            "long above hard cap must not emit a buy: {signals:?}"
        );
    }

    #[tokio::test]
    async fn inventory_skew_still_enforces_hard_cap_for_shorts() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 2.0, 5, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_inventory_skew(3.0)
            .expect("valid");
        let mut snap = book("BTC-USD", 1_700_000_000, 60_000.0);
        snap.positions
            .insert("BTC-USD".to_string(), -1_200.0 / 60_000.0);
        let signals = s.evaluate(&snap).await.expect("eval").expect("signals");
        assert!(
            signals.iter().all(|signal| signal.side == Side::Buy),
            "short below hard cap must not emit a sell: {signals:?}"
        );
    }

    #[tokio::test]
    async fn both_sides_requote_after_their_own_cooldown() {
        let s = strategy();
        let first = book("BTC-USD", 1_700_000_000, 60_000.0);
        let _ = s.evaluate(&first).await.expect("first eval");
        let moved = book("BTC-USD", 1_700_000_006, 60_100.0);
        let signals = s
            .evaluate(&moved)
            .await
            .expect("second eval")
            .expect("requotes");
        assert_eq!(
            signals
                .iter()
                .filter(|signal| signal.side == Side::Buy)
                .count(),
            1
        );
        assert_eq!(
            signals
                .iter()
                .filter(|signal| signal.side == Side::Sell)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn hard_cap_cancels_an_existing_accumulating_quote() {
        let s = strategy();
        let first = book("BTC-USD", 1_700_000_000, 60_000.0);
        let _ = s.evaluate(&first).await.expect("first eval");
        let mut capped = book("BTC-USD", 1_700_000_010, 60_000.0);
        capped
            .positions
            .insert("BTC-USD".to_string(), 1_200.0 / 60_000.0);
        capped.open_orders_authoritative = true;
        for side in [Side::Buy, Side::Sell] {
            capped.open_orders.push(OpenOrderRef {
                symbol: "BTC-USD".to_string(),
                client_id: Some(format!("mq_BTC-USD_{}", side_label(side))),
                side,
                price: 60_000.0,
                quantity: 200.0 / 60_000.0,
                status: "OPEN".into(),
            });
        }
        let signals = s
            .evaluate(&capped)
            .await
            .expect("capped eval")
            .expect("cancel");
        let cancel = signals
            .iter()
            .find(|signal| signal.side == Side::Buy)
            .expect("buy cancel");
        assert_eq!(cancel.action, SignalAction::Cancel);
        assert_eq!(cancel.client_id.as_deref(), Some("mq_BTC-USD_buy"));
    }

    #[tokio::test]
    async fn authoritative_empty_view_waits_for_unconfirmed_quote_cooldown() {
        let s = strategy();
        let first = book("BTC-USD", 1_700_000_000, 60_000.0);
        let _ = s.evaluate(&first).await.expect("first eval");
        let mut empty_view = book("BTC-USD", 1_700_000_001, 60_000.0);
        empty_view.open_orders_authoritative = true;
        let signals = s.evaluate(&empty_view).await.expect("reconcile");
        assert!(
            signals.is_none(),
            "an unconfirmed placement must not be re-emitted before cooldown"
        );
    }

    #[tokio::test]
    async fn authoritative_empty_open_orders_requotes_both_sides() {
        let s = strategy();
        let first = book("BTC-USD", 1_700_000_000, 60_000.0);
        let _ = s.evaluate(&first).await.expect("first eval");
        let mut empty_view = book("BTC-USD", 1_700_000_010, 60_000.0);
        empty_view.open_orders_authoritative = true;
        let signals = s
            .evaluate(&empty_view)
            .await
            .expect("reconcile")
            .expect("requotes");
        assert_eq!(signals.len(), 2);
        assert!(signals
            .iter()
            .all(|signal| signal.action == SignalAction::Place));
    }

    #[tokio::test]
    async fn confirmed_quote_gone_waits_for_requote_cooldown() {
        let s = strategy();
        let first = book("BTC-USD", 1_700_000_000, 60_000.0);
        let _ = s.evaluate(&first).await.expect("first eval");
        let live = with_live_orders(book("BTC-USD", 1_700_000_001, 60_000.0));
        let _ = s.evaluate(&live).await.expect("confirm");
        let mut gone = book("BTC-USD", 1_700_000_002, 60_000.0);
        gone.open_orders_authoritative = true;
        let cooling = s.evaluate(&gone).await.expect("tombstone");
        assert!(
            cooling.is_none(),
            "a confirmed fill must not re-place before requote cooldown: {cooling:?}"
        );
        let mut later = book("BTC-USD", 1_700_000_010, 60_000.0);
        later.open_orders_authoritative = true;
        let signals = s
            .evaluate(&later)
            .await
            .expect("after cooldown")
            .expect("requotes");
        assert_eq!(signals.len(), 2);
        assert!(signals
            .iter()
            .all(|signal| signal.action == SignalAction::Place));
    }

    #[tokio::test]
    async fn repeated_snapshot_timestamp_does_not_advance_features() {
        let s = strategy();
        let first = book("BTC-USD", 1_700_000_000, 60_000.0);
        let _ = s.evaluate(&first).await.expect("first eval");
        let duplicate = book("BTC-USD", 1_700_000_000, 60_100.0);
        let _ = s.evaluate(&duplicate).await.expect("duplicate eval");
        let states = s.states.lock();
        let state = states.get("BTC-USD").expect("state");
        assert_eq!(state.price_history.len(), 1);
        assert_eq!(state.ema, 60_000.0);
    }

    #[tokio::test]
    async fn new_york_cash_open_guard_is_dst_aware() {
        let summer = Utc
            .with_ymd_and_hms(2026, 8, 11, 13, 30, 0)
            .single()
            .unwrap()
            .timestamp();
        let winter = Utc
            .with_ymd_and_hms(2026, 1, 12, 14, 30, 0)
            .single()
            .unwrap()
            .timestamp();
        for timestamp in [summer, winter] {
            let strategy = strategy()
                .with_cash_open_guard(true, 5, 20)
                .expect("valid guard");
            assert!(
                strategy
                    .evaluate(&book("BTC-USD", timestamp, 100.0))
                    .await
                    .expect("evaluate")
                    .is_none(),
                "09:30 America/New_York must not create maker exposure"
            );
        }
    }

    #[tokio::test]
    async fn cash_open_guard_cancels_live_quotes_without_waiting_for_requote_cooldown() {
        let strategy = strategy()
            .with_cash_open_guard(true, 5, 20)
            .expect("valid guard");
        let before_open = Utc
            .with_ymd_and_hms(2026, 8, 11, 13, 24, 58)
            .single()
            .unwrap()
            .timestamp();
        let _ = strategy
            .evaluate(&book("BTC-USD", before_open, 100.0))
            .await
            .expect("initial quotes");
        let guarded = with_live_orders(book("BTC-USD", before_open + 2, 100.0));
        let signals = strategy
            .evaluate(&guarded)
            .await
            .expect("guard")
            .expect("cancel signals");
        assert_eq!(signals.len(), 2);
        assert!(signals
            .iter()
            .all(|signal| signal.action == SignalAction::Cancel));
    }

    #[tokio::test]
    async fn instantaneous_jump_trips_circuit_breaker_and_cancels_both_sides() {
        let strategy = strategy()
            .with_market_circuit_breaker(20.0, 200.0, 60)
            .expect("valid breaker");
        let timestamp = Utc
            .with_ymd_and_hms(2026, 8, 11, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        let _ = strategy
            .evaluate(&book("BTC-USD", timestamp, 100.0))
            .await
            .expect("initial quotes");
        let jumped = with_live_orders(book("BTC-USD", timestamp + 1, 100.30));
        let signals = strategy
            .evaluate(&jumped)
            .await
            .expect("breaker")
            .expect("cancel signals");
        assert_eq!(signals.len(), 2);
        assert!(signals
            .iter()
            .all(|signal| signal.action == SignalAction::Cancel));
    }

    #[tokio::test]
    async fn wide_book_trips_circuit_breaker_before_placing_quotes() {
        let strategy = strategy()
            .with_market_circuit_breaker(100.0, 20.0, 60)
            .expect("valid breaker");
        let mut snapshot = book("BTC-USD", 1_700_000_000, 100.0);
        let order_book = snapshot.order_books.get_mut("BTC-USD").unwrap();
        order_book.bids[0].price = 99.5;
        order_book.asks[0].price = 100.5;
        assert!(strategy
            .evaluate(&snapshot)
            .await
            .expect("evaluate")
            .is_none());
    }

    fn join_book(symbol: &str, ts: i64, bid: f64, ask: f64) -> MarketSnapshot {
        let mut snap = MarketSnapshot::default();
        snap.order_books.insert(
            symbol.to_string(),
            OrderBook {
                symbol: symbol.to_string(),
                market_id: 2,
                bids: vec![PriceLevel {
                    price: bid,
                    quantity: 1.0,
                }],
                asks: vec![PriceLevel {
                    price: ask,
                    quantity: 1.0,
                }],
                timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
            },
        );
        snap
    }

    #[tokio::test]
    async fn join_best_quotes_exact_best_bid_and_ask() {
        let s = strategy()
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode");
        let snap = join_book("io:SNDK", 1_700_000_000, 1069.1, 1069.2);
        let signals = s.evaluate(&snap).await.expect("eval").expect("signals");
        let buy = signals.iter().find(|x| x.side == Side::Buy).expect("buy");
        let sell = signals.iter().find(|x| x.side == Side::Sell).expect("sell");
        assert!((buy.price - 1069.1).abs() < 1e-12);
        assert!((sell.price - 1069.2).abs() < 1e-12);
        assert!(buy.post_only && sell.post_only);
        assert!(buy.price < sell.price);
    }

    #[test]
    fn alo_would_take_detects_crossing_post_only_prices() {
        assert!(!MakerQuoteStrategy::alo_would_take(
            10.0,
            Side::Buy,
            10.0,
            10.5
        ));
        assert!(MakerQuoteStrategy::alo_would_take(
            10.5,
            Side::Buy,
            10.0,
            10.5
        ));
        assert!(MakerQuoteStrategy::alo_would_take(
            10.6,
            Side::Buy,
            10.0,
            10.5
        ));
        assert!(!MakerQuoteStrategy::alo_would_take(
            10.5,
            Side::Sell,
            10.0,
            10.5
        ));
        assert!(MakerQuoteStrategy::alo_would_take(
            10.0,
            Side::Sell,
            10.0,
            10.5
        ));
        assert!(MakerQuoteStrategy::alo_would_take(
            9.9,
            Side::Sell,
            10.0,
            10.5
        ));
    }

    #[tokio::test]
    async fn join_best_does_not_improve_inside_the_spread() {
        let s = strategy()
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode");
        let snap = join_book("io:ANTH", 1_700_000_000, 10.0, 10.5);
        let signals = s.evaluate(&snap).await.expect("eval").expect("signals");
        let buy = signals.iter().find(|x| x.side == Side::Buy).expect("buy");
        let sell = signals.iter().find(|x| x.side == Side::Sell).expect("sell");
        assert!((buy.price - 10.0).abs() < 1e-12);
        assert!((sell.price - 10.5).abs() < 1e-12);
        assert!(buy.price < 10.25 && sell.price > 10.25);
    }

    #[tokio::test]
    async fn join_best_requotes_when_inside_tick_moves() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 0.0, 0, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode");
        let first = join_book("io:SNDK", 1_700_000_000, 1069.1, 1069.2);
        let _ = s.evaluate(&first).await.expect("first");
        let moved = join_book("io:SNDK", 1_700_000_001, 1069.3, 1069.4);
        let signals = s.evaluate(&moved).await.expect("second").expect("requote");
        let buy = signals.iter().find(|x| x.side == Side::Buy).expect("buy");
        let sell = signals.iter().find(|x| x.side == Side::Sell).expect("sell");
        assert!((buy.price - 1069.3).abs() < 1e-12);
        assert!((sell.price - 1069.4).abs() < 1e-12);
    }

    #[tokio::test]
    async fn min_book_spread_cancels_join_best_on_a_tight_book() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 0.0, 0, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode")
            .with_min_book_spread(8.0)
            .expect("min spread");
        let wide = join_book("io:SNDK", 1_700_000_000, 100.0, 100.20); // 20 bps
        let first = s.evaluate(&wide).await.expect("wide").expect("quotes");
        assert!(first
            .iter()
            .any(|signal| signal.action == SignalAction::Place));
        assert!(first.iter().any(|signal| signal.side == Side::Buy));

        let tight = join_book("io:SNDK", 1_700_000_001, 100.0, 100.02); // 2 bps
        let second = s.evaluate(&tight).await.expect("tight").expect("cancels");
        assert!(second
            .iter()
            .all(|signal| signal.action == SignalAction::Cancel));
        assert_eq!(second.len(), 2);
    }

    #[tokio::test]
    async fn wide_book_scales_quote_notional() {
        let s = MakerQuoteStrategy::new(6.0, 100.0, 0.0, 0, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode")
            .with_min_book_spread(10.0)
            .expect("min spread")
            .with_wide_book_size_mult(2.0)
            .expect("size mult");
        let at_min = join_book("io:SNDK", 1_700_000_000, 100.0, 100.11); // ~11 bps
        let base = s.evaluate(&at_min).await.expect("min").expect("quotes");
        let base_buy = base
            .iter()
            .find(|signal| signal.side == Side::Buy)
            .expect("buy");
        let wide = join_book("io:ANTH", 1_700_000_000, 100.0, 100.21); // ~21 bps
        let scaled = s.evaluate(&wide).await.expect("wide").expect("quotes");
        let scaled_buy = scaled
            .iter()
            .find(|signal| signal.side == Side::Buy)
            .expect("buy");
        let base_notional = base_buy.price * base_buy.quantity;
        let scaled_notional = scaled_buy.price * scaled_buy.quantity;
        assert!(
            (base_notional - 100.0).abs() < 15.0,
            "near-min book should stay close to base size, got {base_notional}"
        );
        assert!(
            scaled_notional > base_notional * 1.5,
            "wide book should scale size up ({scaled_notional} vs {base_notional})"
        );
        assert!((scaled_notional - 200.0).abs() < 5.0, "{scaled_notional}");
    }

    #[tokio::test]
    async fn bbo_imbalance_cancels_one_sided_join_best() {
        let s = MakerQuoteStrategy::new(6.0, 200.0, 0.0, 0, 600.0, 1000.0, false, 20, 6.0, 5.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode")
            .with_max_bbo_imbalance(6.0)
            .expect("imbalance");
        let mut balanced = join_book("io:ANTH", 1_700_000_000, 2013.5, 2013.8);
        balanced.order_books.get_mut("io:ANTH").unwrap().bids[0].quantity = 0.5;
        balanced.order_books.get_mut("io:ANTH").unwrap().asks[0].quantity = 0.5;
        let first = s
            .evaluate(&balanced)
            .await
            .expect("balanced")
            .expect("quotes");
        assert!(first
            .iter()
            .any(|signal| signal.action == SignalAction::Place));

        let mut lopsided = join_book("io:ANTH", 1_700_000_001, 2013.5, 2013.8);
        lopsided.order_books.get_mut("io:ANTH").unwrap().bids[0].quantity = 0.6;
        lopsided.order_books.get_mut("io:ANTH").unwrap().asks[0].quantity = 0.01;
        let second = s
            .evaluate(&lopsided)
            .await
            .expect("lopsided")
            .expect("cancels");
        assert!(
            second
                .iter()
                .any(|signal| signal.side == Side::Buy && signal.action == SignalAction::Cancel),
            "heavy bid must pull the buy: {second:?}"
        );
        assert!(
            second
                .iter()
                .all(|signal| !(signal.side == Side::Sell && signal.action == SignalAction::Cancel)),
            "thin ask side must keep quoting: {second:?}"
        );
    }

    #[tokio::test]
    async fn live_two_bps_book_still_quotes_with_low_min_spread() {
        let s = MakerQuoteStrategy::new(6.0, 40.0, 0.0, 0, 80.0, 160.0, false, 20, 12.0, 10.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode")
            .with_min_book_spread(1.5)
            .expect("min spread");
        let snap = join_book("io:SNDK", 1_700_000_000, 1471.6, 1471.9);
        let signals = s.evaluate(&snap).await.expect("eval").expect("quotes");
        assert!(signals
            .iter()
            .any(|signal| signal.side == Side::Buy && signal.action == SignalAction::Place));
        assert!(signals
            .iter()
            .any(|signal| signal.side == Side::Sell && signal.action == SignalAction::Place));
    }

    #[tokio::test]
    async fn join_inside_improves_when_spread_exceeds_two_ticks() {
        let s = MakerQuoteStrategy::new(6.0, 50.0, 0.0, 0, 80.0, 160.0, false, 20, 6.0, 10.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode")
            .with_flatten_cycle(false, 2, 6, 15)
            .expect("inside ticks");
        let snap = join_book("io:ANTH", 1_700_000_000, 1985.00, 1986.90);
        let signals = s.evaluate(&snap).await.expect("eval").expect("quotes");
        let buy = signals.iter().find(|x| x.side == Side::Buy).expect("buy");
        let sell = signals.iter().find(|x| x.side == Side::Sell).expect("sell");
        assert!((buy.price - 1985.8).abs() < 1e-9, "{}", buy.price);
        assert!((sell.price - 1986.1).abs() < 1e-9, "{}", sell.price);
        assert!(buy.post_only && sell.post_only);
        assert!(!buy.risk_reducing && !sell.risk_reducing);
    }

    #[tokio::test]
    async fn flatten_only_quotes_reduce_only_then_alo_refresh() {
        let s = MakerQuoteStrategy::new(6.0, 50.0, 0.0, 0, 80.0, 160.0, false, 20, 6.0, 10.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode")
            .with_flatten_cycle(true, 2, 6, 15)
            .expect("flatten");
        let rest = join_book("io:SNDK", 1_700_000_000, 1471.6, 1471.9);
        let _ = s.evaluate(&rest).await.expect("flat quotes");
        let mut snap = join_book("io:SNDK", 1_700_000_001, 1471.6, 1471.9);
        snap.positions.insert("io:SNDK".into(), 0.04);
        let first = s.evaluate(&snap).await.expect("t0").expect("far");
        let sell = first
            .iter()
            .find(|x| x.side == Side::Sell && x.action == SignalAction::Place)
            .expect("sell");
        assert!(sell.risk_reducing && sell.post_only);
        assert!((sell.price - 1471.9).abs() < 1e-9);
        assert!(first
            .iter()
            .all(|x| x.side != Side::Buy || x.action == SignalAction::Cancel));

        snap = join_book("io:SNDK", 1_700_000_007, 1471.6, 1471.9);
        snap.positions.insert("io:SNDK".into(), 0.04);
        let mid_stage = s.evaluate(&snap).await.expect("t7").expect("mid");
        let mid_sell = mid_stage
            .iter()
            .find(|x| x.side == Side::Sell && x.action == SignalAction::Place)
            .expect("mid sell");
        assert!(mid_sell.post_only && mid_sell.risk_reducing);
        assert!(mid_sell.price < 1471.9 && mid_sell.price > 1471.6);

        snap = join_book("io:SNDK", 1_700_000_016, 1471.6, 1471.9);
        snap.positions.insert("io:SNDK".into(), 0.04);
        let late = s.evaluate(&snap).await.expect("t16");
        if let Some(rows) = late {
            assert!(
                rows.iter().all(|x| x.post_only || x.action == SignalAction::Cancel),
                "flatten must not IOC after flatten_ioc_secs: {rows:?}"
            );
            assert!(rows.iter().all(|x| !x.reason.to_ascii_lowercase().contains("ioc")));
        }

        snap = join_book("io:SNDK", 1_700_000_022, 1471.6, 1471.9);
        snap.positions.insert("io:SNDK".into(), 0.04);
        let refresh = s.evaluate(&snap).await.expect("t22").expect("stale refresh");
        assert!(refresh.iter().any(|x| x.action == SignalAction::Cancel));
        let refresh_sell = refresh
            .iter()
            .find(|x| x.side == Side::Sell && x.action == SignalAction::Place)
            .expect("refresh sell");
        assert!(refresh_sell.post_only && refresh_sell.risk_reducing);
        assert!(refresh_sell.price > 1471.6 && refresh_sell.price <= 1471.9);
        assert!(!refresh_sell.reason.to_ascii_lowercase().contains("ioc"));
        assert!(refresh.iter().all(|x| !x.reason.to_ascii_lowercase().contains("ioc")));
    }

    #[tokio::test]
    async fn flatten_only_anth_short_stays_alo_never_ioc() {
        let s = MakerQuoteStrategy::new(6.0, 50.0, 0.0, 0, 80.0, 160.0, false, 20, 6.0, 10.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode")
            .with_flatten_cycle(true, 2, 6, 15)
            .expect("flatten");
        let rest = join_book("io:ANTH", 1_700_000_000, 1985.00, 1986.90);
        let _ = s.evaluate(&rest).await.expect("flat");
        let mut snap = join_book("io:ANTH", 1_700_000_001, 1985.00, 1986.90);
        snap.positions.insert("io:ANTH".into(), -0.04);
        let first = s.evaluate(&snap).await.expect("t0").expect("far");
        let buy = first
            .iter()
            .find(|x| x.side == Side::Buy && x.action == SignalAction::Place)
            .expect("buy");
        assert!(buy.risk_reducing && buy.post_only);
        assert!((buy.price - 1985.00).abs() < 1e-9);
        assert!(first
            .iter()
            .all(|x| x.side != Side::Sell || x.action == SignalAction::Cancel));

        snap = join_book("io:ANTH", 1_700_000_022, 1985.00, 1986.90);
        snap.positions.insert("io:ANTH".into(), -0.04);
        let refresh = s.evaluate(&snap).await.expect("stale").expect("refresh");
        let refresh_buy = refresh
            .iter()
            .find(|x| x.side == Side::Buy && x.action == SignalAction::Place)
            .expect("refresh buy");
        assert!(refresh_buy.post_only && refresh_buy.risk_reducing);
        assert!(refresh_buy.price >= 1985.00 && refresh_buy.price < 1986.90);
        assert!(refresh.iter().all(|x| x.post_only || x.action == SignalAction::Cancel));
        assert!(refresh.iter().all(|x| !x.reason.to_ascii_lowercase().contains("ioc")));
    }

    #[tokio::test]
    async fn flatten_only_quotes_two_sided_when_flat() {
        let s = MakerQuoteStrategy::new(6.0, 50.0, 0.0, 0, 80.0, 160.0, false, 20, 6.0, 10.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode")
            .with_flatten_cycle(true, 2, 6, 15)
            .expect("flatten");
        let rest = join_book("io:SNDK", 1_700_000_000, 1471.6, 1471.9);
        let signals = s.evaluate(&rest).await.expect("idle").expect("quotes");
        assert!(signals.iter().any(|row| {
            row.side == Side::Buy && row.action == SignalAction::Place && row.post_only
        }));
        assert!(signals.iter().any(|row| {
            row.side == Side::Sell && row.action == SignalAction::Place && row.post_only
        }));
    }

    #[tokio::test]
    async fn two_sided_mm_keeps_both_post_only_quotes_after_fill() {
        let s = MakerQuoteStrategy::new(6.0, 50.0, 0.0, 0, 80.0, 160.0, false, 20, 6.0, 10.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode")
            .with_flatten_cycle(false, 2, 6, 15)
            .expect("mm");
        let rest = join_book("io:SNDK", 1_700_000_000, 1471.6, 1471.9);
        let _ = s.evaluate(&rest).await.expect("flat");
        let mut snap = join_book("io:SNDK", 1_700_000_001, 1471.6, 1471.9);
        snap.positions.insert("io:SNDK".into(), 0.04);
        let quotes = s.evaluate(&snap).await.expect("inv").expect("two-sided");
        assert!(quotes.iter().any(|signal| {
            signal.side == Side::Buy && signal.action == SignalAction::Place && signal.post_only
        }));
        assert!(quotes.iter().any(|signal| {
            signal.side == Side::Sell && signal.action == SignalAction::Place && signal.post_only
        }));
        assert!(quotes
            .iter()
            .all(|signal| signal.post_only || signal.action == SignalAction::Cancel));
    }

    #[tokio::test]
    async fn two_sided_mm_keeps_adding_side_on_bbo_when_long() {
        let s = MakerQuoteStrategy::new(6.0, 50.0, 0.0, 0, 80.0, 160.0, false, 20, 6.0, 10.0)
            .expect("valid")
            .with_quote_mode(QuoteMode::JoinBest)
            .expect("mode")
            .with_flatten_cycle(false, 2, 6, 15)
            .expect("mm");
        let mut snap = join_book("io:ANTH", 1_700_000_000, 1985.00, 1986.90);
        snap.positions.insert("io:ANTH".into(), 0.04);
        let signals = s.evaluate(&snap).await.expect("eval").expect("quotes");
        let buy = signals.iter().find(|x| x.side == Side::Buy).expect("buy");
        let sell = signals.iter().find(|x| x.side == Side::Sell).expect("sell");
        assert!(
            (buy.price - 1985.00).abs() < 1e-9,
            "adding side stays on bid {}",
            buy.price
        );
        assert!(
            (sell.price - 1986.1).abs() < 1e-9,
            "reducing side still 1 tick inside {}",
            sell.price
        );
        assert!(buy.post_only && sell.post_only);
        assert!(!buy.risk_reducing);
        assert!(sell.risk_reducing);
    }
}
