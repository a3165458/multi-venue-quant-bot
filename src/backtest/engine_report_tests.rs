//! Report formatting regressions for alpha/commission fields.

use super::*;

#[test]
fn format_summary_includes_alpha_fields() {
    let results = BacktestResults {
        total_return: 0.05,
        sharpe_ratio: 1.0,
        max_drawdown: 0.02,
        win_rate: 0.5,
        trades: vec![],
        equity_curve: vec![],
        initial_capital: 1000.0,
        final_capital: 1050.0,
        total_trades: 0,
        winning_trades: 0,
        losing_trades: 0,
        avg_profit: 0.0,
        avg_loss: 0.0,
        profit_factor: 0.0,
        benchmark_return: 0.03,
        excess_return: 0.02,
        total_commission: 1.25,
        total_adverse_selection: 0.0,
        peak_notional: 421.5,
        peak_leverage: 1.75,
        peak_position_grids: 6.5,
        liq_count: 2,
        bars_over_soft_cap: 17,
        blocked_by_profitability: 3,
        blocked_by_position_limit: 0,
        blocked_by_total_position_limit: 0,
        stop_loss_exits: 0,
        take_profit_exits: 0,
    };
    let text = crate::backtest::metrics::format_summary_for_test(&results);
    assert!(
        text.contains("基准收益") || text.contains("benchmark"),
        "missing benchmark: {text}"
    );
    assert!(
        text.contains("超额收益") || text.contains("excess"),
        "missing excess: {text}"
    );
    assert!(
        text.contains("总手续费") || text.contains("commission") || text.contains("1.25"),
        "missing commission: {text}"
    );
    assert!(
        text.contains("收益门槛拦截") || text.contains("blocked"),
        "missing blocked: {text}"
    );
}

#[test]
fn format_summary_includes_peak_risk_fields() {
    let results = BacktestResults {
        total_return: -0.01,
        sharpe_ratio: -0.5,
        max_drawdown: 0.08,
        win_rate: 0.4,
        trades: vec![],
        equity_curve: vec![],
        initial_capital: 255.0,
        final_capital: 252.45,
        total_trades: 0,
        winning_trades: 0,
        losing_trades: 0,
        avg_profit: 0.0,
        avg_loss: 0.0,
        profit_factor: 0.0,
        benchmark_return: 0.0,
        excess_return: -0.01,
        total_commission: 0.0,
        total_adverse_selection: 0.0,
        peak_notional: 421.5,
        peak_leverage: 1.75,
        peak_position_grids: 6.5,
        liq_count: 2,
        bars_over_soft_cap: 17,
        blocked_by_profitability: 0,
        blocked_by_position_limit: 0,
        blocked_by_total_position_limit: 0,
        stop_loss_exits: 0,
        take_profit_exits: 0,
    };
    let text = crate::backtest::metrics::format_summary_for_test(&results);
    for needle in [
        "峰值名义敞口",
        "421.50",
        "峰值杠杆",
        "1.75",
        "峰值持仓格数",
        "6.50",
        "强平次数",
    ] {
        assert!(text.contains(needle), "missing {needle}: {text}");
    }
}
