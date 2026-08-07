use anyhow::Result;
use chrono::{DateTime, Utc};
use tracing::{debug, info};

use crate::lighter::types::*;
use crate::risk::profitability::{ProfitabilityGuard, SignalEconomics};
use crate::strategy::Strategy;

use super::margin::MarginTracker;
use super::results::{calculate_results, ResultsCalcInput};

// Re-export public result types for `backtest::engine::{BacktestResults, BacktestTrade}`.
pub use super::results::{BacktestResults, BacktestTrade};

/// 回测引擎
pub struct BacktestEngine {
    initial_capital: f64,
    capital: f64,
    historical_data: Vec<Candlestick>,
    commission_rate: f64,
    slippage_rate: f64,
    trades: Vec<BacktestTrade>,
    equity_curve: Vec<(DateTime<Utc>, f64)>,
    total_commission: f64,
    margin: MarginTracker,
    /// 可选收益门槛：与实盘 `RiskManager::check_signal` 同口径，None = 历史行为（全放行）
    profitability: Option<ProfitabilityGuard>,
    /// 被收益门槛拦截的入场信号数（risk_reducing 永不拦截）
    blocked_by_profitability: usize,
}

impl BacktestEngine {
    pub fn new(initial_capital: f64, historical_data: Vec<Candlestick>) -> Self {
        Self {
            initial_capital,
            capital: initial_capital,
            historical_data,
            commission_rate: 0.001, // 0.1%
            slippage_rate: 0.0005,  // 0.05%
            trades: Vec::new(),
            equity_curve: Vec::new(),
            total_commission: 0.0,
            margin: MarginTracker::default(),
            profitability: None,
            blocked_by_profitability: 0,
        }
    }

    /// 设置手续费率
    #[allow(dead_code)]
    pub fn with_commission(mut self, rate: f64) -> Self {
        self.commission_rate = rate;
        self
    }

    /// 设置滑点
    #[allow(dead_code)]
    pub fn with_slippage(mut self, rate: f64) -> Self {
        self.slippage_rate = rate;
        self
    }

    /// 启用收益门槛（与实盘 RiskManager 同口径）：入场信号必须净收益 > 门槛，
    /// risk_reducing 信号永远放行。不调用 = 历史行为，全部放行。
    #[allow(dead_code)]
    pub fn with_profitability(mut self, guard: ProfitabilityGuard) -> Self {
        self.profitability = Some(guard);
        self
    }

    /// 模拟杠杆上限（决定初始保证金与强平线，默认 3.0）
    #[allow(dead_code)]
    pub fn with_max_leverage(mut self, max_leverage: f64) -> Self {
        self.margin.set_max_leverage(max_leverage);
        self
    }

    /// 单格名义金额（= investment_per_grid），用于统计 peak_position_grids
    #[allow(dead_code)]
    pub fn with_grid_unit_notional(mut self, unit: f64) -> Self {
        self.margin.set_grid_unit_notional(unit);
        self
    }

    /// 软上限网格数，用于统计 bars_over_soft_cap
    #[allow(dead_code)]
    pub fn with_soft_cap_grids(mut self, soft_cap: f64) -> Self {
        self.margin.set_soft_cap_grids(soft_cap);
        self
    }

    /// 运行回测
    pub async fn run(&mut self, strategy: Box<dyn Strategy>) -> Result<BacktestResults> {
        info!("开始回测，数据量: {} 根K线", self.historical_data.len());

        let data = self.historical_data.clone();
        let mut position: Option<(Side, f64, f64)> = None; // (side, entry_price, quantity)

        // 预分配容量提升性能
        self.equity_curve.reserve(data.len());
        self.trades.reserve(data.len() / 10);

        for (i, candle) in data.iter().enumerate() {
            // 只传递最近的窗口数据构建快照，避免 O(n²) 克隆
            // 需要至少2根K线让策略比较前后价格
            let window_start = if i >= 1 { i.saturating_sub(100) } else { 0 };
            let mut snapshot = self.build_snapshot(&data[window_start..=i]);

            // 注入模拟净持仓，使回测能触发持仓感知封顶（与实盘 main.rs 注入行为一致）。
            // 缺此步则 snapshot.positions 恒空 → 封顶永不生效 → 回测反映的是旧的未封顶策略。
            // 环境变量 BACKTEST_NO_CAP=1 可跳过注入，用于封顶 vs 未封顶的 A/B 对照。
            if std::env::var("BACKTEST_NO_CAP").is_err() {
                if let Some((side, _entry, qty)) = position {
                    let signed = match side {
                        Side::Buy => qty,
                        Side::Sell => -qty,
                    };
                    snapshot.positions.insert(candle.symbol.clone(), signed);
                }
            }

            // 评估策略
            if let Some(signals) = strategy.evaluate(&snapshot).await? {
                for signal in signals {
                    // 收益门槛（与实盘 RiskManager::check_signal 同口径）：
                    // 入场信号必须净收益 > min_net_edge；risk_reducing 永远放行。
                    if let Some(guard) = &self.profitability {
                        let economics = if signal.risk_reducing {
                            SignalEconomics::exit()
                        } else {
                            SignalEconomics::entry(signal.expected_edge_bps)
                        };
                        let decision = guard.evaluate(economics);
                        if !decision.allowed {
                            self.blocked_by_profitability += 1;
                            debug!(
                                "收益门槛拦截(回测): {} {:?} reason={}, net={:?}bps",
                                signal.symbol, signal.side, decision.reason, decision.net_edge_bps
                            );
                            continue;
                        }
                    }

                    // 模拟执行
                    let execution_price = self.apply_slippage(signal.price, signal.side);
                    let commission_per_qty = execution_price * self.commission_rate;

                    match position {
                        Some((pos_side, entry_price, pos_qty)) if pos_side == signal.side => {
                            let add_qty = signal.quantity;
                            let add_commission = commission_per_qty * add_qty;
                            let cost = execution_price * add_qty;

                            if cost + add_commission <= self.capital {
                                let new_qty = pos_qty + add_qty;
                                let weighted_entry = ((entry_price * pos_qty)
                                    + (execution_price * add_qty))
                                    / new_qty;
                                self.charge_commission(add_commission);
                                position = Some((pos_side, weighted_entry, new_qty));
                                debug!(
                                    "加仓: {:?} {} @ {:.2}, qty {:.6} -> {:.6}",
                                    signal.side, signal.symbol, execution_price, pos_qty, new_qty
                                );
                            }
                        }
                        Some((pos_side, entry_price, pos_qty)) => {
                            let close_qty = pos_qty.min(signal.quantity);
                            let close_commission = commission_per_qty * close_qty;
                            let pnl = match pos_side {
                                Side::Buy => (execution_price - entry_price) * close_qty,
                                Side::Sell => (entry_price - execution_price) * close_qty,
                            };

                            self.capital += pnl;
                            self.charge_commission(close_commission);
                            self.trades.push(BacktestTrade {
                                timestamp: candle.timestamp,
                                symbol: signal.symbol.clone(),
                                side: signal.side,
                                price: execution_price,
                                quantity: close_qty,
                                pnl,
                                commission: close_commission,
                            });

                            debug!(
                                "平仓: {} @ {:.2}, PnL: {:.2}",
                                signal.symbol, execution_price, pnl
                            );

                            let remaining_pos_qty = pos_qty - close_qty;
                            let remaining_signal_qty = signal.quantity - close_qty;

                            position = if remaining_pos_qty > f64::EPSILON {
                                Some((pos_side, entry_price, remaining_pos_qty))
                            } else {
                                None
                            };

                            if remaining_signal_qty > f64::EPSILON {
                                let open_commission = commission_per_qty * remaining_signal_qty;
                                let cost = execution_price * remaining_signal_qty;
                                if cost + open_commission <= self.capital {
                                    self.charge_commission(open_commission);
                                    position =
                                        Some((signal.side, execution_price, remaining_signal_qty));
                                    debug!(
                                        "反手开仓: {:?} {} @ {:.2}, qty {:.6}",
                                        signal.side,
                                        signal.symbol,
                                        execution_price,
                                        remaining_signal_qty
                                    );
                                }
                            }
                        }
                        None => {
                            let commission = commission_per_qty * signal.quantity;
                            let cost = execution_price * signal.quantity;
                            if cost + commission <= self.capital {
                                self.charge_commission(commission);
                                position = Some((signal.side, execution_price, signal.quantity));
                                debug!(
                                    "开仓: {:?} {} @ {:.2}",
                                    signal.side, signal.symbol, execution_price
                                );
                            } else {
                                // Common when trend notional >> backtest capital: strategy
                                // already advanced internal state but fill never happens.
                                debug!(
                                    "开仓跳过(资金不足): {} {:?} notional≈{:.2} cost={:.2} capital={:.2}",
                                    signal.symbol,
                                    signal.side,
                                    cost,
                                    cost + commission,
                                    self.capital
                                );
                            }
                        }
                    }
                }
            }

            // ===== 线性保证金 v1：峰值风险 + 强平（口径见 backtest::margin） =====
            // 顺序是有意义的：先按本根K线成交后的持仓算 equity/notional，强平在
            // 写入权益曲线**之前**执行，否则被强平的那根K线会记下强平前的权益，
            // MaxDD 与 liq_count 会互相矛盾。
            let mut unrealized_pnl = unrealized_of(position, candle.close);
            if let Some((side, entry, qty)) = position {
                let notional = qty.abs() * candle.close;
                let equity = self.capital + unrealized_pnl;
                if self.margin.observe_bar(notional, equity) {
                    self.force_close(
                        side,
                        entry,
                        qty,
                        candle.close,
                        &candle.symbol,
                        candle.timestamp,
                    );
                    self.margin.record_liquidation();
                    position = None;
                    unrealized_pnl = 0.0;
                    debug!(
                        "强平: {} qty={:.6} @ {:.2} (notional={:.2}, equity={:.2})",
                        candle.symbol, qty, candle.close, notional, equity
                    );
                }
            }

            self.equity_curve
                .push((candle.timestamp, self.capital + unrealized_pnl));
        }

        // 强制平仓
        if let Some((side, entry_price, qty)) = position {
            if let Some(last_candle) = data.last() {
                self.force_close(
                    side,
                    entry_price,
                    qty,
                    last_candle.close,
                    &last_candle.symbol,
                    last_candle.timestamp,
                );
                if let Some(last) = self.equity_curve.last_mut() {
                    *last = (last_candle.timestamp, self.capital);
                } else {
                    self.equity_curve
                        .push((last_candle.timestamp, self.capital));
                }
            }
        }

        Ok(self.build_results())
    }

    fn charge_commission(&mut self, amount: f64) {
        self.capital -= amount;
        self.total_commission += amount;
    }

    /// 以 `mark` 价平掉全部持仓（含滑点与手续费）并记一笔成交。
    /// 强平与收盘强制平仓共用此路径，因此**强平也计入 trades.len()**，
    /// 手续费同样计入 total_commission，避免与现金账目脱节。
    fn force_close(
        &mut self,
        side: Side,
        entry_price: f64,
        qty: f64,
        mark: f64,
        symbol: &str,
        timestamp: DateTime<Utc>,
    ) {
        let close_side = if side == Side::Buy {
            Side::Sell
        } else {
            Side::Buy
        };
        let execution_price = self.apply_slippage(mark, close_side);
        let commission = execution_price * self.commission_rate * qty;
        let pnl = match side {
            Side::Buy => (execution_price - entry_price) * qty,
            Side::Sell => (entry_price - execution_price) * qty,
        };
        self.capital += pnl;
        self.charge_commission(commission);
        self.trades.push(BacktestTrade {
            timestamp,
            symbol: symbol.to_string(),
            side: close_side,
            price: execution_price,
            quantity: qty,
            pnl,
            commission,
        });
    }

    /// 构建市场快照
    fn build_snapshot(&self, candles: &[Candlestick]) -> MarketSnapshot {
        let mut snapshot = MarketSnapshot::default();

        if let Some(last) = candles.last() {
            let ob = OrderBook {
                symbol: last.symbol.clone(),
                market_id: 0,
                bids: vec![PriceLevel {
                    price: last.close * 0.999,
                    quantity: 1.0,
                }],
                asks: vec![PriceLevel {
                    price: last.close * 1.001,
                    quantity: 1.0,
                }],
                timestamp: last.timestamp,
            };
            snapshot.order_books.insert(last.symbol.clone(), ob);

            let candle_vec: Vec<Candlestick> = candles.to_vec();
            snapshot.candles.insert(last.symbol.clone(), candle_vec);
        }

        snapshot
    }

    /// 应用滑点
    fn apply_slippage(&self, price: f64, side: Side) -> f64 {
        match side {
            Side::Buy => price * (1.0 + self.slippage_rate),
            Side::Sell => price * (1.0 - self.slippage_rate),
        }
    }

    fn build_results(&self) -> BacktestResults {
        calculate_results(&ResultsCalcInput {
            initial_capital: self.initial_capital,
            capital: self.capital,
            trades: &self.trades,
            equity_curve: &self.equity_curve,
            total_commission: self.total_commission,
            historical_data: &self.historical_data,
            commission_rate: self.commission_rate,
            slippage_rate: self.slippage_rate,
            peak_notional: self.margin.peak_notional(),
            peak_leverage: self.margin.peak_leverage(),
            peak_position_grids: self.margin.peak_position_grids(),
            liq_count: self.margin.liq_count(),
            bars_over_soft_cap: self.margin.bars_over_soft_cap(),
            blocked_by_profitability: self.blocked_by_profitability,
        })
    }
}

/// 以 `mark` 价计算持仓浮动盈亏（空仓为 0）。
fn unrealized_of(position: Option<(Side, f64, f64)>, mark: f64) -> f64 {
    match position {
        Some((Side::Buy, entry, qty)) => (mark - entry) * qty,
        Some((Side::Sell, entry, qty)) => (entry - mark) * qty,
        None => 0.0,
    }
}

#[cfg(test)]
#[path = "engine_test_support.rs"]
mod engine_test_support;

#[cfg(test)]
#[path = "engine_accounting_tests.rs"]
mod engine_accounting_tests;

#[cfg(test)]
#[path = "engine_commission_tests.rs"]
mod engine_commission_tests;

#[cfg(test)]
#[path = "engine_report_tests.rs"]
mod engine_report_tests;

#[cfg(test)]
#[path = "engine_margin_tests.rs"]
mod engine_margin_tests;

#[cfg(test)]
#[path = "engine_profitability_tests.rs"]
mod engine_profitability_tests;
