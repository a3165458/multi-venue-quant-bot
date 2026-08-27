use anyhow::Result;
use chrono::Utc;
use config::Config;
use tracing::{info, warn};

use crate::lighter::types::{Position, Side, SignalAction, TradeSignal};
use crate::risk::profitability::{ProfitabilityGuard, SignalEconomics};

/// Current exchange exposure used to validate a projected order.
#[derive(Debug, Clone, Copy, Default)]
pub struct RiskExposure {
    /// Signed position notional for the signal's symbol.
    pub symbol_position_notional: f64,
    /// Existing resting buy notional for the signal's symbol.
    pub symbol_buy_open_notional: f64,
    /// Existing resting sell notional for the signal's symbol.
    pub symbol_sell_open_notional: f64,
    /// Sum across symbols of the larger directional outcome if all buys or all sells fill.
    pub total_worst_case_notional: f64,
}

pub(crate) fn worst_case_symbol_notional(position: f64, buy_orders: f64, sell_orders: f64) -> f64 {
    (position + buy_orders)
        .abs()
        .max((position - sell_orders).abs())
}

/// 风险管理器
pub struct RiskManager {
    max_drawdown_pct: f64,
    daily_loss_limit_pct: f64,
    max_leverage: f64,
    max_position_size: f64,
    max_single_trade_pct: f64,
    max_total_position_pct: f64,
    current_daily_pnl: f64,
    current_equity: f64,
    initial_equity: f64,
    equity_initialized: bool,
    /// Per-position stop-loss percentage (e.g., 0.05 = 5%)
    position_stop_loss_pct: f64,
    /// Per-position take-profit percentage (e.g., 0.08 = 8%)
    position_take_profit_pct: f64,
    /// Whether emergency close has been triggered (prevents re-entry)
    emergency_triggered: bool,
    /// Track which UTC day the emergency was triggered
    emergency_day: Option<u32>,
    profitability_guard: ProfitabilityGuard,
}

impl RiskManager {
    /// 从配置文件创建风险管理器
    pub fn new(settings: &Config) -> Result<Self> {
        let max_drawdown_pct = settings
            .get_float("risk.stop_loss.max_drawdown_percent")
            .unwrap_or(10.0)
            / 100.0;

        let daily_loss_limit_pct = settings
            .get_float("risk.stop_loss.daily_loss_limit_percent")
            .unwrap_or(5.0)
            / 100.0;

        let max_leverage = settings
            .get_float("risk.position_limit.max_leverage")
            .unwrap_or(3.0);

        let max_position_size = settings
            .get_float("risk.position_limit.max_position_size")
            .unwrap_or(10000.0);

        let max_single_trade_pct = settings
            .get_float("trading.position.max_single_trade_percent")
            .unwrap_or(10.0)
            / 100.0;

        let max_total_position_pct = settings
            .get_float("trading.position.max_total_position_percent")
            .unwrap_or(50.0)
            / 100.0;

        let position_stop_loss_pct = settings
            .get_float("risk.stop_loss.position_stop_loss_percent")
            .unwrap_or(5.0)
            / 100.0;

        let position_take_profit_pct = settings
            .get_float("risk.stop_loss.position_take_profit_percent")
            .unwrap_or(8.0)
            / 100.0;
        let profitability_guard = ProfitabilityGuard::from_config(settings)?;

        info!("风控初始化: 最大回撤 {:.1}%, 日亏损限制 {:.1}%, 最大杠杆 {:.0}x, 止损 {:.1}%, 止盈 {:.1}%",
            max_drawdown_pct * 100.0,
            daily_loss_limit_pct * 100.0,
            max_leverage,
            position_stop_loss_pct * 100.0,
            position_take_profit_pct * 100.0,
        );

        Ok(Self {
            max_drawdown_pct,
            daily_loss_limit_pct,
            max_leverage,
            max_position_size,
            max_single_trade_pct,
            max_total_position_pct,
            current_daily_pnl: 0.0,
            current_equity: 10000.0,
            initial_equity: 10000.0,
            equity_initialized: false,
            position_stop_loss_pct,
            position_take_profit_pct,
            emergency_triggered: false,
            emergency_day: None,
            profitability_guard,
        })
    }

    pub fn override_profitability_schedule(
        &mut self,
        entry_fee_bps: f64,
        exit_fee_bps: f64,
        adverse_selection_bps: f64,
    ) -> Result<()> {
        self.profitability_guard = self.profitability_guard.clone().with_schedule(
            entry_fee_bps,
            exit_fee_bps,
            adverse_selection_bps,
        )?;
        Ok(())
    }

    /// 更新当前权益
    #[allow(dead_code)]
    pub fn update_equity(&mut self, equity: f64) {
        self.current_equity = equity;
        if !self.equity_initialized {
            self.initial_equity = equity;
            self.equity_initialized = true;
        }
    }

    /// Restore the live-account drawdown baseline after a process restart.
    pub fn restore_equity_baseline(&mut self, initial_equity: f64, current_equity: f64) {
        if initial_equity.is_finite() && initial_equity > 0.0 {
            self.initial_equity = initial_equity;
            self.equity_initialized = true;
        }
        if current_equity.is_finite() && current_equity > 0.0 {
            self.current_equity = current_equity;
        }
    }

    /// 更新日PnL
    #[allow(dead_code)]
    pub fn update_daily_pnl(&mut self, pnl: f64) {
        self.current_daily_pnl = pnl;

        // Auto-reset emergency on new UTC day
        if self.emergency_triggered {
            let today = (Utc::now().timestamp() / 86400) as u32;
            if let Some(trigger_day) = self.emergency_day {
                if today > trigger_day {
                    info!("🔄 新的一天 — 重置紧急模式，恢复交易");
                    self.emergency_triggered = false;
                    self.emergency_day = None;
                    self.initial_equity = self.current_equity; // reset baseline
                }
            }
        }
    }

    /// 重置每日PnL（每日开盘时调用）
    #[allow(dead_code)]
    pub fn reset_daily(&mut self) {
        self.current_daily_pnl = 0.0;
    }

    /// Check a signal without an exchange exposure snapshot.
    ///
    /// Live trading paths should use [`Self::check_signal_with_exposure`].
    pub async fn check_signal(&self, signal: &TradeSignal) -> Result<bool> {
        self.check_signal_with_exposure(signal, RiskExposure::default())
            .await
    }

    /// Validate the projected post-order symbol and account exposure.
    pub async fn check_signal_with_exposure(
        &self,
        signal: &TradeSignal,
        exposure: RiskExposure,
    ) -> Result<bool> {
        if signal.action == SignalAction::Cancel {
            return Ok(true);
        }

        let trade_value = signal.price * signal.quantity;
        if !signal.price.is_finite()
            || !signal.quantity.is_finite()
            || signal.price <= 0.0
            || signal.quantity <= 0.0
            || !trade_value.is_finite()
        {
            warn!("❌ 风控拒绝: 非法价格或数量");
            return Ok(false);
        }

        let economics = SignalEconomics::from_signal(
            signal.expected_edge_bps,
            signal.risk_reducing,
            signal.post_only,
        );
        let profitability = self.profitability_guard.evaluate(economics);
        if !profitability.allowed {
            warn!(
                "❌ 收益门槛拒绝: {} {:?}, reason={}, gross={:?}bps, cost={:.2}bps, net={:?}bps, required>{:.2}bps",
                signal.symbol,
                signal.side,
                profitability.reason,
                profitability.expected_edge_bps,
                profitability.total_cost_bps,
                profitability.net_edge_bps,
                profitability.required_net_edge_bps,
            );
            return Ok(false);
        }

        // Exit orders must remain available after loss or emergency gates fire.
        if signal.risk_reducing {
            return Ok(true);
        }

        if self.emergency_triggered {
            warn!("❌ 风控拒绝: 紧急平仓已触发，禁止新增风险");
            return Ok(false);
        }

        if !self.initial_equity.is_finite() || self.initial_equity <= 0.0 {
            warn!("❌ 风控拒绝: 账户权益未初始化或为 0");
            return Ok(false);
        }

        let daily_loss = -self.current_daily_pnl / self.initial_equity;
        if daily_loss >= self.daily_loss_limit_pct {
            warn!(
                "❌ 风控拒绝: 已达到每日亏损限制 ({:.2}%)",
                daily_loss * 100.0
            );
            return Ok(false);
        }

        let drawdown = (self.initial_equity - self.current_equity) / self.initial_equity;
        if drawdown >= self.max_drawdown_pct {
            warn!("❌ 风控拒绝: 已超过最大回撤限制 ({:.2}%)", drawdown * 100.0);
            return Ok(false);
        }

        let max_trade_value = self.current_equity * self.max_single_trade_pct;
        if trade_value > max_trade_value {
            warn!(
                "❌ 风控拒绝: 交易金额 ${:.2} 超过单笔限制 ${:.2}",
                trade_value, max_trade_value
            );
            return Ok(false);
        }

        let current_symbol_worst = worst_case_symbol_notional(
            exposure.symbol_position_notional,
            exposure.symbol_buy_open_notional,
            exposure.symbol_sell_open_notional,
        );
        let (projected_buy_orders, projected_sell_orders) = match signal.side {
            Side::Buy => (
                exposure.symbol_buy_open_notional + trade_value,
                exposure.symbol_sell_open_notional,
            ),
            Side::Sell => (
                exposure.symbol_buy_open_notional,
                exposure.symbol_sell_open_notional + trade_value,
            ),
        };
        let projected_symbol = worst_case_symbol_notional(
            exposure.symbol_position_notional,
            projected_buy_orders,
            projected_sell_orders,
        );
        if projected_symbol > self.max_position_size {
            warn!(
                "❌ 风控拒绝: {} 最坏方向敞口 ${:.2} 超过单市场限制 ${:.2}",
                signal.symbol, projected_symbol, self.max_position_size
            );
            return Ok(false);
        }

        let total_cap_pct = self.max_total_position_pct.max(0.0);
        let leverage_cap = self.max_leverage.max(0.0);
        let max_total_notional = self.current_equity * total_cap_pct.min(leverage_cap);
        let projected_total =
            (exposure.total_worst_case_notional - current_symbol_worst).max(0.0) + projected_symbol;
        if projected_total > max_total_notional {
            warn!(
                "❌ 风控拒绝: 预计最坏方向总敞口 ${:.2} 超过账户限制 ${:.2}",
                projected_total, max_total_notional
            );
            return Ok(false);
        }

        Ok(true)
    }

    /// 检查是否需要紧急平仓
    pub fn should_emergency_close(&self) -> bool {
        if !self.initial_equity.is_finite() || self.initial_equity <= 0.0 {
            return false;
        }
        let drawdown = (self.initial_equity - self.current_equity) / self.initial_equity;

        // 超过最大回撤的1.5倍时紧急平仓
        if drawdown >= self.max_drawdown_pct * 1.5 {
            warn!("🚨 紧急平仓触发! 回撤 {:.2}%", drawdown * 100.0);
            return true;
        }

        // 日内亏损超过限制的1.5倍
        let daily_loss = -self.current_daily_pnl / self.initial_equity;
        if daily_loss >= self.daily_loss_limit_pct * 1.5 {
            warn!("🚨 紧急平仓触发! 日内亏损 {:.2}%", daily_loss * 100.0);
            return true;
        }

        false
    }

    /// 标记紧急平仓已触发
    pub fn set_emergency_triggered(&mut self) {
        self.emergency_triggered = true;
        self.emergency_day = Some((Utc::now().timestamp() / 86400) as u32);
    }

    /// 检查是否已触发紧急平仓
    pub fn is_emergency_triggered(&self) -> bool {
        self.emergency_triggered
    }

    /// 检查持仓是否需要止损或止盈平仓
    /// 返回需要平仓的持仓列表，每项包含 (symbol, market_id_hint, side_to_close, size, reason)
    pub fn check_position_stop_loss_take_profit(
        &self,
        positions: &[Position],
        current_prices: &std::collections::HashMap<String, f64>,
    ) -> Vec<PositionCloseSignal> {
        let mut signals = Vec::new();

        for pos in positions {
            if pos.size.abs() < 1e-10 {
                continue;
            }

            let current_price = match current_prices.get(&pos.symbol) {
                Some(&p) if p > 0.0 => p,
                _ => continue,
            };

            let pnl_pct = match pos.side {
                Side::Buy => (current_price - pos.entry_price) / pos.entry_price,
                Side::Sell => (pos.entry_price - current_price) / pos.entry_price,
            };

            // Stop-loss: close if loss exceeds threshold
            if pnl_pct <= -self.position_stop_loss_pct {
                warn!(
                    "🛑 止损触发: {} {:?} entry={:.2} now={:.2} pnl={:.2}%",
                    pos.symbol,
                    pos.side,
                    pos.entry_price,
                    current_price,
                    pnl_pct * 100.0
                );
                signals.push(PositionCloseSignal {
                    symbol: pos.symbol.clone(),
                    side_to_close: match pos.side {
                        Side::Buy => Side::Sell,
                        Side::Sell => Side::Buy,
                    },
                    size: pos.size.abs(),
                    entry_price: pos.entry_price,
                    current_price,
                    pnl_pct,
                    reason: format!(
                        "止损: {:.2}% (阈值 -{:.1}%)",
                        pnl_pct * 100.0,
                        self.position_stop_loss_pct * 100.0
                    ),
                });
            }

            // Take-profit: close if profit exceeds threshold
            if pnl_pct >= self.position_take_profit_pct {
                info!(
                    "🎯 止盈触发: {} {:?} entry={:.2} now={:.2} pnl=+{:.2}%",
                    pos.symbol,
                    pos.side,
                    pos.entry_price,
                    current_price,
                    pnl_pct * 100.0
                );
                signals.push(PositionCloseSignal {
                    symbol: pos.symbol.clone(),
                    side_to_close: match pos.side {
                        Side::Buy => Side::Sell,
                        Side::Sell => Side::Buy,
                    },
                    size: pos.size.abs(),
                    entry_price: pos.entry_price,
                    current_price,
                    pnl_pct,
                    reason: format!(
                        "止盈: +{:.2}% (阈值 +{:.1}%)",
                        pnl_pct * 100.0,
                        self.position_take_profit_pct * 100.0
                    ),
                });
            }
        }

        signals
    }

    /// Update risk parameters at runtime from dashboard
    pub fn update_params(
        &mut self,
        max_drawdown_pct: Option<f64>,
        daily_loss_limit_pct: Option<f64>,
        max_leverage: Option<f64>,
        position_stop_loss_pct: Option<f64>,
        position_take_profit_pct: Option<f64>,
    ) {
        if let Some(v) = max_drawdown_pct {
            self.max_drawdown_pct = v / 100.0;
        }
        if let Some(v) = daily_loss_limit_pct {
            self.daily_loss_limit_pct = v / 100.0;
        }
        if let Some(v) = max_leverage {
            self.max_leverage = v;
        }
        if let Some(v) = position_stop_loss_pct {
            self.position_stop_loss_pct = v / 100.0;
        }
        if let Some(v) = position_take_profit_pct {
            self.position_take_profit_pct = v / 100.0;
        }
        info!("🔧 Risk params updated: drawdown={:.1}%, daily_loss={:.1}%, leverage={:.0}x, sl={:.1}%, tp={:.1}%",
            self.max_drawdown_pct * 100.0, self.daily_loss_limit_pct * 100.0, self.max_leverage,
            self.position_stop_loss_pct * 100.0, self.position_take_profit_pct * 100.0);
    }

    /// Get current risk config as a serializable map
    pub fn get_config(&self) -> serde_json::Value {
        serde_json::json!({
            "max_drawdown_pct": (self.max_drawdown_pct * 100.0),
            "daily_loss_limit_pct": (self.daily_loss_limit_pct * 100.0),
            "max_leverage": self.max_leverage,
            "position_stop_loss_pct": (self.position_stop_loss_pct * 100.0),
            "position_take_profit_pct": (self.position_take_profit_pct * 100.0),
        })
    }

    /// Get current max leverage setting
    #[allow(dead_code)]
    pub fn max_leverage(&self) -> f64 {
        self.max_leverage
    }

    /// 获取当前风控状态
    pub fn status(&self) -> RiskStatus {
        let baseline = if self.initial_equity.is_finite() && self.initial_equity > 0.0 {
            self.initial_equity
        } else {
            1.0
        };
        let drawdown = (self.initial_equity - self.current_equity) / baseline;
        let daily_loss = -self.current_daily_pnl / baseline;

        RiskStatus {
            current_equity: self.current_equity,
            drawdown_pct: drawdown * 100.0,
            daily_loss_pct: daily_loss * 100.0,
            max_drawdown_limit: self.max_drawdown_pct * 100.0,
            daily_loss_limit: self.daily_loss_limit_pct * 100.0,
            position_stop_loss_pct: self.position_stop_loss_pct * 100.0,
            position_take_profit_pct: self.position_take_profit_pct * 100.0,
            is_healthy: drawdown < self.max_drawdown_pct
                && daily_loss < self.daily_loss_limit_pct
                && !self.emergency_triggered,
            emergency_triggered: self.emergency_triggered,
        }
    }
}

/// 持仓平仓信号
#[derive(Debug, Clone)]
pub struct PositionCloseSignal {
    pub symbol: String,
    pub side_to_close: Side,
    pub size: f64,
    pub entry_price: f64,
    pub current_price: f64,
    #[allow(dead_code)]
    pub pnl_pct: f64,
    pub reason: String,
}

/// 风控状态
#[derive(Debug, Clone, serde::Serialize)]
pub struct RiskStatus {
    pub current_equity: f64,
    pub drawdown_pct: f64,
    pub daily_loss_pct: f64,
    pub max_drawdown_limit: f64,
    pub daily_loss_limit: f64,
    pub position_stop_loss_pct: f64,
    pub position_take_profit_pct: f64,
    pub is_healthy: bool,
    pub emergency_triggered: bool,
}
