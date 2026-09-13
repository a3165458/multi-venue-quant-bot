//! Crossed-book basis between two books on the same underlying.
//!
//! Same-ecosystem HIP-3 basis (for example `io:SNDK` vs `xyz:SNDK`), not CEX
//! arb. A round-trip is tradeable only when buying one ask and selling the
//! other bid clears the configured taker cost plus a net buffer.
//!
//! Operator fee note: Entropy Tier 4 claims a 200% self-rebate on Entropy's
//! HIP-3 fee share (plus a referred-user benefit). Effective net taker cost
//! after rebate is ≈0 to slightly negative. The Hyperliquid `userFees` API
//! still prints raw 1.5 / 4.5 bps — that is **not** this operator's true cost.
//! Defaults therefore follow a Tier-4-ish ~0 fee plus a small safety buffer.
//! `fee_preset: growth` keeps the conservative HIP-3 growth-mode constants.

use config::Config;

use crate::risk::profitability::HIP3_GROWTH_TAKER_FEE_BPS;

/// Two-sided crossed-book edge in basis points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrossedBasis {
    pub buy_a_sell_b_bps: f64,
    pub buy_b_sell_a_bps: f64,
}

/// HIP-3 growth-mode cost of taking both legs.
pub fn hip3_cross_dex_taker_cost_bps() -> f64 {
    HIP3_GROWTH_TAKER_FEE_BPS * 2.0
}

/// Minimum net bps after two taker fees before a crossed book is called tradeable.
pub const CROSS_DEX_MIN_NET_BPS: f64 = 1.0;

/// Tier-4-ish round-trip taker cost after Entropy rebate, plus a small buffer.
/// Do not substitute raw Hyperliquid `userFees` (1.5 / 4.5 bps) for this.
pub const TIER4_ROUND_TRIP_TAKER_BPS: f64 = 0.2;

/// YAML `fee_preset` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeePreset {
    /// ~0 effective fee + 0.2 bps safety buffer.
    Tier4,
    /// Conservative HIP-3 growth-mode two-sided taker (~1.72 bps).
    Growth,
}

impl FeePreset {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "growth" | "hip3_growth" | "conservative" => Self::Growth,
            _ => Self::Tier4,
        }
    }

    pub fn round_trip_taker_bps(self) -> f64 {
        match self {
            Self::Tier4 => TIER4_ROUND_TRIP_TAKER_BPS,
            Self::Growth => hip3_cross_dex_taker_cost_bps(),
        }
    }
}

/// Disabled-by-default live / paper settings for the HIP-3 basis path.
#[derive(Debug, Clone, PartialEq)]
pub struct CrossDexBasisConfig {
    pub enabled: bool,
    /// Live IOC hedge legs. Must stay false unless the operator arms it.
    pub armed: bool,
    pub pair_a: String,
    pub pair_b: String,
    pub fee_preset: FeePreset,
    pub round_trip_taker_bps: f64,
    pub min_net_bps: f64,
    pub max_notional_usd: f64,
    pub max_abs_delta: f64,
    pub max_gross_notional: f64,
    pub cooldown_secs: u64,
    pub unwind_timeout_secs: u64,
    pub book_stale_ms: u64,
}

impl Default for CrossDexBasisConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            armed: false,
            pair_a: "io:SNDK".to_string(),
            pair_b: "xyz:SNDK".to_string(),
            fee_preset: FeePreset::Tier4,
            round_trip_taker_bps: TIER4_ROUND_TRIP_TAKER_BPS,
            min_net_bps: CROSS_DEX_MIN_NET_BPS,
            max_notional_usd: 40.0,
            max_abs_delta: 0.02,
            max_gross_notional: 80.0,
            cooldown_secs: 5,
            unwind_timeout_secs: 8,
            book_stale_ms: 3_000,
        }
    }
}

impl CrossDexBasisConfig {
    pub fn from_settings(settings: &Config) -> Self {
        let prefix = "trading.strategies.cross_dex_basis";
        let mut cfg = Self::default();
        cfg.enabled = settings
            .get_bool(&format!("{prefix}.enabled"))
            .unwrap_or(false);
        let armed = settings
            .get_bool(&format!("{prefix}.armed"))
            .unwrap_or(false);
        // Never arm a disabled strategy. Live fire also requires !paused at runtime.
        cfg.armed = cfg.enabled && armed;
        if let Ok(pair_a) = settings.get_string(&format!("{prefix}.pair_a")) {
            let pair_a = pair_a.trim().to_string();
            if !pair_a.is_empty() {
                cfg.pair_a = pair_a;
            }
        }
        if let Ok(pair_b) = settings.get_string(&format!("{prefix}.pair_b")) {
            let pair_b = pair_b.trim().to_string();
            if !pair_b.is_empty() {
                cfg.pair_b = pair_b;
            }
        }
        if let Ok(preset) = settings.get_string(&format!("{prefix}.fee_preset")) {
            cfg.fee_preset = FeePreset::parse(&preset);
            cfg.round_trip_taker_bps = cfg.fee_preset.round_trip_taker_bps();
        }
        if let Ok(rt) = settings.get_float(&format!("{prefix}.round_trip_taker_bps")) {
            cfg.round_trip_taker_bps = rt;
        }
        if let Ok(min_net) = settings.get_float(&format!("{prefix}.min_net_bps")) {
            cfg.min_net_bps = min_net;
        }
        if let Ok(notional) = settings.get_float(&format!("{prefix}.max_notional")) {
            cfg.max_notional_usd = notional;
        }
        if let Ok(delta) = settings.get_float(&format!("{prefix}.max_abs_delta")) {
            cfg.max_abs_delta = delta;
        }
        if let Ok(gross) = settings.get_float(&format!("{prefix}.max_gross_notional")) {
            cfg.max_gross_notional = gross;
        }
        if let Ok(secs) = settings.get_int(&format!("{prefix}.cooldown_secs")) {
            cfg.cooldown_secs = secs.max(0) as u64;
        }
        if let Ok(secs) = settings.get_int(&format!("{prefix}.unwind_timeout_secs")) {
            cfg.unwind_timeout_secs = secs.max(1) as u64;
        }
        if let Ok(ms) = settings.get_int(&format!("{prefix}.book_stale_ms")) {
            cfg.book_stale_ms = ms.max(100) as u64;
        }
        cfg
    }

    /// Extra coins that must be resolved / subscribed when the strategy is on,
    /// without adding them to the maker quote set.
    pub fn extra_symbols(&self, maker_coins: &[String]) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        self.pair_coins()
            .into_iter()
            .filter(|coin| !maker_coins.iter().any(|maker| maker == coin))
            .collect()
    }

    pub fn pair_coins(&self) -> Vec<String> {
        vec![self.pair_a.clone(), self.pair_b.clone()]
    }

    pub fn is_pair_coin(&self, coin: &str) -> bool {
        coin == self.pair_a || coin == self.pair_b
    }

    /// Live IOC hedge. Paper / log-only unless enabled, armed, and not paused.
    pub fn live_fire_allowed(&self, paused: bool) -> bool {
        live_fire_allowed(self.enabled, self.armed, paused)
    }
}

/// Merge maker symbols with basis pair coins when the strategy is enabled.
pub fn expand_live_symbols(maker_coins: &[String], cfg: &CrossDexBasisConfig) -> Vec<String> {
    let extras = cfg.extra_symbols(maker_coins);
    if extras.is_empty() {
        return maker_coins.to_vec();
    }
    let mut coins = maker_coins.to_vec();
    coins.extend(extras);
    coins.sort();
    coins.dedup();
    coins
}

pub fn live_fire_allowed(enabled: bool, armed: bool, paused: bool) -> bool {
    enabled && armed && !paused
}

/// `buy_a_sell_b` is `(bid_b - ask_a) / mid * 10_000`.
pub fn crossed_basis_bps(bid_a: f64, ask_a: f64, bid_b: f64, ask_b: f64) -> Option<CrossedBasis> {
    if !(bid_a.is_finite()
        && ask_a.is_finite()
        && bid_b.is_finite()
        && ask_b.is_finite()
        && bid_a > 0.0
        && ask_a > bid_a
        && bid_b > 0.0
        && ask_b > bid_b)
    {
        return None;
    }
    Some(CrossedBasis {
        buy_a_sell_b_bps: (bid_b - ask_a) / ((bid_b + ask_a) / 2.0) * 10_000.0,
        buy_b_sell_a_bps: (bid_a - ask_b) / ((bid_a + ask_b) / 2.0) * 10_000.0,
    })
}

/// Net edge after two taker fees, if either direction clears [`CROSS_DEX_MIN_NET_BPS`].
pub fn tradeable_edge_bps(
    basis: CrossedBasis,
    round_trip_taker_bps: f64,
) -> Option<(&'static str, f64)> {
    tradeable_edge_bps_with_floor(basis, round_trip_taker_bps, CROSS_DEX_MIN_NET_BPS)
}

/// Net edge after a configurable round-trip cost and minimum net floor.
pub fn tradeable_edge_bps_with_floor(
    basis: CrossedBasis,
    round_trip_taker_bps: f64,
    min_net_bps: f64,
) -> Option<(&'static str, f64)> {
    if !round_trip_taker_bps.is_finite() || round_trip_taker_bps < -5.0 {
        return None;
    }
    if !min_net_bps.is_finite() || min_net_bps < 0.0 {
        return None;
    }
    let buy_a = basis.buy_a_sell_b_bps - round_trip_taker_bps;
    let buy_b = basis.buy_b_sell_a_bps - round_trip_taker_bps;
    if buy_a <= min_net_bps && buy_b <= min_net_bps {
        return None;
    }
    if buy_a >= buy_b {
        Some(("buy_a_sell_b", buy_a))
    } else {
        Some(("buy_b_sell_a", buy_b))
    }
}

/// Shared hedge size: min(ask size, bid size, notional / ask).
pub fn hedge_qty(
    buy_ask_sz: f64,
    sell_bid_sz: f64,
    buy_ask_px: f64,
    max_notional_usd: f64,
    min_notional_usd: f64,
) -> Option<f64> {
    if !(buy_ask_sz.is_finite()
        && sell_bid_sz.is_finite()
        && buy_ask_px.is_finite()
        && max_notional_usd.is_finite()
        && min_notional_usd.is_finite()
        && buy_ask_sz > 0.0
        && sell_bid_sz > 0.0
        && buy_ask_px > 0.0
        && max_notional_usd > 0.0
        && min_notional_usd >= 0.0)
    {
        return None;
    }
    let qty = buy_ask_sz
        .min(sell_bid_sz)
        .min(max_notional_usd / buy_ask_px);
    if qty <= 0.0 || qty * buy_ask_px + 1e-9 < min_notional_usd {
        return None;
    }
    Some(qty)
}

/// True when the post-trade pair stays inside delta and gross notional caps.
pub fn inventory_allows_open(
    pos_a: f64,
    pos_b: f64,
    signed_add_a: f64,
    signed_add_b: f64,
    px_a: f64,
    px_b: f64,
    max_abs_delta: f64,
    max_gross_notional: f64,
) -> bool {
    if !(pos_a.is_finite()
        && pos_b.is_finite()
        && signed_add_a.is_finite()
        && signed_add_b.is_finite()
        && px_a.is_finite()
        && px_b.is_finite()
        && px_a > 0.0
        && px_b > 0.0
        && max_abs_delta >= 0.0
        && max_gross_notional >= 0.0)
    {
        return false;
    }
    let new_a = pos_a + signed_add_a;
    let new_b = pos_b + signed_add_b;
    let delta = (new_a + new_b).abs();
    let gross = new_a.abs() * px_a + new_b.abs() * px_b;
    delta <= max_abs_delta + 1e-12 && gross <= max_gross_notional + 1e-9
}

/// Maker-visible inventory after subtracting basis-reserved size on that coin.
pub fn maker_visible_szi(exchange_szi: f64, reserved_szi: f64) -> f64 {
    if !exchange_szi.is_finite() {
        return 0.0;
    }
    if !reserved_szi.is_finite() {
        return exchange_szi;
    }
    exchange_szi - reserved_szi
}

/// Flatten plan after simultaneous IOC legs. Exactly one fill (or a leftover
/// residual) is closed on the filled coin.
#[derive(Debug, Clone, PartialEq)]
pub enum UnwindPlan {
    None,
    Flatten {
        coin: String,
        /// True = sell to flatten a leftover long.
        sell: bool,
        qty: f64,
    },
}

/// After simultaneous IOC legs: if exactly one filled, flatten that inventory.
pub fn unwind_after_legs(
    buy_coin: &str,
    sell_coin: &str,
    buy_filled_qty: f64,
    sell_filled_qty: f64,
    qty_eps: f64,
) -> UnwindPlan {
    let buy_qty = if buy_filled_qty.is_finite() {
        buy_filled_qty.max(0.0)
    } else {
        0.0
    };
    let sell_qty = if sell_filled_qty.is_finite() {
        sell_filled_qty.max(0.0)
    } else {
        0.0
    };
    let eps = if qty_eps.is_finite() && qty_eps >= 0.0 {
        qty_eps
    } else {
        0.0
    };
    let residual = buy_qty - sell_qty;
    if residual > eps {
        UnwindPlan::Flatten {
            coin: buy_coin.to_string(),
            sell: true,
            qty: residual,
        }
    } else if residual < -eps {
        UnwindPlan::Flatten {
            coin: sell_coin.to_string(),
            sell: false,
            qty: -residual,
        }
    } else {
        UnwindPlan::None
    }
}

pub fn unwind_retry_due(has_residual: bool, deadline_ms: Option<u64>, now_ms: u64) -> bool {
    has_residual
        && deadline_ms
            .map(|deadline| now_ms >= deadline)
            .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_books() {
        assert!(crossed_basis_bps(0.0, 1.0, 1.0, 1.1).is_none());
        assert!(crossed_basis_bps(2.0, 1.0, 1.0, 1.1).is_none());
    }

    #[test]
    fn observed_io_xyz_sndk_is_not_tradeable() {
        // Live sample 2026-08-27: io 1537.9/1538.1, xyz 1537.8/1537.9
        let basis = crossed_basis_bps(1537.9, 1538.1, 1537.8, 1537.9).expect("books");
        assert!(basis.buy_a_sell_b_bps < 0.0);
        assert!(basis.buy_b_sell_a_bps <= 0.0);
        assert_eq!(
            tradeable_edge_bps(basis, hip3_cross_dex_taker_cost_bps()),
            None
        );
        assert_eq!(
            tradeable_edge_bps_with_floor(basis, TIER4_ROUND_TRIP_TAKER_BPS, CROSS_DEX_MIN_NET_BPS),
            None
        );
    }

    #[test]
    fn five_bps_cross_clears_two_hip3_taker_fees() {
        let basis = crossed_basis_bps(100.0, 100.01, 100.06, 100.07).expect("books");
        let (side, net) =
            tradeable_edge_bps(basis, hip3_cross_dex_taker_cost_bps()).expect("tradeable");
        assert_eq!(side, "buy_a_sell_b");
        assert!(net > 3.0, "net={net}");
    }

    #[test]
    fn sub_bps_residual_after_fees_is_not_tradeable() {
        // Gross ~2.0 bps cross minus 1.72 taker round-trip leaves ~0.3 bps.
        let basis = crossed_basis_bps(100.0, 100.01, 100.03, 100.04).expect("books");
        assert_eq!(
            tradeable_edge_bps(basis, hip3_cross_dex_taker_cost_bps()),
            None
        );
    }

    #[test]
    fn tier4_near_zero_cost_makes_two_bps_cross_tradeable() {
        // Same ~2.0 bps gross that growth-mode rejects after 1.72 + 1.0 floor.
        let basis = crossed_basis_bps(100.0, 100.01, 100.03, 100.04).expect("books");
        let (side, net) =
            tradeable_edge_bps_with_floor(basis, TIER4_ROUND_TRIP_TAKER_BPS, CROSS_DEX_MIN_NET_BPS)
                .expect("T4 tradeable");
        assert_eq!(side, "buy_a_sell_b");
        assert!(net > 1.5, "net={net}");
        assert!(net < 2.0, "net={net}");
    }

    #[test]
    fn live_like_ten_bps_cross_clears_tier4_and_growth() {
        let basis = crossed_basis_bps(1538.0, 1538.1, 1540.0, 1540.2).expect("books");
        let (_side, t4) =
            tradeable_edge_bps_with_floor(basis, TIER4_ROUND_TRIP_TAKER_BPS, CROSS_DEX_MIN_NET_BPS)
                .expect("T4");
        let (_side, growth) =
            tradeable_edge_bps(basis, hip3_cross_dex_taker_cost_bps()).expect("growth");
        assert!(t4 > 10.0, "t4={t4}");
        assert!(growth > 8.0, "growth={growth}");
        assert!(t4 > growth);
    }

    #[test]
    fn defaults_are_disarmed_and_use_tier4_cost() {
        let cfg = CrossDexBasisConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.armed);
        assert!(!cfg.live_fire_allowed(false));
        assert_eq!(cfg.fee_preset, FeePreset::Tier4);
        assert!((cfg.round_trip_taker_bps - TIER4_ROUND_TRIP_TAKER_BPS).abs() < 1e-12);
        let settings = Config::builder()
            .add_source(config::File::with_name("config/settings.hyperliquid.yaml"))
            .build()
            .expect("yaml");
        let loaded = CrossDexBasisConfig::from_settings(&settings);
        assert!(!loaded.enabled);
        assert!(!loaded.armed);
        assert!(!loaded.live_fire_allowed(false));
        assert_eq!(loaded.pair_a, "io:SNDK");
        assert_eq!(loaded.pair_b, "xyz:SNDK");
    }

    #[test]
    fn enabled_adds_xyz_without_replacing_maker_symbols() {
        let mut cfg = CrossDexBasisConfig::default();
        cfg.enabled = true;
        let maker = vec!["io:SNDK".to_string(), "io:ANTH".to_string()];
        assert_eq!(cfg.extra_symbols(&maker), vec!["xyz:SNDK".to_string()]);
        let expanded = expand_live_symbols(&maker, &cfg);
        assert!(expanded.contains(&"io:SNDK".to_string()));
        assert!(expanded.contains(&"io:ANTH".to_string()));
        assert!(expanded.contains(&"xyz:SNDK".to_string()));
        cfg.enabled = false;
        assert!(cfg.extra_symbols(&maker).is_empty());
        assert_eq!(expand_live_symbols(&maker, &cfg), maker);
    }

    #[test]
    fn armed_without_enabled_or_while_paused_cannot_fire() {
        assert!(!live_fire_allowed(false, true, false));
        assert!(!live_fire_allowed(true, false, false));
        assert!(!live_fire_allowed(true, true, true));
        assert!(live_fire_allowed(true, true, false));
    }

    #[test]
    fn one_leg_fail_unwinds_the_filled_inventory() {
        assert_eq!(
            unwind_after_legs("io:SNDK", "xyz:SNDK", 0.02, 0.0, 1e-8),
            UnwindPlan::Flatten {
                coin: "io:SNDK".into(),
                sell: true,
                qty: 0.02,
            }
        );
        assert_eq!(
            unwind_after_legs("io:SNDK", "xyz:SNDK", 0.0, 0.02, 1e-8),
            UnwindPlan::Flatten {
                coin: "xyz:SNDK".into(),
                sell: false,
                qty: 0.02,
            }
        );
        assert_eq!(
            unwind_after_legs("io:SNDK", "xyz:SNDK", 0.02, 0.02, 1e-8),
            UnwindPlan::None
        );
        assert_eq!(
            unwind_after_legs("io:SNDK", "xyz:SNDK", 0.0, 0.0, 1e-8),
            UnwindPlan::None
        );
        match unwind_after_legs("io:SNDK", "xyz:SNDK", 0.05, 0.02, 1e-8) {
            UnwindPlan::Flatten { coin, sell, qty } => {
                assert_eq!(coin, "io:SNDK");
                assert!(sell);
                assert!((qty - 0.03).abs() < 1e-12, "qty={qty}");
            }
            other => panic!("expected flatten, got {other:?}"),
        }
    }

    #[test]
    fn hedge_qty_and_inventory_caps() {
        assert_eq!(hedge_qty(0.5, 0.4, 100.0, 40.0, 10.0), Some(0.4));
        assert!(hedge_qty(0.05, 0.04, 100.0, 5.0, 10.0).is_none());
        assert!(inventory_allows_open(
            0.0, 0.0, 0.02, -0.02, 100.0, 100.0, 0.02, 80.0
        ));
        assert!(!inventory_allows_open(
            0.05, 0.0, 0.02, -0.02, 100.0, 100.0, 0.02, 80.0
        ));
        assert!((maker_visible_szi(0.06, 0.02) - 0.04).abs() < 1e-12);
    }

    #[test]
    fn unwind_retry_waits_for_deadline() {
        assert!(!unwind_retry_due(true, Some(50), 40));
        assert!(unwind_retry_due(true, Some(50), 50));
        assert!(!unwind_retry_due(false, Some(0), 100));
        assert!(unwind_retry_due(true, None, 0));
    }
}
