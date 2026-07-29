//! 线性保证金模型 v1：峰值风险指标 + 强平判定。
//!
//! 只做回测内部的风险刻画，**不接入实盘 `RiskManager`**，也不实现交易所真实的
//! 维持保证金阶梯 / 资金费 / 部分成交。目的仅是让「不封顶更赚钱」这类结论
//! 无法再靠现金充足假设蒙混过关——扛更大的仓位必须在指标上付出代价。
//!
//! 口径（每根K线成交结算后，用该K线收盘价 mark）：
//! - `equity   = capital + unrealized_pnl`
//! - `notional = |qty| * mark`  （毛名义敞口，收盘价计价，不是成本价）
//! - `leverage = notional / max(equity, 1e-9)`
//! - `IM       = notional / max_leverage`（默认 max_leverage = 3.0）
//! - 强平当且仅当 `free_margin = equity - IM <= 0`

/// 默认模拟杠杆上限，与 dashboard `leverage_limit` 默认值一致。
pub const DEFAULT_MAX_LEVERAGE: f64 = 3.0;

/// 峰值与强平追踪器（引擎每根K线调用一次）。
#[derive(Debug, Clone)]
pub struct MarginTracker {
    max_leverage: f64,
    /// 单格名义金额（= investment_per_grid）。未设置时不统计网格数指标。
    grid_unit_notional: Option<f64>,
    /// 软上限网格数。未设置时不统计 `bars_over_soft_cap`。
    soft_cap_grids: Option<f64>,
    peak_notional: f64,
    peak_leverage: f64,
    peak_position_grids: f64,
    liq_count: usize,
    bars_over_soft_cap: usize,
}

impl Default for MarginTracker {
    fn default() -> Self {
        Self {
            max_leverage: DEFAULT_MAX_LEVERAGE,
            grid_unit_notional: None,
            soft_cap_grids: None,
            peak_notional: 0.0,
            peak_leverage: 0.0,
            peak_position_grids: 0.0,
            liq_count: 0,
            bars_over_soft_cap: 0,
        }
    }
}

impl MarginTracker {
    pub fn set_max_leverage(&mut self, max_leverage: f64) {
        self.max_leverage = max_leverage.max(1e-9);
    }

    /// 单格名义金额。**只能显式设置**——刻意不从首笔信号反推：
    /// soft 模式下首笔信号可能已被缩放，反推会把 peak_position_grids 放大 1/scale，
    /// 而这个指标正是晋升规则与 canary 中止阈值所依赖的。
    pub fn set_grid_unit_notional(&mut self, unit: f64) {
        self.grid_unit_notional = (unit > 0.0).then_some(unit);
    }

    pub fn set_soft_cap_grids(&mut self, soft_cap: f64) {
        self.soft_cap_grids = (soft_cap > 0.0).then_some(soft_cap);
    }

    /// 观察一根K线（成交结算后）。返回 true 表示必须强平。
    ///
    /// `notional` 为 0（空仓）时只是空转，不更新峰值也不会触发强平。
    pub fn observe_bar(&mut self, notional: f64, equity: f64) -> bool {
        if notional <= 0.0 {
            return false;
        }

        self.peak_notional = self.peak_notional.max(notional);
        self.peak_leverage = self.peak_leverage.max(notional / equity.max(1e-9));

        if let Some(unit) = self.grid_unit_notional {
            let grids = notional / unit;
            self.peak_position_grids = self.peak_position_grids.max(grids);
            if let Some(soft) = self.soft_cap_grids {
                if grids >= soft {
                    self.bars_over_soft_cap += 1;
                }
            }
        }

        let initial_margin = notional / self.max_leverage;
        equity - initial_margin <= 0.0
    }

    /// 引擎完成强制平仓后调用。
    pub fn record_liquidation(&mut self) {
        self.liq_count += 1;
    }

    pub fn peak_notional(&self) -> f64 {
        self.peak_notional
    }

    pub fn peak_leverage(&self) -> f64 {
        self.peak_leverage
    }

    pub fn peak_position_grids(&self) -> f64 {
        self.peak_position_grids
    }

    pub fn liq_count(&self) -> usize {
        self.liq_count
    }

    pub fn bars_over_soft_cap(&self) -> usize {
        self.bars_over_soft_cap
    }
}

#[cfg(test)]
#[path = "margin_tests.rs"]
mod margin_tests;
