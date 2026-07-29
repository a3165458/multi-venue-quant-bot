//! `create_strategy*` 的库存政策参数解析与实盘拒绝规则。

use super::*;
use crate::strategy::grid_strategy::tests::snapshot;
use serial_test::serial;

fn live_config(inventory_mode: Option<&str>) -> Config {
    let mut b = Config::builder()
        .set_override("trading.strategies.grid_trading.enabled", true)
        .unwrap()
        .set_override("trading.strategies.grid_trading.grid_count", 12)
        .unwrap()
        .set_override("trading.strategies.grid_trading.investment_per_grid", 30.0)
        .unwrap()
        .set_override("trading.strategies.grid_trading.price_deviation", 0.004)
        .unwrap();
    if let Some(mode) = inventory_mode {
        b = b
            .set_override("trading.strategies.grid_trading.inventory_mode", mode)
            .unwrap();
    }
    b.build().unwrap()
}

#[test]
fn params_default_to_hard_when_unspecified() {
    let s =
        create_strategy_with_params("grid", Some("grid_count=12,investment=30,deviation=0.004"))
            .expect("默认构造");
    assert_eq!(s.name(), "grid_trading");
}

#[test]
fn params_accept_soft_mode() {
    assert!(create_strategy_with_params(
        "grid",
        Some(
            "grid_count=12,investment=30,deviation=0.004,inventory_mode=soft,soft_cap=5,hard_cap=8"
        )
    )
    .is_ok());
}

#[test]
fn params_reject_unknown_mode_and_bad_caps() {
    assert!(create_strategy_with_params("grid", Some("inventory_mode=loose")).is_err());
    assert!(
        create_strategy_with_params("grid", Some("inventory_mode=soft,soft_cap=8,hard_cap=5"))
            .is_err(),
        "hard 必须大于 soft"
    );
    assert!(
        create_strategy_with_params("grid", Some("inventory_mode=soft,soft_cap=abc")).is_err(),
        "非数字上限必须报错而不是静默取默认值"
    );
}

#[tokio::test]
async fn soft_params_actually_scale_signals() {
    // 通过 trait 驱动，证明参数确实生效（而不仅仅是构造成功）
    let s = create_strategy_with_params(
        "grid",
        Some(
            "grid_count=6,investment=100,deviation=0.03,inventory_mode=soft,soft_cap=5,hard_cap=8",
        ),
    )
    .unwrap();
    s.evaluate(&snapshot("BTC", 1_700_000_000, 100.0))
        .await
        .unwrap();
    let mut snap = snapshot("BTC", 1_700_000_100, 98.5);
    snap.positions.insert("BTC".to_string(), 6.5 * 100.0 / 98.5); // 6.5 格
    let sigs = s.evaluate(&snap).await.unwrap().expect("缩量信号");
    assert!((sigs[0].quantity * sigs[0].price - 50.0).abs() < 1e-9);
}

#[test]
#[serial]
fn research_nocap_requires_explicit_research_flag() {
    std::env::remove_var("SOFT_CAP_RESEARCH");
    assert!(
        create_strategy_with_params("grid", Some("inventory_mode=research_nocap")).is_err(),
        "缺开关时必须拒绝"
    );

    std::env::set_var("SOFT_CAP_RESEARCH", "1");
    let ok = create_strategy_with_params("grid", Some("inventory_mode=research_nocap"));
    std::env::remove_var("SOFT_CAP_RESEARCH");
    assert!(ok.is_ok(), "带研究开关时回测路径应放行");
}

#[test]
#[serial]
fn live_path_rejects_research_nocap_even_with_flag() {
    std::env::set_var("SOFT_CAP_RESEARCH", "1");
    let res = create_strategy(&live_config(Some("research_nocap")));
    std::env::remove_var("SOFT_CAP_RESEARCH");
    assert!(res.is_err(), "实盘 yaml 路径必须无条件拒绝 research_nocap");
}

#[test]
fn live_path_defaults_to_hard_and_accepts_soft() {
    assert!(create_strategy(&live_config(None)).is_ok());
    assert!(create_strategy(&live_config(Some("soft"))).is_ok());
    assert!(create_strategy(&live_config(Some("nonsense"))).is_err());
}
