//! Policy C 软上限：缩量表、硬上限封锁、双上限统一、构造校验。

use super::tests::snapshot;
use super::*;

const INVESTMENT: f64 = 100.0;

fn soft_strategy() -> GridStrategy {
    // grid_count=6 → half=3；step = 100*0.03/3 = 1.0 → 买格 [99, 98, 97]
    GridStrategy::with_inventory(
        6,
        INVESTMENT,
        0.03,
        InventoryMode::Soft,
        Some(5.0),
        Some(8.0),
    )
    .expect("soft 构造")
}

/// 把「目标网格数」换算成快照里的净持仓（grid_unit = investment / mid_price）
fn net_position_for(grids: f64, mid_price: f64) -> f64 {
    grids * INVESTMENT / mid_price
}

/// 在 mid=98.5 处触发买单 L0（格价 ≈99.0），返回该信号的名义金额 qty*price。
/// 用名义金额而不是数量断言：trailing anchor 每次评估都会微漂，格价不是整数 99.0，
/// 而 `名义 = investment * scale` 才是缩量政策真正的不变量。
async fn buy_notional_at_grids(strategy: &GridStrategy, grids: f64) -> Option<f64> {
    strategy
        .evaluate(&snapshot("BTC", 1_700_000_000, 100.0))
        .await
        .unwrap(); // 设定 anchor=100
    let mut snap = snapshot("BTC", 1_700_000_100, 98.5);
    snap.positions
        .insert("BTC".to_string(), net_position_for(grids, 98.5));
    strategy
        .evaluate(&snap)
        .await
        .unwrap()
        .map(|sigs| sigs[0].quantity * sigs[0].price)
}

#[test]
fn scale_table_matches_policy_c() {
    let p = InventoryPolicy {
        mode: InventoryMode::Soft,
        soft_cap_grids: 5.0,
        hard_cap_grids: 8.0,
    };
    assert_eq!(p.same_side_scale(0.0), Some(1.0));
    assert_eq!(p.same_side_scale(4.99), Some(1.0));
    assert_eq!(p.same_side_scale(5.0), Some(1.0), "软上限处仍是满仓位");
    assert!((p.same_side_scale(6.5).unwrap() - 0.5).abs() < 1e-9);
    assert!((p.same_side_scale(7.0).unwrap() - 1.0 / 3.0).abs() < 1e-9);
    assert_eq!(p.same_side_scale(8.0), None, "硬上限必须封锁");
    assert_eq!(p.same_side_scale(9.0), None);
}

#[test]
fn hard_mode_keeps_step_function() {
    let p = InventoryPolicy {
        mode: InventoryMode::Hard,
        soft_cap_grids: 5.0,
        hard_cap_grids: 5.0,
    };
    assert_eq!(p.same_side_scale(4.9), Some(1.0), "hard 模式不缩量");
    assert_eq!(p.same_side_scale(5.0), None);
}

#[test]
fn research_nocap_never_blocks() {
    let p = InventoryPolicy {
        mode: InventoryMode::ResearchNoCap,
        soft_cap_grids: 5.0,
        hard_cap_grids: 8.0,
    };
    assert_eq!(p.same_side_scale(50.0), Some(1.0));
}

#[tokio::test]
async fn soft_scales_quantity_at_mid_range() {
    let notional = buy_notional_at_grids(&soft_strategy(), 6.5)
        .await
        .expect("6.5 格应仍有信号（缩量而非封锁）");
    assert!(
        (notional - 0.5 * INVESTMENT).abs() < 1e-9,
        "6.5 格应为半仓位: {notional} vs {}",
        0.5 * INVESTMENT
    );
}

#[tokio::test]
async fn soft_keeps_full_size_below_soft_cap() {
    let notional = buy_notional_at_grids(&soft_strategy(), 4.0)
        .await
        .expect("信号");
    assert!(
        (notional - INVESTMENT).abs() < 1e-9,
        "软上限以下应满仓位: {notional}"
    );
}

#[tokio::test]
async fn soft_blocks_at_hard_cap() {
    assert!(
        buy_notional_at_grids(&soft_strategy(), 8.0).await.is_none(),
        "到硬上限必须封锁同方向开仓"
    );
}

#[tokio::test]
async fn soft_still_sizes_above_five_grids() {
    // 证明「已成交层数」上限没有把软上限偷偷压回 5：6 格仍应出单
    let notional = buy_notional_at_grids(&soft_strategy(), 6.0)
        .await
        .expect("6 格应仍出单");
    assert!(
        (notional - INVESTMENT * 2.0 / 3.0).abs() < 1e-9,
        "6 格应为 2/3 仓位: {notional}"
    );
    assert_eq!(soft_strategy().max_filled_per_side, 8);
}

#[tokio::test]
async fn dust_signal_is_dropped() {
    // soft=5, hard=8, 极小 investment：7.99 格时缩放系数 ~0.0033 → 名义 << $1
    let strategy = GridStrategy::with_inventory(
        6,
        INVESTMENT,
        0.03,
        InventoryMode::Soft,
        Some(5.0),
        Some(8.0),
    )
    .unwrap();
    let notional = buy_notional_at_grids(&strategy, 7.99).await;
    assert!(
        notional.is_none(),
        "缩量后不足 $1 的碎单应被丢弃，实得 {notional:?}"
    );
}

#[tokio::test]
async fn reducing_side_allowed_at_hard_cap() {
    let strategy = soft_strategy();
    strategy
        .evaluate(&snapshot("BTC", 1_700_000_000, 100.0))
        .await
        .unwrap();
    // 净多头远超硬上限，价格上行触发卖格（减仓方向）→ 必须放行且满仓位
    let mut snap = snapshot("BTC", 1_700_000_100, 101.5);
    snap.positions
        .insert("BTC".to_string(), net_position_for(20.0, 101.5));
    let sigs = strategy.evaluate(&snap).await.unwrap().expect("减仓单");
    assert_eq!(sigs[0].side, Side::Sell);
    assert!(
        (sigs[0].quantity * sigs[0].price - INVESTMENT).abs() < 1e-9,
        "减仓方向不应被缩量"
    );
}

#[test]
fn construction_validates_caps() {
    assert!(GridStrategy::with_inventory(
        6,
        100.0,
        0.03,
        InventoryMode::Soft,
        Some(8.0),
        Some(5.0)
    )
    .is_err());
    assert!(
        GridStrategy::with_inventory(6, 100.0, 0.03, InventoryMode::Soft, Some(5.0), Some(5.0))
            .is_err(),
        "hard 必须严格大于 soft"
    );
    assert!(
        GridStrategy::with_inventory(6, 100.0, 0.03, InventoryMode::Soft, Some(0.0), Some(8.0))
            .is_err(),
        "soft 必须 > 0"
    );
    // hard 模式沿用实盘上限，不受 soft 参数影响
    let hard = GridStrategy::with_inventory(12, 30.0, 0.004, InventoryMode::Hard, None, None)
        .expect("hard 构造");
    assert_eq!(hard.max_filled_per_side, 5);
    assert_eq!(hard.inventory.hard_cap_grids, 5.0);
}

#[test]
fn mode_parsing_and_research_gate() {
    assert_eq!(InventoryMode::parse("hard").unwrap(), InventoryMode::Hard);
    assert_eq!(InventoryMode::parse(" SOFT ").unwrap(), InventoryMode::Soft);
    assert!(InventoryMode::parse("bogus").is_err());
    // research_nocap 需要显式研究开关；此测试进程未设置 → 必须报错
    if std::env::var("SOFT_CAP_RESEARCH").is_err() {
        assert!(
            InventoryMode::parse("research_nocap").is_err(),
            "缺少 SOFT_CAP_RESEARCH=1 时必须拒绝 research_nocap"
        );
    }
}
