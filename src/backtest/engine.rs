use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
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
    /// Taker fee rate used by marketable entries, risk exits, and forced closes.
    commission_rate: f64,
    /// Maker fee rate used only by resting post-only fills.
    maker_commission_rate: f64,
    /// Taker slippage rate. Resting maker fills execute at their quote price.
    slippage_rate: f64,
    trades: Vec<BacktestTrade>,
    equity_curve: Vec<(DateTime<Utc>, f64)>,
    total_commission: f64,
    total_adverse_selection: f64,
    margin: MarginTracker,
    /// 可选收益门槛：与实盘 `RiskManager::check_signal` 同口径，None = 历史行为（全放行）
    profitability: Option<ProfitabilityGuard>,
    /// 被收益门槛拦截的入场信号数（risk_reducing 永不拦截）
    blocked_by_profitability: usize,
    blocked_by_position_limit: usize,
    blocked_by_total_position_limit: usize,
    stop_loss_exits: usize,
    take_profit_exits: usize,
    position_stop_loss_pct: Option<f64>,
    position_take_profit_pct: Option<f64>,
    max_position_notional: Option<f64>,
    max_total_notional_pct: Option<f64>,
    /// Maker fill model: post-only quotes become active after the current bar.
    maker_fills: bool,
    /// Fraction of remaining resting quantity filled on each qualifying cross.
    maker_fill_ratio: f64,
    /// Required penetration beyond the quote before a maker fill is assumed.
    maker_min_penetration_bps: f64,
    /// Conservative capital penalty applied to each maker fill.
    maker_adverse_selection_rate: f64,
}

fn maker_order_key(signal: &TradeSignal) -> String {
    signal.client_id.clone().unwrap_or_else(|| {
        let side = match signal.side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        };
        format!("{}:{side}", signal.symbol)
    })
}

impl BacktestEngine {
    pub fn new(initial_capital: f64, historical_data: Vec<Candlestick>) -> Self {
        Self {
            initial_capital,
            capital: initial_capital,
            historical_data,
            commission_rate: 0.001, // 0.1% taker
            maker_commission_rate: 0.001,
            slippage_rate: 0.0005, // 0.05% taker
            trades: Vec::new(),
            equity_curve: Vec::new(),
            total_commission: 0.0,
            total_adverse_selection: 0.0,
            margin: MarginTracker::default(),
            profitability: None,
            blocked_by_profitability: 0,
            blocked_by_position_limit: 0,
            blocked_by_total_position_limit: 0,
            stop_loss_exits: 0,
            take_profit_exits: 0,
            position_stop_loss_pct: None,
            position_take_profit_pct: None,
            max_position_notional: None,
            max_total_notional_pct: None,
            maker_fills: false,
            maker_fill_ratio: 1.0,
            maker_min_penetration_bps: 0.0,
            maker_adverse_selection_rate: 0.0,
        }
    }

    /// 启用 maker 填充模型（配合 `--maker` 与 maker 费率配置使用）
    pub fn with_maker_fills(mut self, enabled: bool) -> Self {
        self.maker_fills = enabled;
        self
    }

    #[cfg(test)]
    /// Set one legacy fee rate for both maker and taker executions.
    pub fn with_commission(mut self, rate: f64) -> Self {
        self.commission_rate = rate;
        self.maker_commission_rate = rate;
        self
    }

    #[cfg(test)]
    /// 设置滑点
    pub fn with_slippage(mut self, rate: f64) -> Self {
        self.slippage_rate = rate;
        self
    }

    /// 当前手续费率（用于日志展示）
    pub fn commission_rate(&self) -> f64 {
        self.commission_rate
    }

    /// Configure separate maker and taker execution costs.
    pub fn with_execution_costs(
        mut self,
        maker_fee_rate: f64,
        taker_fee_rate: f64,
        taker_slippage_rate: f64,
    ) -> Result<Self> {
        if [maker_fee_rate, taker_fee_rate, taker_slippage_rate]
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0 || *value >= 1.0)
        {
            bail!("execution cost rates must be finite and in [0, 1)");
        }
        self.maker_commission_rate = maker_fee_rate;
        self.commission_rate = taker_fee_rate;
        self.slippage_rate = taker_slippage_rate;
        Ok(self)
    }

    /// Enable live-equivalent per-position stop-loss and take-profit exits.
    pub fn with_position_risk(mut self, stop_loss_pct: f64, take_profit_pct: f64) -> Result<Self> {
        if !stop_loss_pct.is_finite()
            || !take_profit_pct.is_finite()
            || !(0.0..1.0).contains(&stop_loss_pct)
            || !(0.0..1.0).contains(&take_profit_pct)
        {
            bail!("position stop/take percentages must be finite and in (0, 1)");
        }
        self.position_stop_loss_pct = Some(stop_loss_pct);
        self.position_take_profit_pct = Some(take_profit_pct);
        Ok(self)
    }

    /// Enforce the live projected worst-case account notional limit at order placement.
    pub fn with_max_position_notional(mut self, max_notional: f64) -> Result<Self> {
        if !max_notional.is_finite() || max_notional <= 0.0 {
            bail!("max position notional must be finite and positive");
        }
        self.max_position_notional = Some(max_notional);
        Ok(self)
    }

    pub fn with_max_total_notional_pct(mut self, max_notional_pct: f64) -> Result<Self> {
        if !max_notional_pct.is_finite() || max_notional_pct <= 0.0 {
            bail!("max total notional percentage must be finite and positive");
        }
        self.max_total_notional_pct = Some(max_notional_pct);
        Ok(self)
    }

    /// Configure deterministic conservative maker fill assumptions.
    pub fn with_conservative_maker_model(
        mut self,
        fill_ratio: f64,
        min_penetration_bps: f64,
        adverse_selection_bps: f64,
    ) -> Result<Self> {
        if !fill_ratio.is_finite() || !(0.0..=1.0).contains(&fill_ratio) || fill_ratio == 0.0 {
            bail!("maker fill ratio must be finite and in (0, 1]");
        }
        if !min_penetration_bps.is_finite()
            || min_penetration_bps < 0.0
            || !adverse_selection_bps.is_finite()
            || adverse_selection_bps < 0.0
        {
            bail!("maker penetration and adverse-selection bps must be finite and non-negative");
        }
        self.maker_fill_ratio = fill_ratio;
        self.maker_min_penetration_bps = min_penetration_bps;
        self.maker_adverse_selection_rate = adverse_selection_bps / 10_000.0;
        Ok(self)
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
        let mut market_candles: HashMap<String, Vec<Candlestick>> = HashMap::new();

        let data = self.historical_data.clone();
        // 多市场持仓表：symbol -> (side, entry_price, quantity)。
        // 单市场回测与多市场回测共用同一引擎，每个 symbol 独立仓位。
        let mut positions: HashMap<String, (Side, f64, f64)> = HashMap::new();
        // 各 symbol 最近一根K线的收盘价（多市场 mark-to-market 用）
        let mut last_close: HashMap<String, f64> = HashMap::new();
        // Stable client/order key -> resting maker signal. A quote created from
        // bar t can only be crossed by a later bar for the same symbol.
        let mut pending_maker_orders: HashMap<String, TradeSignal> = HashMap::new();

        // 预分配容量提升性能
        self.equity_curve.reserve(data.len());
        self.trades.reserve(data.len() / 10);

        for candle in &data {
            // Existing positions are checked before this bar's maker crosses. This avoids
            // pretending that a newly filled quote was already exposed to the bar's earlier
            // high/low path, whose intrabar ordering is unknowable from OHLC data.
            self.apply_position_risk_exit(candle, &mut positions, &mut pending_maker_orders);

            if self.maker_fills {
                let penetration = self.maker_min_penetration_bps / 10_000.0;
                let mut crossed_keys: Vec<String> = pending_maker_orders
                    .iter()
                    .filter(|(_, signal)| {
                        signal.symbol == candle.symbol
                            && match signal.side {
                                Side::Buy => candle.low <= signal.price * (1.0 - penetration),
                                Side::Sell => candle.high >= signal.price * (1.0 + penetration),
                            }
                    })
                    .map(|(key, _)| key.clone())
                    .collect();
                crossed_keys.sort_unstable();
                for key in crossed_keys {
                    if let Some(mut resting) = pending_maker_orders.remove(&key) {
                        let fill_quantity = resting.quantity * self.maker_fill_ratio;
                        let mut fill = resting.clone();
                        fill.quantity = fill_quantity;
                        if self.execute_signal(&fill, candle, &mut positions, true) {
                            resting.quantity -= fill_quantity;
                            if resting.quantity * resting.price > f64::EPSILON {
                                pending_maker_orders.insert(key, resting);
                            }
                        }
                    }
                }
            }

            let history = market_candles.entry(candle.symbol.clone()).or_default();
            history.push(candle.clone());
            if history.len() > 100 {
                history.remove(0);
            }
            let mut snapshot = self.build_snapshot(&market_candles);
            if std::env::var("BACKTEST_NO_CAP").is_err() {
                for (sym, (side, _entry, qty)) in &positions {
                    let signed = match side {
                        Side::Buy => *qty,
                        Side::Sell => -*qty,
                    };
                    snapshot.positions.insert(sym.clone(), signed);
                }
            }
            if self.maker_fills {
                snapshot.open_orders_authoritative = true;
                snapshot.open_orders = pending_maker_orders
                    .values()
                    .map(|signal| OpenOrderRef {
                        symbol: signal.symbol.clone(),
                        client_id: signal.client_id.clone(),
                        side: signal.side,
                        price: signal.price,
                        quantity: signal.quantity,
                        status: "OPEN".into(),
                    })
                    .collect();
            }

            if let Some(signals) = strategy.evaluate(&snapshot).await? {
                for signal in signals {
                    if signal.action == SignalAction::Cancel {
                        let key = maker_order_key(&signal);
                        pending_maker_orders.remove(&key);
                        continue;
                    }

                    if let Some(guard) = &self.profitability {
                        let economics = SignalEconomics::from_signal(
                            signal.expected_edge_bps,
                            signal.risk_reducing,
                            signal.post_only,
                        );
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

                    if !self.signal_within_exposure_limits(
                        &signal,
                        &positions,
                        &pending_maker_orders,
                        &last_close,
                        candle,
                    ) {
                        continue;
                    }

                    if self.maker_fills && signal.post_only {
                        pending_maker_orders.insert(maker_order_key(&signal), signal);
                    } else {
                        let _ = self.execute_signal(&signal, candle, &mut positions, false);
                    }
                }
            }

            last_close.insert(candle.symbol.clone(), candle.close);

            // ===== 线性保证金 v1：峰值风险 + 强平（口径见 backtest::margin） =====
            // 顺序是有意义的：先按本根K线成交后的持仓算 equity/notional，强平在
            // 写入权益曲线**之前**执行，否则被强平的那根K线会记下强平前的权益，
            // MaxDD 与 liq_count 会互相矛盾。多市场：按各 symbol 最近收盘价聚合。
            let mut unrealized_pnl = unrealized_total(&positions, &last_close);
            let total_notional: f64 = positions
                .iter()
                .filter_map(|(sym, (_side, _e, qty))| last_close.get(sym).map(|c| qty.abs() * c))
                .sum();
            let equity = self.capital + unrealized_pnl;
            if !positions.is_empty() && self.margin.observe_bar(total_notional, equity) {
                // 账户级强平：全部持仓
                let symbols: Vec<String> = positions.keys().cloned().collect();
                for sym in symbols {
                    if let Some((side, entry, qty)) = positions.remove(&sym) {
                        let mark = last_close.get(&sym).copied().unwrap_or(candle.close);
                        self.force_close(side, entry, qty, mark, &sym, candle.timestamp);
                    }
                }
                self.margin.record_liquidation();
                unrealized_pnl = 0.0;
                debug!(
                    "强平: 全部持仓 (notional={:.2}, equity={:.2})",
                    total_notional, equity
                );
            }

            self.equity_curve
                .push((candle.timestamp, self.capital + unrealized_pnl));
        }

        // 强制平仓
        if !positions.is_empty() {
            if let Some(last_candle) = data.last() {
                let symbols: Vec<String> = positions.keys().cloned().collect();
                for sym in symbols {
                    if let Some((side, entry_price, qty)) = positions.remove(&sym) {
                        let mark = last_close.get(&sym).copied().unwrap_or(last_candle.close);
                        self.force_close(side, entry_price, qty, mark, &sym, last_candle.timestamp);
                    }
                }
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

    fn apply_position_risk_exit(
        &mut self,
        candle: &Candlestick,
        positions: &mut HashMap<String, (Side, f64, f64)>,
        pending_maker_orders: &mut HashMap<String, TradeSignal>,
    ) {
        let Some((side, entry_price, quantity)) = positions.get(&candle.symbol).copied() else {
            return;
        };
        let stop = self.position_stop_loss_pct.and_then(|pct| {
            let trigger = match side {
                Side::Buy => entry_price * (1.0 - pct),
                Side::Sell => entry_price * (1.0 + pct),
            };
            match side {
                Side::Buy if candle.low <= trigger => Some(candle.open.min(trigger)),
                Side::Sell if candle.high >= trigger => Some(candle.open.max(trigger)),
                _ => None,
            }
        });
        let take = self.position_take_profit_pct.and_then(|pct| {
            let trigger = match side {
                Side::Buy => entry_price * (1.0 + pct),
                Side::Sell => entry_price * (1.0 - pct),
            };
            match side {
                Side::Buy if candle.high >= trigger => Some(candle.open.max(trigger)),
                Side::Sell if candle.low <= trigger => Some(candle.open.min(trigger)),
                _ => None,
            }
        });

        // OHLC does not reveal whether stop or take was touched first. Prefer the
        // adverse path when both occur so validation cannot gain from unknowable ordering.
        let Some((mark, stop_loss)) = stop
            .map(|mark| (mark, true))
            .or_else(|| take.map(|mark| (mark, false)))
        else {
            return;
        };

        positions.remove(&candle.symbol);
        pending_maker_orders.retain(|_, signal| signal.symbol != candle.symbol);
        self.force_close(
            side,
            entry_price,
            quantity,
            mark,
            &candle.symbol,
            candle.timestamp,
        );
        if stop_loss {
            self.stop_loss_exits += 1;
        } else {
            self.take_profit_exits += 1;
        }
    }

    fn signal_within_exposure_limits(
        &mut self,
        signal: &TradeSignal,
        positions: &HashMap<String, (Side, f64, f64)>,
        pending_orders: &HashMap<String, TradeSignal>,
        marks: &HashMap<String, f64>,
        candle: &Candlestick,
    ) -> bool {
        if signal.risk_reducing {
            return true;
        }
        if self.max_position_notional.is_none() && self.max_total_notional_pct.is_none() {
            return true;
        }

        let mut by_symbol: HashMap<String, (f64, f64, f64)> = HashMap::new();
        for (symbol, (side, entry, quantity)) in positions {
            let mark = if symbol == &candle.symbol {
                candle.close
            } else {
                marks.get(symbol).copied().unwrap_or(*entry)
            };
            let signed_notional =
                quantity.abs() * mark * if *side == Side::Buy { 1.0 } else { -1.0 };
            by_symbol.entry(symbol.clone()).or_default().0 = signed_notional;
        }
        for order in pending_orders.values() {
            let entry = by_symbol.entry(order.symbol.clone()).or_default();
            let notional = (order.price * order.quantity).abs();
            if order.side == Side::Buy {
                entry.1 += notional;
            } else {
                entry.2 += notional;
            }
        }
        let proposed = by_symbol.entry(signal.symbol.clone()).or_default();
        let proposed_notional = (signal.price * signal.quantity).abs();
        if signal.side == Side::Buy {
            proposed.1 += proposed_notional;
        } else {
            proposed.2 += proposed_notional;
        }

        let symbol_worst = crate::risk::risk_manager::worst_case_symbol_notional(
            proposed.0, proposed.1, proposed.2,
        );
        if self
            .max_position_notional
            .is_some_and(|limit| symbol_worst > limit)
        {
            self.blocked_by_position_limit += 1;
            debug!(
                "单市场最坏方向敞口拦截(回测): {} projected={:.2}",
                signal.symbol, symbol_worst
            );
            return false;
        }

        if let Some(limit_pct) = self.max_total_notional_pct {
            let total_worst: f64 = by_symbol
                .values()
                .map(|(position, buys, sells)| {
                    crate::risk::risk_manager::worst_case_symbol_notional(*position, *buys, *sells)
                })
                .sum();
            let unrealized: f64 = positions
                .iter()
                .map(|(symbol, (side, entry, quantity))| {
                    let mark = if symbol == &candle.symbol {
                        candle.close
                    } else {
                        marks.get(symbol).copied().unwrap_or(*entry)
                    };
                    match side {
                        Side::Buy => (mark - entry) * quantity,
                        Side::Sell => (entry - mark) * quantity,
                    }
                })
                .sum();
            let max_total = (self.capital + unrealized).max(0.0) * limit_pct;
            if total_worst > max_total {
                self.blocked_by_total_position_limit += 1;
                debug!(
                    "账户最坏方向敞口拦截(回测): projected={:.2}, limit={:.2}",
                    total_worst, max_total
                );
                return false;
            }
        }
        true
    }

    fn execute_signal(
        &mut self,
        signal: &TradeSignal,
        candle: &Candlestick,
        positions: &mut HashMap<String, (Side, f64, f64)>,
        maker_execution: bool,
    ) -> bool {
        let execution_price = if maker_execution {
            signal.price
        } else {
            self.apply_slippage(signal.price, signal.side)
        };
        let fee_rate = if maker_execution {
            self.maker_commission_rate
        } else {
            self.commission_rate
        };
        let commission_per_qty = execution_price * fee_rate;
        let adverse_per_qty = if maker_execution {
            execution_price * self.maker_adverse_selection_rate
        } else {
            0.0
        };
        let symbol = signal.symbol.clone();

        let executed_quantity = match positions.get(&symbol).copied() {
            Some((pos_side, entry_price, pos_qty)) if pos_side == signal.side => {
                let add_qty = signal.quantity;
                let add_commission = commission_per_qty * add_qty;
                let add_adverse = adverse_per_qty * add_qty;
                let cost = execution_price * add_qty;
                if cost + add_commission + add_adverse > self.capital {
                    0.0
                } else {
                    let new_qty = pos_qty + add_qty;
                    let weighted_entry =
                        ((entry_price * pos_qty) + (execution_price * add_qty)) / new_qty;
                    self.charge_commission(add_commission);
                    positions.insert(symbol.clone(), (pos_side, weighted_entry, new_qty));
                    debug!(
                        "加仓: {:?} {} @ {:.2}, qty {:.6} -> {:.6}",
                        signal.side, symbol, execution_price, pos_qty, new_qty
                    );
                    add_qty
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
                    symbol: symbol.clone(),
                    side: signal.side,
                    price: execution_price,
                    quantity: close_qty,
                    pnl,
                    commission: close_commission,
                });

                let remaining_pos_qty = pos_qty - close_qty;
                let remaining_signal_qty = signal.quantity - close_qty;
                if remaining_pos_qty > f64::EPSILON {
                    positions.insert(symbol.clone(), (pos_side, entry_price, remaining_pos_qty));
                } else {
                    positions.remove(&symbol);
                }
                let mut opened_qty = 0.0;
                if remaining_signal_qty > f64::EPSILON {
                    let open_commission = commission_per_qty * remaining_signal_qty;
                    let open_adverse = adverse_per_qty * remaining_signal_qty;
                    let cost = execution_price * remaining_signal_qty;
                    if cost + open_commission + open_adverse <= self.capital {
                        self.charge_commission(open_commission);
                        positions
                            .insert(symbol, (signal.side, execution_price, remaining_signal_qty));
                        opened_qty = remaining_signal_qty;
                    }
                }
                close_qty + opened_qty
            }
            None => {
                let commission = commission_per_qty * signal.quantity;
                let adverse = adverse_per_qty * signal.quantity;
                let cost = execution_price * signal.quantity;
                if cost + commission + adverse <= self.capital {
                    self.charge_commission(commission);
                    positions.insert(
                        symbol.clone(),
                        (signal.side, execution_price, signal.quantity),
                    );
                    debug!(
                        "开仓: {:?} {} @ {:.2}",
                        signal.side, symbol, execution_price
                    );
                    signal.quantity
                } else {
                    debug!(
                        "开仓跳过(资金不足): {} {:?} cost={:.2} capital={:.2}",
                        symbol,
                        signal.side,
                        cost + commission + adverse,
                        self.capital
                    );
                    0.0
                }
            }
        };

        if executed_quantity > 0.0 && maker_execution {
            self.charge_adverse_selection(adverse_per_qty * executed_quantity);
        }
        executed_quantity > 0.0
    }

    fn charge_commission(&mut self, amount: f64) {
        self.capital -= amount;
        self.total_commission += amount;
    }

    fn charge_adverse_selection(&mut self, amount: f64) {
        self.capital -= amount;
        self.total_adverse_selection += amount;
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

    /// Build an event-time snapshot containing the latest known state for
    /// every market, plus an independent rolling candle history per symbol.
    fn build_snapshot(&self, market_candles: &HashMap<String, Vec<Candlestick>>) -> MarketSnapshot {
        let mut snapshot = MarketSnapshot::default();
        for (symbol, candles) in market_candles {
            let Some(last) = candles.last() else {
                continue;
            };
            snapshot.order_books.insert(
                symbol.clone(),
                OrderBook {
                    symbol: symbol.clone(),
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
                },
            );
            snapshot.candles.insert(symbol.clone(), candles.clone());
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
            total_adverse_selection: self.total_adverse_selection,
            historical_data: &self.historical_data,
            commission_rate: self.commission_rate,
            slippage_rate: self.slippage_rate,
            peak_notional: self.margin.peak_notional(),
            peak_leverage: self.margin.peak_leverage(),
            peak_position_grids: self.margin.peak_position_grids(),
            liq_count: self.margin.liq_count(),
            bars_over_soft_cap: self.margin.bars_over_soft_cap(),
            blocked_by_profitability: self.blocked_by_profitability,
            blocked_by_position_limit: self.blocked_by_position_limit,
            blocked_by_total_position_limit: self.blocked_by_total_position_limit,
            stop_loss_exits: self.stop_loss_exits,
            take_profit_exits: self.take_profit_exits,
        })
    }
}

/// 以 `mark` 价计算持仓浮动盈亏（空仓为 0）。
/// 多市场未实现盈亏：按各 symbol 最近收盘价聚合。
fn unrealized_total(
    positions: &HashMap<String, (Side, f64, f64)>,
    marks: &HashMap<String, f64>,
) -> f64 {
    positions
        .iter()
        .filter_map(|(sym, &(side, entry, qty))| {
            marks.get(sym).map(|&mark| match side {
                Side::Buy => (mark - entry) * qty,
                Side::Sell => (entry - mark) * qty,
            })
        })
        .sum()
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

#[cfg(test)]
#[path = "engine_maker_tests.rs"]
mod engine_maker_tests;

#[cfg(test)]
#[path = "engine_multi_market_tests.rs"]
mod engine_multi_market_tests;

#[cfg(test)]
#[path = "engine_live_parity_tests.rs"]
mod engine_live_parity_tests;
