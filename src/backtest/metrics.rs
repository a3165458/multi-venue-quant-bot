use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::info;

use super::engine::BacktestResults;

/// 生成回测报告
pub async fn generate_report(results: &BacktestResults, output_dir: &str) -> Result<()> {
    fs::create_dir_all(output_dir)?;

    // 生成文本摘要
    let summary = format_summary(results);
    let summary_path = Path::new(output_dir).join("summary.txt");
    fs::write(&summary_path, &summary)?;
    info!("报告已保存: {}", summary_path.display());

    // 生成交易记录CSV
    let trades_path = Path::new(output_dir).join("trades.csv");
    write_trades_csv(results, &trades_path)?;

    // 生成权益曲线CSV
    let equity_path = Path::new(output_dir).join("equity_curve.csv");
    write_equity_csv(results, &equity_path)?;

    Ok(())
}

fn format_summary(results: &BacktestResults) -> String {
    format!(
        r#"========================================
    回测报告
========================================

基本信息:
  初始资金:     ${:.2}
  最终资金:     ${:.2}
  总收益率:     {:.2}%
  基准收益:     {:.2}%
  超额收益:     {:.2}%
  总手续费:     ${:.4}
  逆向选择成本: ${:.4}

绩效指标:
  夏普比率:     {:.3}
  最大回撤:     {:.2}%
  盈亏比:       {:.2}

交易统计:
  总交易次数:   {}
  盈利交易:     {}
  亏损交易:     {}
  胜率:         {:.1}%
  平均盈利:     ${:.2}
  平均亏损:     ${:.2}

风险指标 (线性保证金 v1, 收盘价口径):
  峰值名义敞口: ${:.2}
  峰值杠杆:     {:.2}x
  峰值持仓格数: {:.2}
  强平次数:     {}
  超软上限K线:  {}
  收益门槛拦截: {}
  持仓上限拦截: {}
  账户上限拦截: {}
  止损退出:     {}
  止盈退出:     {}

========================================
"#,
        results.initial_capital,
        results.final_capital,
        results.total_return * 100.0,
        results.benchmark_return * 100.0,
        results.excess_return * 100.0,
        results.total_commission,
        results.total_adverse_selection,
        results.sharpe_ratio,
        results.max_drawdown * 100.0,
        results.profit_factor,
        results.total_trades,
        results.winning_trades,
        results.losing_trades,
        results.win_rate * 100.0,
        results.avg_profit,
        results.avg_loss,
        results.peak_notional,
        results.peak_leverage,
        results.peak_position_grids,
        results.liq_count,
        results.bars_over_soft_cap,
        results.blocked_by_profitability,
        results.blocked_by_position_limit,
        results.blocked_by_total_position_limit,
        results.stop_loss_exits,
        results.take_profit_exits,
    )
}

#[cfg(test)]
pub(crate) fn format_summary_for_test(results: &BacktestResults) -> String {
    format_summary(results)
}

/// 写入交易记录CSV
fn write_trades_csv(results: &BacktestResults, path: &Path) -> Result<()> {
    let mut csv = String::from("timestamp,symbol,side,price,quantity,pnl,commission\n");

    for trade in &results.trades {
        csv.push_str(&format!(
            "{},{},{:?},{:.6},{:.6},{:.6},{:.6}\n",
            trade.timestamp.to_rfc3339(),
            trade.symbol,
            trade.side,
            trade.price,
            trade.quantity,
            trade.pnl,
            trade.commission,
        ));
    }

    fs::write(path, csv)?;
    Ok(())
}

/// 写入权益曲线CSV
fn write_equity_csv(results: &BacktestResults, path: &Path) -> Result<()> {
    let mut csv = String::from("timestamp,equity\n");

    for (timestamp, equity) in &results.equity_curve {
        csv.push_str(&format!("{},{:.2}\n", timestamp.to_rfc3339(), equity,));
    }

    fs::write(path, csv)?;
    Ok(())
}
