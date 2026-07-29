use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::debug;

use super::Strategy;
use crate::lighter::types::*;

#[path = "trend_ema.rs"]
mod trend_ema;
use trend_ema::ema_last_two;

/// 趋势跟踪策略
///
/// 使用快/慢 EMA 交叉判断趋势方向，带最小分离度过滤减少震荡市假信号。
/// 每个市场独立维护仓位状态，支持止损、止盈和移动止损（从最优价回撤触发）。
pub struct TrendStrategy {
    fast_period: usize,
    slow_period: usize,
    stop_loss_pct: f64,
    take_profit_pct: f64,
    /// 移动止损：盈利超过该值后启动，从最优价回撤该值即离场（0 = 关闭）
    trailing_stop_pct: f64,
    /// 每次开仓的名义金额（USD）
    notional_per_trade: f64,
    /// 交叉时快慢线最小分离度（|fast-slow|/slow），过滤噪音交叉
    min_separation: f64,
    states: Mutex<HashMap<String, TrendState>>,
}

#[derive(Debug, Clone, Copy)]
struct PositionState {
    side: Side,
    entry_price: f64,
    quantity: f64,
    /// 开仓以来对仓位最有利的价格（多头最高价 / 空头最低价）
    best_price: f64,
}

#[derive(Default)]
struct TrendState {
    position: Option<PositionState>,
    pending_side: Option<Side>,
}

impl TrendStrategy {
    #[allow(dead_code)]
    pub fn new(
        fast_period: usize,
        slow_period: usize,
        stop_loss_pct: f64,
        take_profit_pct: f64,
    ) -> Self {
        Self::with_options(
            fast_period,
            slow_period,
            stop_loss_pct,
            take_profit_pct,
            0.0,
            1000.0,
        )
    }

    pub fn with_options(
        fast_period: usize,
        slow_period: usize,
        stop_loss_pct: f64,
        take_profit_pct: f64,
        trailing_stop_pct: f64,
        notional_per_trade: f64,
    ) -> Self {
        Self {
            fast_period,
            slow_period,
            stop_loss_pct,
            take_profit_pct,
            trailing_stop_pct,
            notional_per_trade,
            min_separation: 0.0005, // 0.05%
            states: Mutex::new(HashMap::new()),
        }
    }

    /// 检查持仓离场条件，返回 (原因, 离场方向)
    fn check_exit(&self, pos: &PositionState, price: f64) -> Option<String> {
        let pnl_pct = match pos.side {
            Side::Buy => (price - pos.entry_price) / pos.entry_price,
            Side::Sell => (pos.entry_price - price) / pos.entry_price,
        };

        if pnl_pct <= -self.stop_loss_pct {
            return Some(format!("止损: PnL {:.2}%", pnl_pct * 100.0));
        }
        if pnl_pct >= self.take_profit_pct {
            return Some(format!("止盈: PnL {:.2}%", pnl_pct * 100.0));
        }

        // 移动止损：最优价盈利超过 trailing 后，从最优价回撤 trailing 即离场
        if self.trailing_stop_pct > 0.0 {
            let best_pnl_pct = match pos.side {
                Side::Buy => (pos.best_price - pos.entry_price) / pos.entry_price,
                Side::Sell => (pos.entry_price - pos.best_price) / pos.entry_price,
            };
            if best_pnl_pct >= self.trailing_stop_pct {
                let retrace = match pos.side {
                    Side::Buy => (pos.best_price - price) / pos.best_price,
                    Side::Sell => (price - pos.best_price) / pos.best_price,
                };
                if retrace >= self.trailing_stop_pct {
                    return Some(format!(
                        "移动止损: 最优 {:.2} 回撤 {:.2}%, PnL {:.2}%",
                        pos.best_price,
                        retrace * 100.0,
                        pnl_pct * 100.0
                    ));
                }
            }
        }

        None
    }
}

#[async_trait]
impl Strategy for TrendStrategy {
    fn name(&self) -> &str {
        "trend_following"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        let mut signals = Vec::new();
        let mut states = self.states.lock().unwrap();

        for (symbol, ob) in &snapshot.order_books {
            let mid_price = match ob.mid_price() {
                Some(p) if p > 0.0 => p,
                _ => continue,
            };

            let state = states.entry(symbol.clone()).or_default();

            // 更新持仓最优价
            if let Some(pos) = state.position.as_mut() {
                match pos.side {
                    Side::Buy => pos.best_price = pos.best_price.max(mid_price),
                    Side::Sell => pos.best_price = pos.best_price.min(mid_price),
                }
            }

            // 1) 持仓离场检查（优先于开仓信号）
            if let Some(pos) = state.position {
                if let Some(reason) = self.check_exit(&pos, mid_price) {
                    let exit_side = match pos.side {
                        Side::Buy => Side::Sell,
                        Side::Sell => Side::Buy,
                    };
                    signals.push(TradeSignal {
                        symbol: symbol.clone(),
                        market_id: ob.market_id,
                        side: exit_side,
                        price: mid_price,
                        quantity: pos.quantity,
                        order_type: OrderType::Market,
                        reason,
                        timestamp: ob.timestamp,
                    });
                    state.position = None;
                    continue; // 本轮不再开新仓，等下一根K线确认趋势
                }
            }

            // 2) EMA 交叉信号
            let prices: Vec<f64> = match snapshot.candles.get(symbol) {
                Some(candles) => candles.iter().map(|c| c.close).collect(),
                None => continue,
            };
            if prices.len() < self.slow_period + 2 {
                debug!("{}: 价格数据不足，需要 {} 根", symbol, self.slow_period + 2);
                continue;
            }

            let (prev_fast, fast) = match ema_last_two(&prices, self.fast_period) {
                Some(v) => v,
                None => continue,
            };
            let (prev_slow, slow) = match ema_last_two(&prices, self.slow_period) {
                Some(v) => v,
                None => continue,
            };

            let separation = (fast - slow).abs() / slow;
            let cross_up = prev_fast <= prev_slow && fast > slow;
            let cross_down = prev_fast >= prev_slow && fast < slow;
            let bull_regime = fast > slow;
            let bear_regime = fast < slow;

            if cross_up {
                state.pending_side = Some(Side::Buy);
            } else if cross_down {
                state.pending_side = Some(Side::Sell);
            } else {
                match state.pending_side {
                    Some(Side::Buy) if !bull_regime => state.pending_side = None,
                    Some(Side::Sell) if !bear_regime => state.pending_side = None,
                    _ => {}
                }
            }

            let same_side_open = state
                .position
                .map(|p| Some(p.side) == state.pending_side)
                .unwrap_or(false);
            let desired_side = state.pending_side.filter(|&side| {
                let regime_ok = match side {
                    Side::Buy => bull_regime,
                    Side::Sell => bear_regime,
                };
                regime_ok && separation >= self.min_separation && !same_side_open
            });

            if let Some(side) = desired_side {
                // 已有同向仓位则不重复开仓
                if let Some(pos) = state.position {
                    if pos.side == side {
                        state.pending_side = None;
                        continue;
                    }
                }

                let new_qty = self.notional_per_trade / mid_price;
                // 反向持仓时，一并平掉旧仓（数量 = 旧仓 + 新仓，成交引擎先平后开）
                let close_qty = state.position.map(|p| p.quantity).unwrap_or(0.0);
                let total_qty = close_qty + new_qty;

                signals.push(TradeSignal {
                    symbol: symbol.clone(),
                    market_id: ob.market_id,
                    side,
                    price: mid_price,
                    quantity: total_qty,
                    order_type: OrderType::Market,
                    reason: match side {
                        Side::Buy => format!(
                            "金叉做多: EMA{} {:.2} 上穿 EMA{} {:.2} (分离 {:.3}%)",
                            self.fast_period,
                            fast,
                            self.slow_period,
                            slow,
                            separation * 100.0
                        ),
                        Side::Sell => format!(
                            "死叉做空: EMA{} {:.2} 下穿 EMA{} {:.2} (分离 {:.3}%)",
                            self.fast_period,
                            fast,
                            self.slow_period,
                            slow,
                            separation * 100.0
                        ),
                    },
                    timestamp: ob.timestamp,
                });

                state.position = Some(PositionState {
                    side,
                    entry_price: mid_price,
                    quantity: new_qty,
                    best_price: mid_price,
                });
                state.pending_side = None;
            }
        }

        if signals.is_empty() {
            Ok(None)
        } else {
            Ok(Some(signals))
        }
    }

    fn reset(&mut self) {
        self.states.lock().unwrap().clear();
    }

    fn clear_filled_state(&self) {
        // 趋势策略持仓状态以交易所为准，此处不清空仓位记录
    }
}

#[cfg(test)]
#[path = "trend_strategy_tests.rs"]
mod tests;
