//! Crossed-book basis between two books on the same underlying.
//!
//! A round-trip is tradeable only when buying one ask and selling the other bid
//! clears two HIP-3 taker fees plus a 1 bps net buffer. Live `io:SNDK` vs
//! `xyz:SNDK` has printed sub-bps residuals; those are not armed.

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

/// Net edge after two taker fees, if either direction is positive.
pub fn tradeable_edge_bps(
    basis: CrossedBasis,
    round_trip_taker_bps: f64,
) -> Option<(&'static str, f64)> {
    if !round_trip_taker_bps.is_finite() || round_trip_taker_bps < 0.0 {
        return None;
    }
    let buy_a = basis.buy_a_sell_b_bps - round_trip_taker_bps;
    let buy_b = basis.buy_b_sell_a_bps - round_trip_taker_bps;
    if buy_a <= CROSS_DEX_MIN_NET_BPS && buy_b <= CROSS_DEX_MIN_NET_BPS {
        return None;
    }
    if buy_a >= buy_b {
        Some(("buy_a_sell_b", buy_a))
    } else {
        Some(("buy_b_sell_a", buy_b))
    }
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
}
