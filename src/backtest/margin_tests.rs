//! 线性保证金模型的数值口径测试（TDD 先行部分）。

use super::*;

#[test]
fn peaks_track_max_notional_and_leverage() {
    let mut t = MarginTracker::default();
    // equity=1000, notional=500 → lev 0.5；IM = 500/3 = 166.67 < 1000 → 不强平
    assert!(!t.observe_bar(500.0, 1000.0));
    // equity=1000, notional=1200 → lev 1.2；IM = 400 < 1000 → 仍不强平
    assert!(!t.observe_bar(1200.0, 1000.0));
    // 更小的敞口不应拉低峰值
    assert!(!t.observe_bar(300.0, 1000.0));

    assert!((t.peak_notional() - 1200.0).abs() < 1e-9);
    assert!((t.peak_leverage() - 1.2).abs() < 1e-9);
    assert_eq!(t.liq_count(), 0);
}

#[test]
fn flat_position_bars_are_ignored() {
    let mut t = MarginTracker::default();
    assert!(!t.observe_bar(0.0, 1000.0));
    assert_eq!(t.peak_notional(), 0.0);
    assert_eq!(t.peak_leverage(), 0.0);
}

#[test]
fn liquidates_exactly_when_free_margin_hits_zero() {
    let mut t = MarginTracker::default(); // max_leverage 3.0
                                          // IM = 300/3 = 100；equity 100.01 → free margin > 0 → 不强平
    assert!(!t.observe_bar(300.0, 100.01));
    // equity 恰好 = IM → free_margin == 0 → 按 `<= 0` 规则强平
    assert!(t.observe_bar(300.0, 100.0));
    t.record_liquidation();
    assert_eq!(t.liq_count(), 1);
}

#[test]
fn max_leverage_override_changes_liq_threshold() {
    let mut t = MarginTracker::default();
    t.set_max_leverage(10.0);
    // IM = 300/10 = 30 < equity 100 → 10x 下不强平（3x 下会）
    assert!(!t.observe_bar(300.0, 100.0));
}

#[test]
fn grid_metrics_require_explicit_unit() {
    let mut t = MarginTracker::default();
    t.observe_bar(300.0, 1000.0);
    // 未设置单格名义 → 网格数指标保持 0，不做任何反推
    assert_eq!(t.peak_position_grids(), 0.0);
    assert_eq!(t.bars_over_soft_cap(), 0);

    let mut t2 = MarginTracker::default();
    t2.set_grid_unit_notional(30.0);
    t2.set_soft_cap_grids(5.0);
    t2.observe_bar(120.0, 1000.0); // 4 格 → 未超软上限
    t2.observe_bar(195.0, 1000.0); // 6.5 格 → 超软上限
    t2.observe_bar(150.0, 1000.0); // 5 格 → 恰好达到软上限（>= 计入）
    assert!((t2.peak_position_grids() - 6.5).abs() < 1e-9);
    assert_eq!(t2.bars_over_soft_cap(), 2);
}
