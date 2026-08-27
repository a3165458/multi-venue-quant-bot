pub mod cross_dex_basis;
pub mod dca_strategy;
pub mod grid_strategy;
pub mod maker_quote;
pub mod trend_strategy;

use anyhow::Result;
use async_trait::async_trait;
use config::Config;

use crate::lighter::types::{MarketSnapshot, TradeSignal};

/// 策略特征
#[async_trait]
pub trait Strategy: Send + Sync {
    /// 策略名称
    #[allow(dead_code)]
    fn name(&self) -> &str;

    /// 评估市场状态，返回交易信号
    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>>;

    /// 重置策略状态
    #[allow(dead_code)]
    fn reset(&mut self);

    /// Clear filled/pending state (e.g. after stale orders cancelled).
    /// Uses interior mutability so it can be called via &self / Arc<dyn Strategy>.
    fn clear_filled_state(&self) {}
}

/// 根据配置创建策略
pub fn create_strategy(settings: &Config) -> Result<Box<dyn Strategy>> {
    let maker_enabled = settings
        .get_bool("trading.strategies.maker_quote.enabled")
        .unwrap_or(false);

    let grid_enabled = settings
        .get_bool("trading.strategies.grid_trading.enabled")
        .unwrap_or(false);

    let trend_enabled = settings
        .get_bool("trading.strategies.trend_following.enabled")
        .unwrap_or(false);

    if maker_enabled {
        Ok(Box::new(build_maker_quote_from_settings(settings)?))
    } else if grid_enabled {
        let grid_count = settings
            .get_int("trading.strategies.grid_trading.grid_count")
            .unwrap_or(10) as usize;
        let investment = settings
            .get_float("trading.strategies.grid_trading.investment_per_grid")
            .unwrap_or(100.0);
        let deviation = settings
            .get_float("trading.strategies.grid_trading.price_deviation")
            .unwrap_or(0.02);

        // 实盘路径：yaml 可选覆盖库存政策，但 research_nocap 一律拒绝
        let mode_raw = settings
            .get_string("trading.strategies.grid_trading.inventory_mode")
            .unwrap_or_else(|_| "hard".to_string());
        if mode_raw.trim().eq_ignore_ascii_case("research_nocap") {
            anyhow::bail!("实盘配置不允许 inventory_mode=research_nocap（研究专用）");
        }
        let mode = grid_strategy::InventoryMode::parse(&mode_raw)?;
        let soft_cap = settings
            .get_float("trading.strategies.grid_trading.soft_cap_grids")
            .ok();
        let hard_cap = settings
            .get_float("trading.strategies.grid_trading.hard_cap_grids")
            .ok();

        Ok(Box::new(grid_strategy::GridStrategy::with_inventory(
            grid_count, investment, deviation, mode, soft_cap, hard_cap,
        )?))
    } else if trend_enabled {
        let fast_ma = settings
            .get_int("trading.strategies.trend_following.fast_ma")
            .unwrap_or(10) as usize;
        let slow_ma = settings
            .get_int("trading.strategies.trend_following.slow_ma")
            .unwrap_or(30) as usize;
        let stop_loss = settings
            .get_float("trading.strategies.trend_following.stop_loss")
            .unwrap_or(0.05);
        let take_profit = settings
            .get_float("trading.strategies.trend_following.take_profit")
            .unwrap_or(0.1);
        let trailing_stop = settings
            .get_float("trading.strategies.trend_following.trailing_stop")
            .unwrap_or(0.0);
        let notional = settings
            .get_float("trading.strategies.trend_following.notional")
            .unwrap_or(1000.0);
        let adx_threshold = settings
            .get_float("trading.strategies.trend_following.adx_threshold")
            .unwrap_or(0.0);
        let adx_period = settings
            .get_int("trading.strategies.trend_following.adx_period")
            .unwrap_or(14) as usize;

        let slow_period_read = slow_ma;
        let confirm_min = settings
            .get_float("trading.strategies.trend_following.confirm_slope_min")
            .unwrap_or(0.0);
        let confirm_lookback = settings
            .get_int("trading.strategies.trend_following.confirm_lookback")
            .unwrap_or((slow_period_read / 2).max(1) as i64)
            as usize;

        Ok(Box::new(
            trend_strategy::TrendStrategy::with_options(
                fast_ma,
                slow_ma,
                stop_loss,
                take_profit,
                trailing_stop,
                notional,
            )
            .with_adx_filter(adx_threshold, adx_period)
            .with_slope_confirm(confirm_min, confirm_lookback),
        ))
    } else {
        // Default to grid strategy
        Ok(Box::new(grid_strategy::GridStrategy::new(10, 100.0, 0.02)))
    }
}

fn build_maker_quote_from_settings(settings: &Config) -> Result<maker_quote::MakerQuoteStrategy> {
    let prefix = "trading.strategies.maker_quote";
    let spread_bps = settings
        .get_float(&format!("{prefix}.spread_bps"))
        .unwrap_or(6.0);
    let per_quote_notional = settings
        .get_float(&format!("{prefix}.per_quote_notional"))
        .unwrap_or(200.0);
    let requote_threshold_bps = settings
        .get_float(&format!("{prefix}.requote_threshold_bps"))
        .unwrap_or(2.0);
    let requote_cooldown_secs = settings
        .get_int(&format!("{prefix}.requote_cooldown_secs"))
        .unwrap_or(5);
    let soft_cap_notional = settings
        .get_float(&format!("{prefix}.soft_cap_notional"))
        .unwrap_or(600.0);
    let hard_cap_notional = settings
        .get_float(&format!("{prefix}.hard_cap_notional"))
        .unwrap_or(1000.0);
    let trend_filter = settings
        .get_bool(&format!("{prefix}.trend_filter"))
        .unwrap_or(true);
    let ema_period = settings
        .get_int(&format!("{prefix}.ema_period"))
        .unwrap_or(20) as usize;
    let trend_block_bps = settings
        .get_float(&format!("{prefix}.trend_block_bps"))
        .unwrap_or(6.0);
    let min_quote_notional = settings
        .get_float(&format!("{prefix}.min_quote_notional"))
        .unwrap_or(5.0);
    let vol_window = settings
        .get_int(&format!("{prefix}.vol_window"))
        .unwrap_or(0) as usize;
    let vol_multiplier = settings
        .get_float(&format!("{prefix}.vol_multiplier"))
        .unwrap_or(0.0);
    let max_skew_bps = settings
        .get_float(&format!("{prefix}.max_skew_bps"))
        .unwrap_or(0.0);
    let total_quote_budget = settings
        .get_float(&format!("{prefix}.total_quote_budget"))
        .unwrap_or(0.0);
    let feature_interval_secs = settings
        .get_int(&format!("{prefix}.feature_interval_secs"))
        .unwrap_or(60);
    let jump_circuit_breaker_bps = settings
        .get_float(&format!("{prefix}.jump_circuit_breaker_bps"))
        .unwrap_or(20.0);
    let max_book_spread_bps = settings
        .get_float(&format!("{prefix}.max_book_spread_bps"))
        .unwrap_or(40.0);
    let min_book_spread_bps = settings
        .get_float(&format!("{prefix}.min_book_spread_bps"))
        .unwrap_or(0.0);
    let wide_book_size_mult = settings
        .get_float(&format!("{prefix}.wide_book_size_mult"))
        .unwrap_or(1.0);
    let max_bbo_imbalance = settings
        .get_float(&format!("{prefix}.max_bbo_imbalance"))
        .unwrap_or(0.0);
    let flatten_only = settings
        .get_bool(&format!("{prefix}.flatten_only"))
        .unwrap_or(false);
    let join_inside_ticks = settings
        .get_int(&format!("{prefix}.join_inside_ticks"))
        .unwrap_or(0);
    let flatten_mid_secs = settings
        .get_int(&format!("{prefix}.flatten_mid_secs"))
        .unwrap_or(6);
    let flatten_ioc_secs = settings
        .get_int(&format!("{prefix}.flatten_ioc_secs"))
        .unwrap_or(15);
    let circuit_breaker_cooldown_secs = settings
        .get_int(&format!("{prefix}.circuit_breaker_cooldown_secs"))
        .unwrap_or(60);
    let cash_open_guard = settings
        .get_bool(&format!("{prefix}.cash_open_guard"))
        .unwrap_or(true);
    let cash_open_guard_before_minutes = settings
        .get_int(&format!("{prefix}.cash_open_guard_before_minutes"))
        .unwrap_or(5);
    let cash_open_guard_after_minutes = settings
        .get_int(&format!("{prefix}.cash_open_guard_after_minutes"))
        .unwrap_or(20);
    let quote_mode = maker_quote::QuoteMode::parse(
        &settings
            .get_string(&format!("{prefix}.quote_mode"))
            .unwrap_or_else(|_| "mid_spread".to_string()),
    )?;

    maker_quote::MakerQuoteStrategy::new(
        spread_bps,
        per_quote_notional,
        requote_threshold_bps,
        requote_cooldown_secs,
        soft_cap_notional,
        hard_cap_notional,
        trend_filter,
        ema_period,
        trend_block_bps,
        min_quote_notional,
    )?
    .with_adaptive_spread(vol_window, vol_multiplier)?
    .with_inventory_skew(max_skew_bps)?
    .with_quote_budget(total_quote_budget)?
    .with_feature_interval(feature_interval_secs)?
    .with_market_circuit_breaker(
        jump_circuit_breaker_bps,
        max_book_spread_bps,
        circuit_breaker_cooldown_secs,
    )?
    .with_min_book_spread(min_book_spread_bps)?
    .with_wide_book_size_mult(wide_book_size_mult)?
    .with_max_bbo_imbalance(max_bbo_imbalance)?
    .with_flatten_cycle(
        flatten_only,
        join_inside_ticks,
        flatten_mid_secs,
        flatten_ioc_secs,
    )?
    .with_cash_open_guard(
        cash_open_guard,
        cash_open_guard_before_minutes,
        cash_open_guard_after_minutes,
    )?
    .with_quote_mode(quote_mode)
}

/// 根据策略名称创建策略（用于回测）
#[allow(dead_code)]
pub fn create_strategy_from_name(name: &str) -> Result<Box<dyn Strategy>> {
    create_strategy_with_params(name, None)
}

/// 根据策略名和可选参数创建策略
/// params 格式: "grid_count=10,investment=8.0,deviation=0.008"
pub fn create_strategy_with_params(name: &str, params: Option<&str>) -> Result<Box<dyn Strategy>> {
    let kv = parse_params(params.unwrap_or(""));

    match name {
        "maker_quote" | "maker" => {
            let spread_bps = kv
                .get("spread_bps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(6.0);
            let per_quote_notional = kv
                .get("per_quote_notional")
                .or_else(|| kv.get("notional"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(200.0);
            let requote_threshold_bps = kv
                .get("requote_threshold_bps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(2.0);
            let requote_cooldown_secs = kv
                .get("requote_cooldown_secs")
                .and_then(|v| v.parse().ok())
                .unwrap_or(5);
            let soft_cap_notional = kv
                .get("soft_cap_notional")
                .and_then(|v| v.parse().ok())
                .unwrap_or(600.0);
            let hard_cap_notional = kv
                .get("hard_cap_notional")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1000.0);
            let trend_filter = kv
                .get("trend_filter")
                .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
                .unwrap_or(true);
            let ema_period = kv
                .get("ema_period")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            let trend_block_bps = kv
                .get("trend_block_bps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(6.0);
            let min_quote_notional = kv
                .get("min_quote_notional")
                .and_then(|v| v.parse().ok())
                .unwrap_or(5.0);
            let vol_window = kv
                .get("vol_window")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let vol_multiplier = kv
                .get("vol_multiplier")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let max_skew_bps = kv
                .get("max_skew_bps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let total_quote_budget = kv
                .get("total_quote_budget")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let feature_interval_secs = kv
                .get("feature_interval_secs")
                .and_then(|v| v.parse().ok())
                .unwrap_or(60);
            let jump_circuit_breaker_bps = kv
                .get("jump_circuit_breaker_bps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20.0);
            let max_book_spread_bps = kv
                .get("max_book_spread_bps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(40.0);
            let min_book_spread_bps = kv
                .get("min_book_spread_bps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let wide_book_size_mult = kv
                .get("wide_book_size_mult")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0);
            let max_bbo_imbalance = kv
                .get("max_bbo_imbalance")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let flatten_only = kv
                .get("flatten_only")
                .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
                .unwrap_or(false);
            let join_inside_ticks = kv
                .get("join_inside_ticks")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let flatten_mid_secs = kv
                .get("flatten_mid_secs")
                .and_then(|v| v.parse().ok())
                .unwrap_or(6);
            let flatten_ioc_secs = kv
                .get("flatten_ioc_secs")
                .and_then(|v| v.parse().ok())
                .unwrap_or(15);
            let circuit_breaker_cooldown_secs = kv
                .get("circuit_breaker_cooldown_secs")
                .and_then(|v| v.parse().ok())
                .unwrap_or(60);
            let cash_open_guard = kv
                .get("cash_open_guard")
                .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
                .unwrap_or(true);
            let cash_open_guard_before_minutes = kv
                .get("cash_open_guard_before_minutes")
                .and_then(|v| v.parse().ok())
                .unwrap_or(5);
            let cash_open_guard_after_minutes = kv
                .get("cash_open_guard_after_minutes")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            let quote_mode = maker_quote::QuoteMode::parse(
                kv.get("quote_mode")
                    .map(|s| s.as_str())
                    .unwrap_or("mid_spread"),
            )?;
            Ok(Box::new(
                maker_quote::MakerQuoteStrategy::new(
                    spread_bps,
                    per_quote_notional,
                    requote_threshold_bps,
                    requote_cooldown_secs,
                    soft_cap_notional,
                    hard_cap_notional,
                    trend_filter,
                    ema_period,
                    trend_block_bps,
                    min_quote_notional,
                )?
                .with_adaptive_spread(vol_window, vol_multiplier)?
                .with_inventory_skew(max_skew_bps)?
                .with_quote_budget(total_quote_budget)?
                .with_feature_interval(feature_interval_secs)?
                .with_market_circuit_breaker(
                    jump_circuit_breaker_bps,
                    max_book_spread_bps,
                    circuit_breaker_cooldown_secs,
                )?
                .with_min_book_spread(min_book_spread_bps)?
                .with_wide_book_size_mult(wide_book_size_mult)?
                .with_max_bbo_imbalance(max_bbo_imbalance)?
                .with_flatten_cycle(
                    flatten_only,
                    join_inside_ticks,
                    flatten_mid_secs,
                    flatten_ioc_secs,
                )?
                .with_cash_open_guard(
                    cash_open_guard,
                    cash_open_guard_before_minutes,
                    cash_open_guard_after_minutes,
                )?
                .with_quote_mode(quote_mode)?,
            ))
        }
        "grid_trading" | "grid" => {
            let grid_count = kv
                .get("grid_count")
                .and_then(|v| v.parse().ok())
                .unwrap_or(10);
            let investment = kv
                .get("investment_per_grid")
                .or_else(|| kv.get("investment"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(8.0);
            let deviation = kv
                .get("price_deviation")
                .or_else(|| kv.get("deviation"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.008);

            // 库存政策（回测/研究）。未指定时保持原行为 = 实盘硬上限。
            let mode_raw = kv.get("inventory_mode").map(|s| s.as_str());
            let soft_cap = kv
                .get("soft_cap")
                .or_else(|| kv.get("soft_cap_grids"))
                .map(|v| v.parse::<f64>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("soft_cap 解析失败: {e}"))?;
            let hard_cap = kv
                .get("hard_cap")
                .or_else(|| kv.get("hard_cap_grids"))
                .map(|v| v.parse::<f64>())
                .transpose()
                .map_err(|e| anyhow::anyhow!("hard_cap 解析失败: {e}"))?;

            match mode_raw {
                None if soft_cap.is_none() && hard_cap.is_none() => Ok(Box::new(
                    grid_strategy::GridStrategy::new(grid_count, investment, deviation),
                )),
                _ => {
                    let mode = grid_strategy::InventoryMode::parse(mode_raw.unwrap_or("hard"))?;
                    Ok(Box::new(grid_strategy::GridStrategy::with_inventory(
                        grid_count, investment, deviation, mode, soft_cap, hard_cap,
                    )?))
                }
            }
        }
        "trend_following" | "trend" => {
            let fast_ma = kv.get("fast_ma").and_then(|v| v.parse().ok()).unwrap_or(7);
            let slow_ma = kv.get("slow_ma").and_then(|v| v.parse().ok()).unwrap_or(21);
            let stop_loss = kv
                .get("stop_loss")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.03);
            let take_profit = kv
                .get("take_profit")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.06);
            let trailing_stop = kv
                .get("trailing_stop")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let notional = kv
                .get("notional")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1000.0);
            let adx_threshold = kv
                .get("adx_threshold")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let adx_period = kv
                .get("adx_period")
                .and_then(|v| v.parse().ok())
                .unwrap_or(14);
            let confirm_min = kv
                .get("confirm_slope_min")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let confirm_lookback = kv
                .get("confirm_lookback")
                .and_then(|v| v.parse().ok())
                .unwrap_or((slow_ma / 2).max(1));
            Ok(Box::new(
                trend_strategy::TrendStrategy::with_options(
                    fast_ma,
                    slow_ma,
                    stop_loss,
                    take_profit,
                    trailing_stop,
                    notional,
                )
                .with_adx_filter(adx_threshold, adx_period)
                .with_slope_confirm(confirm_min, confirm_lookback),
            ))
        }
        "dca" => {
            let interval = kv
                .get("interval")
                .and_then(|v| v.parse().ok())
                .unwrap_or(4.0);
            let amount = kv.get("amount").and_then(|v| v.parse().ok()).unwrap_or(5.0);
            let dip = kv
                .get("dip_threshold")
                .and_then(|v| v.parse().ok())
                .unwrap_or(2.0);
            Ok(Box::new(dca_strategy::DcaStrategy::new(
                interval, amount, dip,
            )))
        }
        _ => anyhow::bail!("未知策略: {}", name),
    }
}

#[cfg(test)]
#[path = "params_tests.rs"]
mod params_tests;

fn parse_params(s: &str) -> std::collections::HashMap<String, String> {
    s.split(',')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?.trim();
            let value = parts.next()?.trim();
            if key.is_empty() {
                None
            } else {
                Some((key.to_string(), value.to_string()))
            }
        })
        .collect()
}
