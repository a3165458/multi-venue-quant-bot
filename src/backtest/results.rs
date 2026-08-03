//! Backtest trade/result types and statistics (Sharpe, DD, BH, win stats).

use chrono::{DateTime, Utc};

use crate::lighter::types::{Candlestick, Side};

/// 回测交易记录
#[derive(Debug, Clone)]
pub struct BacktestTrade {
    pub timestamp: DateTime<Utc>,
    pub symbol: String,
    pub side: Side,
    pub price: f64,
    pub quantity: f64,
    pub pnl: f64,
    pub commission: f64,
}

/// 回测结果
#[derive(Debug, Clone)]
pub struct BacktestResults {
    pub total_return: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown: f64,
    pub win_rate: f64,
    pub trades: Vec<BacktestTrade>,
    pub equity_curve: Vec<(DateTime<Utc>, f64)>,
    pub initial_capital: f64,
    pub final_capital: f64,
    pub total_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub avg_profit: f64,
    pub avg_loss: f64,
    pub profit_factor: f64,
    pub benchmark_return: f64,
    pub excess_return: f64,
    pub total_commission: f64,
    /// 峰值毛名义敞口 = max over bars of |qty| * close（收盘价计价，非成本价）
    pub peak_notional: f64,
    /// 峰值杠杆 = max over bars of notional / max(equity, 1e-9)
    pub peak_leverage: f64,
    /// 峰值持仓网格数 = max over bars of notional / grid_unit_notional
    /// （仅当引擎显式设置了 `with_grid_unit_notional` 时才非零）
    pub peak_position_grids: f64,
    /// 线性保证金模型触发的强平次数
    pub liq_count: usize,
    /// 持仓网格数 ≥ soft_cap 的K线数（需显式设置 soft_cap 才统计）
    pub bars_over_soft_cap: usize,
    /// 被收益门槛（profitability gate）拦截的入场信号数
    pub blocked_by_profitability: usize,
}

/// Inputs for result statistics (mirrors engine state at end of run).
pub(crate) struct ResultsCalcInput<'a> {
    pub initial_capital: f64,
    pub capital: f64,
    pub trades: &'a [BacktestTrade],
    pub equity_curve: &'a [(DateTime<Utc>, f64)],
    pub total_commission: f64,
    pub historical_data: &'a [Candlestick],
    pub commission_rate: f64,
    pub slippage_rate: f64,
    pub peak_notional: f64,
    pub peak_leverage: f64,
    pub peak_position_grids: f64,
    pub liq_count: usize,
    pub bars_over_soft_cap: usize,
    pub blocked_by_profitability: usize,
}

pub(crate) fn calculate_results(input: &ResultsCalcInput<'_>) -> BacktestResults {
    let total_return = (input.capital - input.initial_capital) / input.initial_capital;
    let benchmark_return = buy_and_hold_return(input);
    let excess_return = total_return - benchmark_return;

    let winning_trades: Vec<&BacktestTrade> = input.trades.iter().filter(|t| t.pnl > 0.0).collect();
    let losing_trades: Vec<&BacktestTrade> = input.trades.iter().filter(|t| t.pnl <= 0.0).collect();

    let win_rate = if input.trades.is_empty() {
        0.0
    } else {
        winning_trades.len() as f64 / input.trades.len() as f64
    };

    let avg_profit = if winning_trades.is_empty() {
        0.0
    } else {
        winning_trades.iter().map(|t| t.pnl).sum::<f64>() / winning_trades.len() as f64
    };

    let avg_loss = if losing_trades.is_empty() {
        0.0
    } else {
        losing_trades.iter().map(|t| t.pnl).sum::<f64>() / losing_trades.len() as f64
    };

    let total_profit: f64 = winning_trades.iter().map(|t| t.pnl).sum();
    let total_loss: f64 = losing_trades.iter().map(|t| t.pnl.abs()).sum();
    let profit_factor = if total_loss > 0.0 {
        total_profit / total_loss
    } else {
        f64::INFINITY
    };

    let max_drawdown = calculate_max_drawdown(input.equity_curve);
    let sharpe_ratio = calculate_sharpe_ratio(input.equity_curve);

    BacktestResults {
        total_return,
        sharpe_ratio,
        max_drawdown,
        win_rate,
        trades: input.trades.to_vec(),
        equity_curve: input.equity_curve.to_vec(),
        initial_capital: input.initial_capital,
        final_capital: input.capital,
        total_trades: input.trades.len(),
        winning_trades: winning_trades.len(),
        losing_trades: losing_trades.len(),
        avg_profit,
        avg_loss,
        profit_factor,
        benchmark_return,
        excess_return,
        total_commission: input.total_commission,
        peak_notional: input.peak_notional,
        peak_leverage: input.peak_leverage,
        peak_position_grids: input.peak_position_grids,
        liq_count: input.liq_count,
        bars_over_soft_cap: input.bars_over_soft_cap,
        blocked_by_profitability: input.blocked_by_profitability,
    }
}

fn apply_slippage(price: f64, side: Side, slippage_rate: f64) -> f64 {
    match side {
        Side::Buy => price * (1.0 + slippage_rate),
        Side::Sell => price * (1.0 - slippage_rate),
    }
}

fn buy_and_hold_return(input: &ResultsCalcInput<'_>) -> f64 {
    let (Some(first), Some(last)) = (input.historical_data.first(), input.historical_data.last())
    else {
        return 0.0;
    };
    if input.initial_capital <= 0.0 {
        return 0.0;
    }
    let buy_px = apply_slippage(first.close, Side::Buy, input.slippage_rate);
    if buy_px <= 0.0 {
        return 0.0;
    }
    let sell_px = apply_slippage(last.close, Side::Sell, input.slippage_rate);
    let qty = input.initial_capital / buy_px;
    let open_comm = buy_px * input.commission_rate * qty;
    let close_comm = sell_px * input.commission_rate * qty;
    let pnl = (sell_px - buy_px) * qty;
    let final_cap = input.initial_capital - open_comm + pnl - close_comm;
    (final_cap - input.initial_capital) / input.initial_capital
}

/// 计算最大回撤
fn calculate_max_drawdown(equity_curve: &[(DateTime<Utc>, f64)]) -> f64 {
    let mut max_equity = 0.0_f64;
    let mut max_drawdown = 0.0_f64;

    for (_, equity) in equity_curve {
        max_equity = max_equity.max(*equity);
        let drawdown = (max_equity - equity) / max_equity;
        max_drawdown = max_drawdown.max(drawdown);
    }

    max_drawdown
}

/// 计算夏普比率（年化）
fn calculate_sharpe_ratio(equity_curve: &[(DateTime<Utc>, f64)]) -> f64 {
    if equity_curve.len() < 2 {
        return 0.0;
    }

    let returns: Vec<f64> = equity_curve
        .windows(2)
        .map(|w| (w[1].1 - w[0].1) / w[0].1)
        .collect();

    if returns.is_empty() {
        return 0.0;
    }

    let mean_return: f64 = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance: f64 = returns
        .iter()
        .map(|r| (r - mean_return).powi(2))
        .sum::<f64>()
        / returns.len() as f64;
    let std_dev = variance.sqrt();

    if std_dev == 0.0 {
        return 0.0;
    }

    // 按数据实际间隔推断年化因子（每年周期数 = 一年秒数 / 平均K线间隔秒数）
    let first_ts = equity_curve
        .first()
        .map(|(t, _)| *t)
        .unwrap_or_else(Utc::now);
    let last_ts = equity_curve.last().map(|(t, _)| *t).unwrap_or(first_ts);
    let span_secs = (last_ts - first_ts).num_seconds().max(1) as f64;
    let avg_interval_secs = span_secs / (equity_curve.len() - 1) as f64;
    let periods_per_year = (365.25 * 86400.0) / avg_interval_secs.max(1.0);
    let annualized_factor = periods_per_year.sqrt();
    (mean_return / std_dev) * annualized_factor
}
