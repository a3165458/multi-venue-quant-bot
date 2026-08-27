//! 回测/实盘收益门槛 parity：引擎应拒绝实盘 `RiskManager::check_signal` 会拒绝的入场信号。
//! 口径与 live 完全一致：risk_reducing 永远放行，入场必须净收益 > min_net_edge。

use super::engine_test_support::{known_candles, signal_price};
use super::*;
use async_trait::async_trait;
use config::{Config, File, FileFormat};

use crate::risk::profitability::ProfitabilityGuard;

fn settings(yaml: &str) -> Config {
    Config::builder()
        .add_source(File::from_str(yaml, FileFormat::Yaml))
        .build()
        .expect("test config")
}

/// 每根K线都发射一个带指定收益/减仓标记的信号。
struct EdgeSignal {
    pub qty: f64,
    pub edge_bps: Option<f64>,
    pub risk_reducing: bool,
}

#[async_trait]
impl Strategy for EdgeSignal {
    fn name(&self) -> &str {
        "edge_signal"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        let (symbol, price, ts) = signal_price(snapshot);
        Ok(Some(vec![TradeSignal {
            symbol,
            market_id: 0,
            side: Side::Buy,
            price,
            quantity: self.qty,
            order_type: OrderType::Market,
            reason: "edge signal".into(),
            timestamp: ts,
            expected_edge_bps: self.edge_bps,
            risk_reducing: self.risk_reducing,
            ..Default::default()
        }]))
    }

    fn reset(&mut self) {}
}

fn strict_guard() -> ProfitabilityGuard {
    // 总成本 8bps + 2bps 缓冲 → 需要净收益 > 2bps 且毛收益 > 10bps
    ProfitabilityGuard::from_config(&settings(
        r#"
profitability:
  enabled: true
  entry_fee_bps: 2.0
  exit_fee_bps: 2.0
  entry_slippage_bps: 1.0
  exit_slippage_bps: 1.0
  funding_bps: 1.0
  adverse_selection_bps: 1.0
  min_net_edge_bps: 2.0
"#,
    ))
    .expect("valid guard")
}

#[tokio::test]
async fn engine_with_guard_rejects_low_edge_entry() {
    let mut engine = BacktestEngine::new(1000.0, known_candles())
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_profitability(strict_guard());

    let results = engine
        .run(Box::new(EdgeSignal {
            qty: 1.0,
            edge_bps: Some(5.0), // 毛收益 5bps < 10bps 门槛
            risk_reducing: false,
        }))
        .await
        .expect("run");

    assert_eq!(results.total_trades, 0, "低收益入场不应成交");
    assert_eq!(results.blocked_by_profitability, 3, "三根K线各拦截一次");
    assert_eq!(results.final_capital, 1000.0, "现金不应变动");
}

#[tokio::test]
async fn engine_with_guard_allows_sufficient_edge_entry() {
    let mut engine = BacktestEngine::new(1000.0, known_candles())
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_profitability(strict_guard());

    let results = engine
        .run(Box::new(EdgeSignal {
            qty: 1.0,
            edge_bps: Some(50.0), // 毛收益 50bps >> 10bps 门槛
            risk_reducing: false,
        }))
        .await
        .expect("run");

    assert_eq!(results.total_trades, 1, "高收益入场应成交");
    assert_eq!(results.blocked_by_profitability, 0);
}

#[tokio::test]
async fn engine_with_guard_never_blocks_risk_reducing_exit() {
    let mut engine = BacktestEngine::new(1000.0, known_candles())
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_profitability(strict_guard());

    // 即使完全没有 expected_edge，减仓单也必须放行（与 live bypass 一致）
    let results = engine
        .run(Box::new(EdgeSignal {
            qty: 1.0,
            edge_bps: None,
            risk_reducing: true,
        }))
        .await
        .expect("run");

    assert_eq!(results.total_trades, 1, "减仓单不应被收益门槛拦截");
    assert_eq!(results.blocked_by_profitability, 0);
}

#[tokio::test]
async fn engine_without_guard_preserves_legacy_fills() {
    // 不配置 guard 时行为与历史完全一致：None edge 也照常成交
    let mut engine = BacktestEngine::new(1000.0, known_candles())
        .with_commission(0.0)
        .with_slippage(0.0);

    let results = engine
        .run(Box::new(EdgeSignal {
            qty: 1.0,
            edge_bps: None,
            risk_reducing: false,
        }))
        .await
        .expect("run");

    assert_eq!(results.total_trades, 1, "无 guard 时 None edge 仍应成交");
    assert_eq!(results.blocked_by_profitability, 0);
}

#[tokio::test]
async fn engine_rejects_missing_edge_entry_when_guard_enabled() {
    // 与 live 的 missing_expected_edge 语义一致：策略无法量化收益时拒绝入场
    let mut engine = BacktestEngine::new(1000.0, known_candles())
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_profitability(strict_guard());

    let results = engine
        .run(Box::new(EdgeSignal {
            qty: 1.0,
            edge_bps: None,
            risk_reducing: false,
        }))
        .await
        .expect("run");

    assert_eq!(results.total_trades, 0);
    assert_eq!(results.blocked_by_profitability, 3);
}

#[tokio::test]
async fn blocked_signals_do_not_distort_equity_curve() {
    // 拦截只跳过成交，不应写入权益曲线或影响 benchmark/DD 口径
    let mut engine = BacktestEngine::new(1000.0, known_candles())
        .with_commission(0.0)
        .with_slippage(0.0)
        .with_profitability(strict_guard());

    let results = engine
        .run(Box::new(EdgeSignal {
            qty: 1.0,
            edge_bps: Some(1.0), // 远低于门槛
            risk_reducing: false,
        }))
        .await
        .expect("run");

    assert_eq!(results.total_trades, 0);
    assert_eq!(results.equity_curve.len(), known_candles().len());
    assert!(
        results
            .equity_curve
            .iter()
            .all(|(_, eq)| (*eq - 1000.0).abs() < 1e-9),
        "被拦截时权益曲线应恒等于初始资金"
    );
    assert_eq!(results.liq_count, 0);
}
