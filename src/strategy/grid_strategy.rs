use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::{debug, info};

use super::Strategy;
use crate::lighter::types::*;

/// 缩量后低于此名义金额（USD）的信号直接丢弃
const DUST_NOTIONAL: f64 = 1.0;

/// Grid Trading Strategy for Live Trading
///
/// Places limit buy orders below current price and limit sell orders above it.
/// When a grid level is filled, the opposite direction is unlocked for profit.
/// Each market maintains its own independent grid state.
///
/// Features:
/// - Multi-tier EMA trend filter (blocks all buys in very strong downtrend)
/// - Max accumulated position limit (caps filled levels per side)
/// - Trailing anchor that gradually drifts toward EMA
/// - Faster anchor reset at 1.5x grid range
pub struct GridStrategy {
    grid_count: usize,
    investment_per_grid: f64,
    price_deviation: f64,
    /// Max filled grid levels per side before blocking new signals
    max_filled_per_side: usize,
    inventory: InventoryPolicy,
    states: Mutex<HashMap<String, MarketGridState>>,
}

/// 库存（持仓）政策。默认 `Hard` = 现行实盘行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryMode {
    /// 到达硬上限即封锁同方向开仓（实盘默认）
    Hard,
    /// Policy C：软上限以下满仓位下单，软→硬之间线性缩量，到硬上限封锁
    Soft,
    /// 研究专用：完全不做库存封锁/缩量。实盘路径必须拒绝
    ResearchNoCap,
}

impl InventoryMode {
    /// 解析模式名。`research_nocap` 额外要求显式研究开关 `SOFT_CAP_RESEARCH=1`——
    /// 实盘的持久化参数文件（data/strategy_config.json）是用户可手改的，
    /// 少了这道闸，手改一行就能让实盘裸奔；缺开关时直接报错而不是静默降级。
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "hard" => Ok(Self::Hard),
            "soft" => Ok(Self::Soft),
            "research_nocap" => {
                if std::env::var("SOFT_CAP_RESEARCH").as_deref() == Ok("1") {
                    Ok(Self::ResearchNoCap)
                } else {
                    bail!("inventory_mode=research_nocap 仅限研究用途，需设置 SOFT_CAP_RESEARCH=1")
                }
            }
            other => bail!("未知 inventory_mode: {other}（可选 hard|soft|research_nocap）"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct InventoryPolicy {
    mode: InventoryMode,
    /// 软上限（网格单位）。仅 Soft 模式使用
    soft_cap_grids: f64,
    /// 硬上限（网格单位）——任何模式下都是最终兜底（ResearchNoCap 除外）
    hard_cap_grids: f64,
}

impl InventoryPolicy {
    /// 同方向开仓的数量缩放系数；`None` = 封锁。
    /// `grids` 为该方向上的净持仓格数（买单看净多头，卖单看净空头）。
    fn same_side_scale(&self, grids: f64) -> Option<f64> {
        match self.mode {
            InventoryMode::ResearchNoCap => Some(1.0),
            InventoryMode::Hard => (grids < self.hard_cap_grids).then_some(1.0),
            InventoryMode::Soft => {
                if grids >= self.hard_cap_grids {
                    None
                } else if grids >= self.soft_cap_grids {
                    let span = self.hard_cap_grids - self.soft_cap_grids;
                    Some((1.0 - (grids - self.soft_cap_grids) / span).max(0.0))
                } else {
                    Some(1.0)
                }
            }
        }
    }
}

struct MarketGridState {
    anchor_price: f64,
    last_mid_price: f64,
    last_signal_time: Option<chrono::DateTime<Utc>>,
    filled_buy: Vec<bool>,
    filled_sell: Vec<bool>,
    /// Rolling price history for trend detection (up to 50 prices)
    price_history: Vec<f64>,
    /// EMA value
    ema: f64,
    /// Far-off tick awaiting confirmation by a second tick at the same level
    pending_outlier: Option<f64>,
    /// Consecutive ticks with anchor drift beyond reset threshold
    drift_streak: u8,
}

impl GridStrategy {
    pub fn new(grid_count: usize, investment_per_grid: f64, price_deviation: f64) -> Self {
        let max_filled = Self::live_hard_cap(grid_count);
        Self {
            grid_count,
            investment_per_grid,
            price_deviation,
            max_filled_per_side: max_filled,
            inventory: InventoryPolicy {
                mode: InventoryMode::Hard,
                soft_cap_grids: max_filled as f64,
                hard_cap_grids: max_filled as f64,
            },
            states: Mutex::new(HashMap::new()),
        }
    }

    /// 实盘硬上限：每侧最多半数网格层，且限制在 [3, 5]
    fn live_hard_cap(grid_count: usize) -> usize {
        let half = grid_count / 2;
        half.min(5).max(3)
    }

    /// 指定库存政策构造（回测/研究用；`Hard` + 缺省值等价于 `new`）。
    ///
    /// **双上限统一**：Soft / ResearchNoCap 下把 `max_filled_per_side` 抬到 `hard_cap`，
    /// 否则「已成交层数」这套独立计数会在 5 层悄悄把软上限重新压回硬上限。
    pub fn with_inventory(
        grid_count: usize,
        investment_per_grid: f64,
        price_deviation: f64,
        mode: InventoryMode,
        soft_cap_grids: Option<f64>,
        hard_cap_grids: Option<f64>,
    ) -> Result<Self> {
        let live_cap = Self::live_hard_cap(grid_count) as f64;
        let hard = hard_cap_grids.unwrap_or(match mode {
            InventoryMode::Hard => live_cap,
            _ => 8.0,
        });
        let soft = soft_cap_grids.unwrap_or(5.0);

        if hard <= 0.0 {
            bail!("hard_cap 必须 > 0（收到 {hard}）");
        }
        if mode == InventoryMode::Soft && !(hard > soft && soft > 0.0) {
            bail!("soft 模式要求 hard_cap > soft_cap > 0（收到 soft={soft}, hard={hard}）");
        }

        let max_filled_per_side = match mode {
            InventoryMode::Hard => Self::live_hard_cap(grid_count),
            _ => hard.ceil() as usize,
        };

        Ok(Self {
            grid_count,
            investment_per_grid,
            price_deviation,
            max_filled_per_side,
            inventory: InventoryPolicy {
                mode,
                soft_cap_grids: soft,
                hard_cap_grids: hard,
            },
            states: Mutex::new(HashMap::new()),
        })
    }

    fn grid_prices(&self, anchor: f64) -> (Vec<f64>, Vec<f64>) {
        let half = self.grid_count / 2;
        let step = anchor * self.price_deviation / half.max(1) as f64;
        let buy_grids: Vec<f64> = (1..=half).map(|i| anchor - i as f64 * step).collect();
        let sell_grids: Vec<f64> = (1..=half).map(|i| anchor + i as f64 * step).collect();
        (buy_grids, sell_grids)
    }
}

#[async_trait]
impl Strategy for GridStrategy {
    fn name(&self) -> &str {
        "grid_trading"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        let mut all_signals = Vec::new();

        for (symbol, ob) in &snapshot.order_books {
            let mid_price = match ob.mid_price() {
                Some(p) if p > 0.0 => p,
                _ => continue,
            };

            let half = self.grid_count / 2;
            let mut states = self.states.lock().unwrap();

            // Get or initialize per-market state
            let state = states.entry(symbol.clone()).or_insert_with(|| {
                info!("Grid anchor set: {:.2} for {}", mid_price, symbol);
                MarketGridState {
                    anchor_price: mid_price,
                    last_mid_price: mid_price,
                    last_signal_time: None,
                    filled_buy: vec![false; half],
                    filled_sell: vec![false; half],
                    price_history: vec![mid_price],
                    ema: mid_price,
                    pending_outlier: None,
                    drift_streak: 0,
                }
            });

            // Outlier filter: a tick >1.5% from last known price is only accepted
            // if the next tick confirms the same level (genuine move); a lone
            // spike that snaps back is dropped without touching grid state.
            let tick_change = (mid_price - state.last_mid_price).abs() / state.last_mid_price;
            if tick_change > 0.015 {
                let confirmed = state
                    .pending_outlier
                    .map(|p| (mid_price - p).abs() / p < 0.005)
                    .unwrap_or(false);
                if !confirmed {
                    debug!(
                        "{} outlier tick pending confirmation: {:.2} -> {:.2} ({:.2}%)",
                        symbol,
                        state.last_mid_price,
                        mid_price,
                        tick_change * 100.0
                    );
                    state.pending_outlier = Some(mid_price);
                    continue;
                }
            }
            state.pending_outlier = None;
            state.last_mid_price = mid_price;

            // Update EMA (20-period exponential moving average)
            state.price_history.push(mid_price);
            if state.price_history.len() > 50 {
                state.price_history.remove(0);
            }
            let alpha = 2.0 / 21.0; // EMA-20
            state.ema = alpha * mid_price + (1.0 - alpha) * state.ema;

            // Multi-tier trend detection
            let trend_pct = (mid_price - state.ema) / state.ema;
            // Tier 1: Very strong trend (>0.6% from EMA) — block ALL counter-trend signals
            let very_bearish = trend_pct < -0.006;
            let very_bullish = trend_pct > 0.006;
            // Tier 2: Strong trend (>0.3% from EMA) — only allow nearest level
            let bearish = trend_pct < -0.003;
            let bullish = trend_pct > 0.003;

            // Use market timestamps so backtests are throttled by simulated time, not wall clock.
            if let Some(last_signal_time) = state.last_signal_time {
                if ob.timestamp.signed_duration_since(last_signal_time) < Duration::seconds(15) {
                    continue;
                }
            }

            // Trailing anchor: gradually blend toward EMA to keep grid centered on market
            let anchor_drift_rate = 0.0005; // 0.05% per tick toward EMA
            state.anchor_price =
                state.anchor_price * (1.0 - anchor_drift_rate) + state.ema * anchor_drift_rate;
            let anchor = state.anchor_price;

            // Reset anchor if price drifted beyond 1.5x the full grid range (faster than 2x).
            // Requires two consecutive drifted ticks so a single distorted tick
            // can't wipe grid state while real orders remain on the exchange.
            let drift = (mid_price - anchor).abs() / anchor;
            if drift > self.price_deviation * 1.5 {
                state.drift_streak += 1;
                if state.drift_streak < 2 {
                    debug!(
                        "{} anchor drift {:.2}% awaiting confirmation",
                        symbol,
                        drift * 100.0
                    );
                    continue;
                }
                state.drift_streak = 0;
                state.anchor_price = mid_price;
                state.filled_buy = vec![false; half];
                state.filled_sell = vec![false; half];
                state.ema = mid_price;
                info!(
                    "Grid anchor reset: {:.2} -> {:.2} for {} (drift {:.2}%)",
                    anchor,
                    mid_price,
                    symbol,
                    drift * 100.0
                );
                continue;
            }
            state.drift_streak = 0;

            let (buy_grids, sell_grids) = self.grid_prices(anchor);

            // Count currently filled levels per side
            let filled_buy_count = state.filled_buy.iter().filter(|&&f| f).count();
            let filled_sell_count = state.filled_sell.iter().filter(|&&f| f).count();

            // Position-aware accumulation cap: the strategy's own filled_buy/filled_sell
            // booleans get wiped on every anchor/auto reset, but the exchange position does
            // not — so cap against the REAL net position. Once net exposure reaches
            // max_filled_per_side grid units on one side, block that side (only allow the
            // reducing direction) so a one-sided bag can't keep growing across resets.
            let grid_unit = self.investment_per_grid / mid_price;
            let net_position = snapshot.positions.get(symbol).copied().unwrap_or(0.0);
            let position_grids = if grid_unit > 0.0 {
                net_position / grid_unit
            } else {
                0.0
            };
            // 库存政策：hard = 到顶即封；soft = 软上限以上线性缩量、硬上限封锁；
            // research_nocap = 不封不缩（仅研究）。None 表示该方向完全封锁。
            let buy_scale = self.inventory.same_side_scale(position_grids);
            let sell_scale = self.inventory.same_side_scale(-position_grids);
            let block_buys_position = buy_scale.is_none();
            let block_sells_position = sell_scale.is_none();

            // Only trust aggressive trend tiers after enough EMA history (avoids false signals on init)
            let has_enough_history = state.price_history.len() >= 10;

            // Check buy grids: price dropped to grid level
            let mut signal_found = false;
            for (i, &grid_price) in buy_grids.iter().enumerate() {
                if i >= state.filled_buy.len() || state.filled_buy[i] {
                    continue;
                }
                // Multi-tier trend filter for buys:
                // Net long already at accumulation cap → stop opening more longs
                if block_buys_position {
                    continue;
                }
                // Very strong downtrend → block ALL buys (only after enough EMA data)
                if has_enough_history && very_bearish {
                    continue;
                }
                // Strong downtrend → only allow nearest buy level (L0)
                if bearish && i >= 1 {
                    continue;
                }
                // Max accumulated position limit per side
                if filled_buy_count >= self.max_filled_per_side {
                    continue;
                }
                if mid_price <= grid_price {
                    let scale = buy_scale.unwrap_or(1.0);
                    let quantity = self.investment_per_grid * scale / grid_price;
                    // 碎单过滤：缩量后名义金额不足 $1 视为无信号（交易所也接不了）
                    if quantity * grid_price < DUST_NOTIONAL {
                        continue;
                    }
                    all_signals.push(TradeSignal {
                        symbol: symbol.clone(),
                        market_id: ob.market_id,
                        side: Side::Buy,
                        price: grid_price,
                        quantity,
                        order_type: OrderType::Limit,
                        reason: format!("Grid Buy L{}: {:.2}", i + 1, grid_price),
                        timestamp: ob.timestamp,
                    });
                    state.filled_buy[i] = true;
                    if i < state.filled_sell.len() {
                        state.filled_sell[i] = false;
                    }
                    state.last_signal_time = Some(ob.timestamp);
                    signal_found = true;
                    break;
                }
            }

            // Check sell grids if no buy signal for this market
            if !signal_found {
                for (i, &grid_price) in sell_grids.iter().enumerate() {
                    if i >= state.filled_sell.len() || state.filled_sell[i] {
                        continue;
                    }
                    // Multi-tier trend filter for sells:
                    // Net short already at accumulation cap → stop opening more shorts
                    if block_sells_position {
                        continue;
                    }
                    // Very strong uptrend → block ALL sells (only after enough EMA data)
                    if has_enough_history && very_bullish {
                        continue;
                    }
                    // Strong uptrend → only allow nearest sell level (L0)
                    if bullish && i >= 1 {
                        continue;
                    }
                    // Max accumulated position limit per side
                    if filled_sell_count >= self.max_filled_per_side {
                        continue;
                    }
                    if mid_price >= grid_price {
                        let scale = sell_scale.unwrap_or(1.0);
                        let quantity = self.investment_per_grid * scale / grid_price;
                        if quantity * grid_price < DUST_NOTIONAL {
                            continue;
                        }
                        all_signals.push(TradeSignal {
                            symbol: symbol.clone(),
                            market_id: ob.market_id,
                            side: Side::Sell,
                            price: grid_price,
                            quantity,
                            order_type: OrderType::Limit,
                            reason: format!("Grid Sell L{}: {:.2}", i + 1, grid_price),
                            timestamp: ob.timestamp,
                        });
                        state.filled_sell[i] = true;
                        if i < state.filled_buy.len() {
                            state.filled_buy[i] = false;
                        }
                        state.last_signal_time = Some(ob.timestamp);
                        break;
                    }
                }
            }

            debug!("{} mid={:.2} anchor={:.2} ema={:.2} trend={:+.3}% {} filled_buy={} filled_sell={} pos_grids={:+.1}{}",
                symbol, mid_price, anchor, state.ema, trend_pct * 100.0,
                if very_bearish { "⬇VERY_BEAR" } else if bearish { "↓BEAR" }
                else if very_bullish { "⬆VERY_BULL" } else if bullish { "↑BULL" }
                else { "→RANGE" },
                filled_buy_count, filled_sell_count, position_grids,
                match (buy_scale, sell_scale) {
                    (None, _) => " [BUYS CAPPED]".to_string(),
                    (_, None) => " [SELLS CAPPED]".to_string(),
                    (Some(b), Some(s)) if b < 1.0 || s < 1.0 =>
                        format!(" [scale buy={:.2} sell={:.2}]", b, s),
                    _ => String::new(),
                });
        }

        if all_signals.is_empty() {
            Ok(None)
        } else {
            Ok(Some(all_signals))
        }
    }

    fn reset(&mut self) {
        let mut states = self.states.lock().unwrap();
        states.clear();
    }

    fn clear_filled_state(&self) {
        let mut states = self.states.lock().unwrap();
        for (symbol, state) in states.iter_mut() {
            let half = state.filled_buy.len();
            state.filled_buy = vec![false; half];
            state.filled_sell = vec![false; half];
            info!("Grid filled state cleared for {}", symbol);
        }
    }
}

#[cfg(test)]
#[path = "grid_soft_cap_tests.rs"]
mod grid_soft_cap_tests;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    pub(crate) fn snapshot(symbol: &str, ts: i64, price: f64) -> MarketSnapshot {
        let mut snap = MarketSnapshot::default();
        snap.order_books.insert(
            symbol.to_string(),
            OrderBook {
                symbol: symbol.to_string(),
                market_id: 1,
                bids: vec![PriceLevel {
                    price: price - 0.1,
                    quantity: 1.0,
                }],
                asks: vec![PriceLevel {
                    price: price + 0.1,
                    quantity: 1.0,
                }],
                timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
            },
        );
        snap
    }

    #[test]
    fn test_grid_prices() {
        let strategy = GridStrategy::new(10, 100.0, 0.02);
        let (buy_grids, sell_grids) = strategy.grid_prices(50000.0);
        assert_eq!(buy_grids.len(), 5);
        assert_eq!(sell_grids.len(), 5);
        assert!(buy_grids.iter().all(|p| *p < 50000.0));
        assert!(sell_grids.iter().all(|p| *p > 50000.0));
    }

    #[test]
    fn test_grid_symmetry() {
        let strategy = GridStrategy::new(20, 50.0, 0.015);
        let (buy_grids, sell_grids) = strategy.grid_prices(10000.0);
        assert_eq!(buy_grids.len(), 10);
        assert_eq!(sell_grids.len(), 10);
        assert!(buy_grids[0] > buy_grids[1]);
        assert!(sell_grids[0] < sell_grids[1]);
    }

    #[tokio::test]
    async fn test_grid_cooldown_uses_market_time() {
        let strategy = GridStrategy::new(4, 100.0, 0.02);
        // half=2, buy grids: [99.0, 98.0], sell grids: [101.0, 102.0]

        // Initial eval: sets anchor at 100.0 (no grid level hit)
        assert!(strategy
            .evaluate(&snapshot("BTC", 1_700_000_000, 100.0))
            .await
            .unwrap()
            .is_none());

        // Price drops to 98.5 → hits buy L0 at 99.0
        let first = strategy
            .evaluate(&snapshot("BTC", 1_700_000_900, 98.5))
            .await
            .unwrap();
        assert!(first.is_some(), "Should trigger buy signal");
        assert_eq!(first.unwrap().len(), 1);

        // Only 5 seconds later → blocked by 15s cooldown
        let blocked = strategy
            .evaluate(&snapshot("BTC", 1_700_000_905, 98.5))
            .await
            .unwrap();
        assert!(blocked.is_none(), "Should be blocked by cooldown");

        // After cooldown (900s later) at same price → L0 already filled, L1 blocked by trend filter
        let after_cooldown = strategy
            .evaluate(&snapshot("BTC", 1_700_001_800, 98.5))
            .await
            .unwrap();
        assert!(
            after_cooldown.is_none(),
            "L1 should be blocked by bearish trend filter"
        );
    }

    #[tokio::test]
    async fn test_grid_trend_filter_blocks_deep_buys() {
        let strategy = GridStrategy::new(6, 100.0, 0.03);
        // half=3, step = 100 * 0.03 / 3 = 1.0
        // buy grids: ~[99.0, 98.0, 97.0]

        // Set anchor
        strategy
            .evaluate(&snapshot("BTC", 1_700_000_000, 100.0))
            .await
            .unwrap();

        // Price drops to clearly hit buy L0 (bearish but L0 allowed)
        let sig = strategy
            .evaluate(&snapshot("BTC", 1_700_000_100, 98.5))
            .await
            .unwrap();
        assert!(
            sig.is_some(),
            "Buy L0 should be allowed even in bearish trend"
        );

        // Price drops further → L1 blocked by bearish filter (i >= 1)
        let sig2 = strategy
            .evaluate(&snapshot("BTC", 1_700_000_200, 97.5))
            .await
            .unwrap();
        assert!(
            sig2.is_none(),
            "Buy L1 should be blocked by bearish trend filter"
        );
    }

    #[tokio::test]
    async fn test_position_cap_blocks_same_direction() {
        // grid_count=6 → max_filled_per_side = 3. investment_per_grid=100.
        let strategy = GridStrategy::new(6, 100.0, 0.03);
        // grid_unit at price 100 = 100/100 = 1.0 → cap = 3 units = 3.0 net long.

        // Set anchor at 100.0
        strategy
            .evaluate(&snapshot("BTC", 1_700_000_000, 100.0))
            .await
            .unwrap();

        // A hit on buy L0 with a large existing net long (5 grid units) must be blocked.
        let mut snap = snapshot("BTC", 1_700_000_100, 98.5);
        snap.positions.insert("BTC".to_string(), 5.0); // net long well above cap of 3
        let capped = strategy.evaluate(&snap).await.unwrap();
        assert!(
            capped.is_none(),
            "Buys must be blocked once net long is at the accumulation cap"
        );

        // Same price, but flat position → buy L0 is allowed again.
        let flat = strategy
            .evaluate(&snapshot("BTC", 1_700_000_200, 98.5))
            .await
            .unwrap();
        assert!(
            flat.is_some(),
            "Buy should be allowed when position is within cap"
        );
    }

    #[tokio::test]
    async fn test_position_cap_still_allows_reducing_side() {
        let strategy = GridStrategy::new(6, 100.0, 0.03);
        // Set anchor at 100.0
        strategy
            .evaluate(&snapshot("BTC", 1_700_000_000, 100.0))
            .await
            .unwrap();

        // Net long above cap, price rises to hit a SELL grid (reducing) → must be allowed.
        let mut snap = snapshot("BTC", 1_700_000_100, 101.5);
        snap.positions.insert("BTC".to_string(), 5.0);
        let sig = strategy.evaluate(&snap).await.unwrap();
        assert!(
            sig.is_some(),
            "Reducing sells must still fire even when net long is capped"
        );
        assert_eq!(sig.unwrap()[0].side, Side::Sell);
    }
}
