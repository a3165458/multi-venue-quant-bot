//! Aster Pro Futures V3 live control loop.
//!
//! This module deliberately keeps the exchange protocol adapter (`aster.rs`)
//! separate from live-trading policy. Every ambiguous execution outcome fails
//! closed: pause first, cancel all configured symbols, then leave the loop.

use anyhow::{bail, Context, Result};
use chrono::{TimeZone, Utc};
use config::Config;
use futures::{SinkExt, StreamExt};
use multi_venue_quant_bot::aster::{
    Account, AccountUpdate, AsterClient, AsterCredentials, AsterError, AsterMarket, AsterWsEvent,
    Income, IncomeQuery, ModifyOrderRequest, NewOrderRequest, Order, OrderSide, OrderTradeUpdate,
    PositionRisk, UserTrade, UserTradesQuery, MAINNET_REST_URL, MAINNET_WS_URL,
};
use multi_venue_quant_bot::exchange::LiveVenue;
use serde::{Deserialize, Serialize};
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::RwLock;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::aster_hft_shadow::{HftLabConfig, HftLabSnapshot, HftProfileConfig, HftShadowLab};
use crate::aster_shadow::{ShadowConfig, ShadowMakerMonitor, ShadowSnapshot};
use crate::{dashboard, data, env_profiles, lighter, risk, strategy};

const NETWORK: &str = "aster-mainnet";
const LEDGER_FILE: &str = "aster_ledger.json";
const SHADOW_FILE: &str = "shadow_metrics.json";
const HFT_SHADOW_FILE: &str = "hft_shadow_metrics.json";
const COUNTDOWN_MS: u64 = 120_000;
const PARSE_ERROR_LIMIT: u8 = 3;
const SAFE_SESSION_AGE: Duration = Duration::from_secs(23 * 60 * 60 + 50 * 60);
const HISTORY_LOOKBACK_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const INCOME_OVERLAP_MS: u64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveDispatch {
    Lighter,
    Arcus,
    Aster,
    Hyperliquid,
}

impl LiveDispatch {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "lighter" => Ok(Self::Lighter),
            "arcus" => Ok(Self::Arcus),
            "aster" => Ok(Self::Aster),
            "hyperliquid" => Ok(Self::Hyperliquid),
            other => bail!("unsupported exchange.kind {other}; refusing to select a fallback"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalOrderStatus {
    Pending,
    Live,
    Unknown,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalOrder {
    pub(crate) strategy_key: String,
    pub(crate) exchange_client_id: String,
    pub(crate) order_id: Option<u64>,
    pub(crate) symbol: String,
    pub(crate) side: OrderSide,
    pub(crate) price: f64,
    pub(crate) quantity: f64,
    pub(crate) status: LocalOrderStatus,
    pub(crate) last_event_time: u64,
    pub(crate) last_transaction_time: u64,
}

#[derive(Debug, Default)]
pub(crate) struct OrderTracker {
    pub(crate) by_strategy_key: HashMap<String, LocalOrder>,
    by_exchange_client_id: HashMap<String, String>,
    by_order_id: HashMap<u64, String>,
}

impl OrderTracker {
    pub(crate) fn insert(&mut self, order: LocalOrder) {
        self.remove(&order.strategy_key);
        self.by_exchange_client_id
            .insert(order.exchange_client_id.clone(), order.strategy_key.clone());
        if let Some(order_id) = order.order_id {
            self.by_order_id
                .insert(order_id, order.strategy_key.clone());
        }
        self.by_strategy_key
            .insert(order.strategy_key.clone(), order);
    }

    fn remove(&mut self, strategy_key: &str) -> Option<LocalOrder> {
        let removed = self.by_strategy_key.remove(strategy_key)?;
        self.by_exchange_client_id
            .remove(&removed.exchange_client_id);
        if let Some(order_id) = removed.order_id {
            self.by_order_id.remove(&order_id);
        }
        Some(removed)
    }

    fn strategy_key_for(&self, client_id: &str, order_id: u64) -> Option<String> {
        self.by_exchange_client_id
            .get(client_id)
            .or_else(|| self.by_order_id.get(&order_id))
            .cloned()
    }

    pub(crate) fn apply_ws_order(&mut self, update: &OrderTradeUpdate) -> bool {
        let Some(strategy_key) =
            self.strategy_key_for(&update.order.client_order_id, update.order.order_id)
        else {
            return false;
        };
        let Some(local) = self.by_strategy_key.get_mut(&strategy_key) else {
            return false;
        };
        let incoming = (update.event_time, update.transaction_time);
        if incoming <= (local.last_event_time, local.last_transaction_time) {
            return false;
        }
        local.last_event_time = update.event_time;
        local.last_transaction_time = update.transaction_time;
        local.order_id = Some(update.order.order_id);
        self.by_order_id
            .insert(update.order.order_id, strategy_key.clone());
        match update.order.status.to_ascii_uppercase().as_str() {
            "NEW" | "PARTIALLY_FILLED" => local.status = LocalOrderStatus::Live,
            "FILLED" | "CANCELED" | "EXPIRED" | "REJECTED" => {
                self.remove(&strategy_key);
            }
            _ => {}
        }
        true
    }

    fn reconcile_open_orders(&mut self, orders: &[Order]) {
        let mut live_keys = HashSet::new();
        for order in orders {
            let Some(key) = self.strategy_key_for(&order.client_order_id, order.order_id) else {
                continue;
            };
            live_keys.insert(key.clone());
            if let Some(local) = self.by_strategy_key.get_mut(&key) {
                local.status = LocalOrderStatus::Live;
                local.order_id = Some(order.order_id);
                local.price = parse_f64(&order.price).unwrap_or(local.price);
                local.quantity = parse_f64(&order.orig_qty).unwrap_or(local.quantity);
                self.by_order_id.insert(order.order_id, key);
            }
        }
        let gone: Vec<String> = self
            .by_strategy_key
            .iter()
            .filter(|(key, order)| {
                order.status == LocalOrderStatus::Live && !live_keys.contains(*key)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in gone {
            self.remove(&key);
        }
    }

    fn local_open_count_not_in_rest(&self, rest: &[Order]) -> usize {
        self.by_strategy_key
            .values()
            .filter(|local| {
                !rest.iter().any(|order| {
                    order.client_order_id == local.exchange_client_id
                        || local.order_id == Some(order.order_id)
                })
            })
            .count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmissionFailureDecision {
    Reconcile,
    Reject,
}

pub(crate) fn submission_failure_decision(error: &AsterError) -> SubmissionFailureDecision {
    match error {
        AsterError::UnknownExecution { .. } => SubmissionFailureDecision::Reconcile,
        // A transport error can occur after the exchange accepted the bytes,
        // even when reqwest does not classify it specifically as a timeout.
        AsterError::Transport(_) => SubmissionFailureDecision::Reconcile,
        _ => SubmissionFailureDecision::Reject,
    }
}

fn safe_aster_error(operation: &str, error: &AsterError) -> anyhow::Error {
    let detail = match error {
        AsterError::Credentials(_) => "credential validation failed".to_string(),
        AsterError::InvalidRequest(_) => "request validation failed".to_string(),
        AsterError::Signing(_) => "request signing failed".to_string(),
        AsterError::Transport(source) if source.is_timeout() => "transport timeout".to_string(),
        AsterError::Transport(source) if source.is_connect() => {
            "transport connection failed".to_string()
        }
        AsterError::Transport(_) => "transport request failed".to_string(),
        AsterError::InvalidResponse(_) => "invalid response".to_string(),
        AsterError::RateLimited { .. } => "HTTP 429 rate limit".to_string(),
        AsterError::IpBanned { .. } => "HTTP 418 IP ban".to_string(),
        AsterError::UnknownExecution { .. } => "HTTP 503 execution status unknown".to_string(),
        AsterError::Api { status, code, .. } => {
            format!("API error HTTP {status}, code {code:?}")
        }
    };
    anyhow::anyhow!("{operation}: {detail} (signed parameters redacted)")
}

pub(crate) fn maker_strategy_allowed(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "maker_quote" | "maker"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuoteReplaceDecision {
    Noop,
    Amend,
    CancelThenWait,
    BlockedUnresolved,
}

pub(crate) fn quote_replace_decision(
    existing: &LocalOrder,
    new_price: f64,
    post_only: bool,
) -> QuoteReplaceDecision {
    if (existing.price - new_price).abs() <= f64::EPSILON {
        return QuoteReplaceDecision::Noop;
    }
    if existing.status != LocalOrderStatus::Live {
        return QuoteReplaceDecision::BlockedUnresolved;
    }
    if post_only && existing.order_id.is_some() {
        QuoteReplaceDecision::Amend
    } else {
        QuoteReplaceDecision::CancelThenWait
    }
}

pub(crate) fn modify_order_is_gone(error: &AsterError) -> bool {
    match error {
        AsterError::Api { code, message, .. } => {
            matches!(code, Some(-2011) | Some(-2013))
                || message.to_ascii_uppercase().contains("UNKNOWN ORDER")
                || message
                    .to_ascii_uppercase()
                    .contains("ORDER DOES NOT EXIST")
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Copy)]
struct RiskCeilings {
    max_drawdown_pct: f64,
    daily_loss_limit_pct: f64,
    max_leverage: f64,
    position_stop_loss_pct: f64,
    position_take_profit_pct: f64,
}

impl RiskCeilings {
    fn from_config(value: &serde_json::Value) -> Result<Self> {
        Ok(Self {
            max_drawdown_pct: required_json_number(value, "max_drawdown_pct")?,
            daily_loss_limit_pct: required_json_number(value, "daily_loss_limit_pct")?,
            max_leverage: required_json_number(value, "max_leverage")?,
            position_stop_loss_pct: required_json_number(value, "position_stop_loss_pct")?,
            position_take_profit_pct: required_json_number(value, "position_take_profit_pct")?,
        })
    }
}

pub(crate) fn signal_allowed(
    is_cancel: bool,
    paused: bool,
    active_markets: &[u32],
    market_id: u32,
    risk_reducing: bool,
    post_only: bool,
) -> bool {
    if is_cancel {
        return true;
    }
    if risk_reducing && !post_only {
        return true;
    }
    !paused && post_only && active_markets.contains(&market_id)
}

static CLIENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(crate) fn unique_client_id(strategy_key: &str, market_id: u32, side: OrderSide) -> String {
    let sequence = CLIENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = DefaultHasher::new();
    strategy_key.hash(&mut hasher);
    market_id.hash(&mut hasher);
    (side == OrderSide::Buy).hash(&mut hasher);
    sequence.hash(&mut hasher);
    unix_millis().hash(&mut hasher);
    let side = if side == OrderSide::Buy { 'b' } else { 's' };
    let value = format!("qb{market_id:x}{side}{:016x}", hasher.finish());
    debug_assert!(value.len() <= 36);
    value
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExposureInput {
    pub(crate) positions: Vec<(String, f64, f64)>,
    pub(crate) rest_orders: Vec<(String, String, Option<u64>, OrderSide, f64, f64)>,
    pub(crate) local_orders: Vec<LocalOrder>,
}

pub(crate) fn calculate_exposure(
    input: &ExposureInput,
    signal_symbol: &str,
) -> risk::risk_manager::RiskExposure {
    let mut by_symbol = HashMap::<String, (f64, f64, f64)>::new();
    for (symbol, signed_quantity, mark) in &input.positions {
        by_symbol.entry(symbol.clone()).or_default().0 = signed_quantity * mark;
    }
    let mut seen_clients = HashSet::new();
    let mut seen_orders = HashSet::new();
    for (symbol, client_id, order_id, side, price, quantity) in &input.rest_orders {
        seen_clients.insert(client_id.clone());
        if let Some(order_id) = order_id {
            seen_orders.insert(*order_id);
        }
        add_open_notional(&mut by_symbol, symbol, *side, price * quantity);
    }
    for order in &input.local_orders {
        if seen_clients.contains(&order.exchange_client_id)
            || order.order_id.is_some_and(|id| seen_orders.contains(&id))
        {
            continue;
        }
        add_open_notional(
            &mut by_symbol,
            &order.symbol,
            order.side,
            order.price * order.quantity,
        );
    }
    let total_worst_case_notional = by_symbol
        .values()
        .map(|(position, buys, sells)| {
            risk::risk_manager::worst_case_symbol_notional(*position, *buys, *sells)
        })
        .sum();
    let (symbol_position_notional, symbol_buy_open_notional, symbol_sell_open_notional) =
        by_symbol.get(signal_symbol).copied().unwrap_or_default();
    risk::risk_manager::RiskExposure {
        symbol_position_notional,
        symbol_buy_open_notional,
        symbol_sell_open_notional,
        total_worst_case_notional,
    }
}

fn add_open_notional(
    by_symbol: &mut HashMap<String, (f64, f64, f64)>,
    symbol: &str,
    side: OrderSide,
    notional: f64,
) {
    let entry = by_symbol.entry(symbol.to_string()).or_default();
    match side {
        OrderSide::Buy => entry.1 += notional.abs(),
        OrderSide::Sell => entry.2 += notional.abs(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct AsterLedger {
    #[serde(default)]
    pub(crate) seen_trade_ids: HashSet<String>,
    #[serde(default)]
    pub(crate) seen_income_ids: HashSet<String>,
    #[serde(default)]
    pub(crate) trade_high_water: HashMap<String, u64>,
    #[serde(default)]
    pub(crate) income_high_water_ms: u64,
    #[serde(default)]
    pnl_checkpoint: Option<dashboard::server::PersistentPnlData>,
}

impl AsterLedger {
    fn path() -> Result<PathBuf> {
        dashboard::runtime_paths::data_file(NETWORK, LEDGER_FILE)
    }

    fn load() -> Result<Option<Self>> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(None);
        }
        let bytes =
            std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let ledger = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(ledger))
    }

    fn save_atomic(&self) -> Result<()> {
        let path = Self::path()?;
        let parent = path.parent().context("Aster ledger path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, &path)?;
        Ok(())
    }

    pub(crate) fn record_trade_id(&mut self, symbol: &str, id: u64) -> bool {
        self.trade_high_water
            .entry(symbol.to_string())
            .and_modify(|high| *high = (*high).max(id))
            .or_insert(id);
        self.seen_trade_ids.insert(format!("{symbol}:{id}"))
    }

    pub(crate) fn record_income(&mut self, income: &Income) -> bool {
        self.income_high_water_ms = self.income_high_water_ms.max(income.time);
        self.seen_income_ids.insert(income_key(income))
    }
}

fn pnl_snapshot(
    dashboard: &dashboard::server::DashboardState,
) -> dashboard::server::PersistentPnlData {
    let mut daily_pnl_map = dashboard.daily_pnl_map.clone();
    daily_pnl_map.insert(
        Utc::now().format("%Y-%m-%d").to_string(),
        dashboard.daily_realized_pnl,
    );
    dashboard::server::PersistentPnlData {
        total_realized_pnl: dashboard.total_realized_pnl,
        total_funding_pnl: dashboard.total_funding_pnl,
        daily_funding_pnl: dashboard.daily_funding_pnl,
        initial_equity: dashboard.initial_equity,
        peak_equity: dashboard.peak_equity,
        equity_history: dashboard.equity_history.clone(),
        pnl_history: dashboard.pnl_history.clone(),
        trade_history: dashboard.trade_history.clone(),
        daily_pnl_map,
        total_volume: dashboard.total_volume,
        total_closed_trades: dashboard.total_closed_trades,
    }
}

fn income_key(income: &Income) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        income.symbol,
        income.income_type,
        income.time,
        income.tran_id,
        income.trade_id,
        income.income
    )
}

pub(crate) fn one_way_positions(positions: &[PositionRisk]) -> Result<Vec<serde_json::Value>> {
    positions
        .iter()
        .filter_map(|position| {
            let amount = match parse_f64(&position.position_amt) {
                Ok(value) if value.abs() > 1e-12 => value,
                Ok(_) => return None,
                Err(error) => return Some(Err(error)),
            };
            if !position.position_side.eq_ignore_ascii_case("BOTH") {
                return Some(Err(anyhow::anyhow!(
                    "Aster returned non-one-way position {} side {}",
                    position.symbol,
                    position.position_side
                )));
            }
            Some((|| {
                Ok(serde_json::json!({
                    "symbol": position.symbol,
                    "side": if amount > 0.0 { "Buy" } else { "Sell" },
                    "size": amount.abs(),
                    "entry_price": parse_f64(&position.entry_price)?,
                    "mark_price": parse_f64(&position.mark_price)?,
                    "unrealized_pnl": parse_f64(&position.un_realized_profit)?,
                    "leverage": parse_f64(&position.leverage)?,
                }))
            })())
        })
        .collect()
}

pub(crate) fn account_totals(account: &Account) -> Result<(f64, f64, f64)> {
    let reported_equity = parse_f64(&account.total_margin_balance)?;
    let reported_available = parse_f64(&account.available_balance)?;
    let reported_unrealized = parse_f64(&account.total_unrealized_profit)?;
    let mut asset_equity = 0.0;
    let mut asset_available = 0.0;
    let mut asset_unrealized = 0.0;
    for asset in &account.assets {
        if !asset.margin_available {
            continue;
        }
        asset_equity += parse_f64(&asset.margin_balance)?;
        asset_available += parse_f64(&asset.available_balance)?;
        asset_unrealized += parse_f64(&asset.unrealized_profit)?;
    }
    Ok((
        if reported_equity.abs() > 1e-12 || asset_equity.abs() <= 1e-12 {
            reported_equity
        } else {
            asset_equity
        },
        if reported_available.abs() > 1e-12 || asset_available.abs() <= 1e-12 {
            reported_available
        } else {
            asset_available
        },
        if reported_unrealized.abs() > 1e-12 || asset_unrealized.abs() <= 1e-12 {
            reported_unrealized
        } else {
            asset_unrealized
        },
    ))
}

pub(crate) fn validate_runtime_positions(
    positions: &[PositionRisk],
    symbols: &[String],
    require_isolated_margin: bool,
) -> Result<()> {
    for position in positions {
        if !position.position_side.eq_ignore_ascii_case("BOTH") {
            bail!(
                "Aster returned non-one-way position {} side {}",
                position.symbol,
                position.position_side
            );
        }
        let quantity = parse_f64(&position.position_amt)?;
        if quantity.abs() > 1e-12
            && !symbols
                .iter()
                .any(|symbol| symbol.eq_ignore_ascii_case(&position.symbol))
        {
            bail!(
                "Aster sub-account has an open position in unconfigured symbol {}; refusing to trade",
                position.symbol
            );
        }
    }
    if require_isolated_margin {
        for symbol in symbols {
            let position = positions
                .iter()
                .find(|position| position.symbol.eq_ignore_ascii_case(symbol))
                .with_context(|| {
                    format!("Aster positionRisk omitted configured symbol {symbol}")
                })?;
            if !position.margin_type.eq_ignore_ascii_case("isolated") {
                bail!("Aster symbol {symbol} is not in isolated margin mode");
            }
        }
    }
    Ok(())
}

pub(crate) fn apply_ws_account_update(
    positions: &mut Vec<PositionRisk>,
    update: &AccountUpdate,
    last_events: &mut HashMap<String, (u64, u64)>,
    last_prices: &HashMap<String, f64>,
) -> Result<()> {
    for changed in &update.positions {
        if !changed.position_side.eq_ignore_ascii_case("BOTH") {
            bail!(
                "Aster ACCOUNT_UPDATE changed {} to non-one-way side {}",
                changed.symbol,
                changed.position_side
            );
        }
        let incoming = (update.event_time, update.transaction_time);
        if last_events
            .get(&changed.symbol)
            .is_some_and(|last| incoming <= *last)
        {
            continue;
        }
        if let Some(position) = positions
            .iter_mut()
            .find(|position| position.symbol == changed.symbol)
        {
            position.position_amt = changed.position_amount.clone();
            position.entry_price = changed.entry_price.clone();
            position.un_realized_profit = changed.unrealized_pnl.clone();
            position.margin_type = changed.margin_type.clone();
            position.position_side = changed.position_side.clone();
            position.update_time = update.event_time;
        } else {
            positions.push(PositionRisk {
                symbol: changed.symbol.clone(),
                position_amt: changed.position_amount.clone(),
                entry_price: changed.entry_price.clone(),
                mark_price: last_prices
                    .get(&changed.symbol)
                    .copied()
                    .map(decimal)
                    .unwrap_or_else(|| "0".to_string()),
                un_realized_profit: changed.unrealized_pnl.clone(),
                liquidation_price: "0".to_string(),
                leverage: "1".to_string(),
                margin_type: changed.margin_type.clone(),
                position_side: changed.position_side.clone(),
                update_time: update.event_time,
            });
        }
        last_events.insert(changed.symbol.clone(), incoming);
    }
    Ok(())
}

pub(crate) fn merge_rest_positions(
    positions: &mut Vec<PositionRisk>,
    rest_positions: Vec<PositionRisk>,
    last_events: &mut HashMap<String, (u64, u64)>,
) {
    let mut merged = rest_positions;
    for existing in positions.iter() {
        let Some(last_ws) = last_events.get(&existing.symbol).copied() else {
            continue;
        };
        match merged
            .iter_mut()
            .find(|position| position.symbol == existing.symbol)
        {
            Some(rest) if last_ws > (rest.update_time, 0) => {
                *rest = existing.clone();
            }
            None => merged.push(existing.clone()),
            _ => {}
        }
    }
    for position in &merged {
        let rest_event = (position.update_time, 0);
        last_events
            .entry(position.symbol.clone())
            .and_modify(|last| *last = (*last).max(rest_event))
            .or_insert(rest_event);
    }
    *positions = merged;
}

fn maker_dashboard_params(settings: &Config) -> HashMap<String, String> {
    let prefix = "trading.strategies.maker_quote";
    let mut params = HashMap::new();
    let floats = [
        ("spread_bps", 30.0),
        ("per_quote_notional", 10.0),
        ("requote_threshold_bps", 2.0),
        ("soft_cap_notional", 25.0),
        ("hard_cap_notional", 50.0),
        ("trend_block_bps", 6.0),
        ("min_quote_notional", 5.0),
        ("vol_multiplier", 0.5),
        ("jump_circuit_breaker_bps", 20.0),
        ("min_book_spread_bps", 0.0),
        ("max_book_spread_bps", 40.0),
        ("wide_book_size_mult", 1.0),
        ("max_bbo_imbalance", 0.0),
        ("max_skew_bps", 3.0),
        ("total_quote_budget", 20.0),
    ];
    for (key, default) in floats {
        let value = settings
            .get_float(&format!("{prefix}.{key}"))
            .unwrap_or(default);
        params.insert(key.to_string(), value.to_string());
    }
    let integers = [
        ("requote_cooldown_secs", 5),
        ("ema_period", 20),
        ("vol_window", 24),
        ("cash_open_guard_before_minutes", 5),
        ("cash_open_guard_after_minutes", 20),
        ("circuit_breaker_cooldown_secs", 60),
        ("feature_interval_secs", 60),
        ("join_inside_ticks", 0),
        ("flatten_mid_secs", 6),
        ("flatten_ioc_secs", 15),
    ];
    for (key, default) in integers {
        let value = settings
            .get_int(&format!("{prefix}.{key}"))
            .unwrap_or(default);
        params.insert(key.to_string(), value.to_string());
    }
    let booleans = [
        ("trend_filter", true),
        ("cash_open_guard", true),
        ("flatten_only", false),
    ];
    for (key, default) in booleans {
        let value = settings
            .get_bool(&format!("{prefix}.{key}"))
            .unwrap_or(default);
        params.insert(key.to_string(), value.to_string());
    }
    params
}

fn build_shadow_monitor(settings: &Config) -> Result<Option<Arc<AsyncMutex<ShadowMakerMonitor>>>> {
    if !settings
        .get_bool("trading.shadow_maker.enabled")
        .unwrap_or(true)
    {
        return Ok(None);
    }
    let raw_horizons: Vec<i64> = settings
        .get("trading.shadow_maker.markout_horizons_ms")
        .unwrap_or_else(|_| vec![1_000, 5_000, 30_000]);
    let horizons = raw_horizons
        .into_iter()
        .map(|value| {
            u64::try_from(value).context("shadow markout horizons must be positive integers")
        })
        .collect::<Result<Vec<_>>>()?;
    let monitor = ShadowMakerMonitor::new(ShadowConfig {
        penetration_bps: settings
            .get_float("trading.shadow_maker.penetration_bps")
            .unwrap_or(2.0),
        fill_ratio: settings
            .get_float("trading.shadow_maker.fill_ratio")
            .unwrap_or(0.5),
        markout_horizons_ms: horizons,
        max_recent_fills: settings
            .get_int("trading.shadow_maker.max_recent_fills")
            .unwrap_or(50)
            .max(1) as usize,
    })?;
    Ok(Some(Arc::new(AsyncMutex::new(monitor))))
}

async fn publish_shadow_snapshot(
    monitor: &Option<Arc<AsyncMutex<ShadowMakerMonitor>>>,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    persist: bool,
) -> Result<()> {
    let snapshot = match monitor {
        Some(monitor) => monitor.lock().await.snapshot(unix_millis()),
        None => return Ok(()),
    };
    dashboard_state.write().await.shadow_metrics = Some(serde_json::to_value(&snapshot)?);
    if persist {
        save_shadow_snapshot(&snapshot)?;
    }
    Ok(())
}

fn save_shadow_snapshot(snapshot: &ShadowSnapshot) -> Result<()> {
    let path = dashboard::runtime_paths::data_file(NETWORK, SHADOW_FILE)?;
    let parent = path.parent().context("shadow metrics path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(snapshot)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn build_hft_shadow_lab(
    settings: &Config,
    symbols: &[String],
    market_ids: &HashMap<String, u32>,
    markets: &HashMap<u32, AsterMarket>,
) -> Result<Option<Arc<AsyncMutex<HftShadowLab>>>> {
    if !settings
        .get_bool("trading.hft_shadow.enabled")
        .unwrap_or(false)
    {
        return Ok(None);
    }
    if symbols.len() != 1 {
        bail!("HFT shadow currently requires exactly one configured symbol");
    }
    let market_id = market_ids
        .get(&symbols[0])
        .context("HFT shadow symbol has no market id")?;
    let tick_size = markets
        .get(market_id)
        .context("HFT shadow symbol has no market metadata")?
        .tick_size_value()
        .context("invalid HFT shadow market tick size")?;
    let profiles: Vec<HftProfileConfig> = settings
        .get("trading.hft_shadow.profiles")
        .context("trading.hft_shadow.profiles is required")?;
    Ok(Some(Arc::new(AsyncMutex::new(HftShadowLab::new(
        HftLabConfig {
            tick_size,
            quote_notional: settings
                .get_float("trading.hft_shadow.quote_notional")
                .unwrap_or(20.0),
            penetration_bps: settings
                .get_float("trading.hft_shadow.penetration_bps")
                .unwrap_or(0.2),
            fill_ratio: settings
                .get_float("trading.hft_shadow.fill_ratio")
                .unwrap_or(0.5),
            toxicity_1s_bps: settings
                .get_float("trading.hft_shadow.toxicity_1s_bps")
                .unwrap_or(-2.0),
            toxicity_min_samples: {
                let samples = settings
                    .get_int("trading.hft_shadow.toxicity_min_samples")
                    .unwrap_or(8);
                if samples <= 0 {
                    bail!("HFT shadow toxicity_min_samples must be positive");
                }
                samples as u64
            },
            profiles,
        },
    )?))))
}

async fn publish_hft_shadow_snapshot(
    lab: &Option<Arc<AsyncMutex<HftShadowLab>>>,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    persist: bool,
) -> Result<()> {
    let snapshot = match lab {
        Some(lab) => lab.lock().await.snapshot(unix_millis()),
        None => return Ok(()),
    };
    dashboard_state.write().await.hft_shadow_metrics = Some(serde_json::to_value(&snapshot)?);
    if persist {
        save_hft_shadow_snapshot(&snapshot)?;
    }
    Ok(())
}

fn save_hft_shadow_snapshot(snapshot: &HftLabSnapshot) -> Result<()> {
    let path = dashboard::runtime_paths::data_file(NETWORK, HFT_SHADOW_FILE)?;
    let parent = path
        .parent()
        .context("HFT shadow metrics path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(snapshot)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

pub(crate) async fn run_aster_live_trading(settings: Config) -> Result<()> {
    validate_aster_selection(&settings)?;
    let venue = LiveVenue::AsterMainnet;
    let (loaded, credential_path) = env_profiles::load_aster_credentials(venue)?;
    let credentials = AsterCredentials::new(&loaded.signer_address, &loaded.signer_private_key)
        .map_err(|error| safe_aster_error("invalid Aster API Wallet credentials", &error))?;
    drop(loaded);
    let signer_tail = &credentials.signer()[credentials.signer().len().saturating_sub(8)..];
    info!(
        "🔐 Using Aster API Wallet signer …{} from {}",
        signer_tail,
        credential_path.display()
    );
    let client = Arc::new(AsterClient::authenticated(credentials));
    client
        .sync_server_time()
        .await
        .map_err(|error| safe_aster_error("failed to synchronize Aster server time", &error))?;

    let (symbols, market_ids, markets) = load_markets(&client, &settings).await?;
    let execution_strategy: Arc<RwLock<Box<dyn strategy::Strategy>>> =
        Arc::new(RwLock::new(strategy::create_strategy(&settings)?));
    if !maker_strategy_allowed(execution_strategy.read().await.name()) {
        bail!("Aster live mode requires the maker_quote strategy");
    }
    if let Some(saved) = dashboard::server::PersistentStrategyConfig::load(NETWORK) {
        if !maker_strategy_allowed(&saved.strategy_name) {
            bail!(
                "persisted Aster strategy {} is not maker_quote; refusing startup before cancel-all",
                saved.strategy_name
            );
        }
        let params = sorted_params(&saved.strategy_params);
        strategy::create_strategy_with_params(
            "maker_quote",
            (!params.is_empty()).then_some(params.as_str()),
        )
        .context("persisted Aster maker strategy is invalid; refusing startup before cancel-all")?;
    }
    let mut risk_manager = risk::risk_manager::RiskManager::new(&settings)?;
    let risk_config = risk_manager.get_config();
    let risk_ceilings = RiskCeilings::from_config(&risk_config)?;
    let shadow_monitor = build_shadow_monitor(&settings)?;
    let hft_shadow_lab = build_hft_shadow_lab(&settings, &symbols, &market_ids, &markets)?;
    let position_mode = client
        .position_mode()
        .await
        .map_err(|error| safe_aster_error("failed to query Aster position mode", &error))?;
    if position_mode.dual_side_position {
        bail!("Aster Hedge Mode is enabled; refusing to alter account position mode");
    }
    let account = client
        .account_with_join_margin()
        .await
        .map_err(|error| safe_aster_error("failed to query Aster account", &error))?;
    if !account.can_trade {
        bail!("Aster account canTrade=false");
    }
    let positions = client
        .position_risk(None)
        .await
        .map_err(|error| safe_aster_error("failed to load Aster positions", &error))?;
    let require_isolated_margin = settings
        .get_bool("trading.require_isolated_margin")
        .unwrap_or(true);
    validate_runtime_positions(&positions, &symbols, require_isolated_margin)
        .context("Aster account safety validation failed; refusing to change account settings")?;
    let startup_orders = client
        .open_orders(None)
        .await
        .map_err(|error| safe_aster_error("failed to verify Aster startup open orders", &error))?;
    if !startup_orders.is_empty() {
        let unknown: Vec<&str> = startup_orders
            .iter()
            .filter(|order| !market_ids.contains_key(&order.symbol))
            .map(|order| order.symbol.as_str())
            .collect();
        if !unknown.is_empty() {
            bail!(
                "Aster sub-account has open orders in unconfigured symbols {unknown:?}; refusing to start"
            );
        }
    }
    cancel_all_symbols(&client, &symbols)
        .await
        .context("Aster startup cancel-all failed; refusing to start")?;
    let remaining_orders = fetch_open_orders(&client, &symbols)
        .await
        .context("failed to verify Aster startup cancel-all")?;
    if !remaining_orders.is_empty() {
        bail!("Aster startup cancel-all did not clear every configured symbol");
    }

    let (equity, available_balance, unrealized_pnl) = account_totals(&account)?;
    risk_manager.update_equity(equity);
    let start_paused = settings.get_bool("trading.start_paused").unwrap_or(true);
    let initial_shadow_metrics = match shadow_monitor.as_ref() {
        Some(monitor) => Some(serde_json::to_value(
            monitor.lock().await.snapshot(unix_millis()),
        )?),
        None => Some(serde_json::json!({"enabled": false})),
    };
    let initial_hft_shadow_metrics = match hft_shadow_lab.as_ref() {
        Some(lab) => Some(serde_json::to_value(
            lab.lock().await.snapshot(unix_millis()),
        )?),
        None => Some(serde_json::json!({"enabled": false})),
    };
    let configured_ids: Vec<u32> = symbols.iter().map(|symbol| market_ids[symbol]).collect();
    let dashboard_state = Arc::new(RwLock::new(dashboard::server::DashboardState {
        network_name: NETWORK.to_string(),
        rest_url: MAINNET_REST_URL.to_string(),
        ws_url: MAINNET_WS_URL.to_string(),
        equity,
        available_balance,
        unrealized_pnl,
        strategy_name: "maker_quote".to_string(),
        initial_equity: equity,
        peak_equity: equity,
        equity_history: vec![(Utc::now().timestamp(), equity)],
        active_markets: configured_ids.clone(),
        trading_paused: start_paused,
        available_markets: symbols
            .iter()
            .map(|symbol| (market_ids[symbol], symbol.clone()))
            .collect(),
        positions: one_way_positions(&positions)?,
        strategy_params: maker_dashboard_params(&settings),
        risk_config,
        leverage_limit: risk_manager.max_leverage(),
        shadow_metrics: initial_shadow_metrics,
        hft_shadow_metrics: initial_hft_shadow_metrics,
        quant_agent: dashboard::quant_agent::AgentLedger::load(NETWORK),
        ..dashboard::server::DashboardState::default()
    }));

    let loaded_ledger = AsterLedger::load()?;
    let ledger_existed = loaded_ledger.is_some();
    let mut ledger = loaded_ledger.unwrap_or_default();
    let saved_pnl = ledger
        .pnl_checkpoint
        .clone()
        .or_else(|| dashboard::server::PersistentPnlData::load(NETWORK));
    if let Some(saved) = saved_pnl.as_ref() {
        dashboard_state.write().await.restore_pnl(saved);
    }
    {
        let dashboard = dashboard_state.read().await;
        risk_manager.restore_equity_baseline(dashboard.initial_equity, equity);
        risk_manager.update_daily_pnl(dashboard.daily_realized_pnl);
    }
    restore_risk(&dashboard_state, &mut risk_manager, risk_ceilings).await;
    restore_strategy(&dashboard_state, &execution_strategy).await?;

    reconcile_account_history(
        &client,
        &symbols,
        &dashboard_state,
        &mut ledger,
        !ledger_existed && saved_pnl.is_some(),
    )
    .await?;

    let data_store = Arc::new(RwLock::new(data::storage::MarketDataStore::new()));
    preload_candles(&client, &symbols, &data_store).await?;
    let dashboard_host = settings
        .get_string("dashboard.host")
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let dashboard_port = settings.get_int("dashboard.port").unwrap_or(4028) as u16;
    let dashboard_for_server = dashboard_state.clone();
    tokio::spawn(async move {
        if let Err(error) = dashboard::server::start_with_state(
            &dashboard_host,
            dashboard_port,
            dashboard_for_server,
        )
        .await
        {
            error!("Aster dashboard failed: {error}");
        }
    });

    let listen_key = client
        .create_listen_key()
        .await
        .map_err(|error| safe_aster_error("failed to create Aster listenKey", &error))?;
    let market_url = combined_book_ticker_url(&symbols);
    let user_url = client.user_stream_url(&listen_key.listen_key)?;
    let market_socket = match connect_async(&market_url).await {
        Ok((socket, _)) => socket,
        Err(error) => {
            safety_shutdown(&client, &dashboard_state, &symbols).await;
            warn!("Aster market WebSocket connection failed: {error}");
            bail!("failed to connect Aster market WebSocket");
        }
    };
    let user_socket = match connect_async(&user_url).await {
        Ok((socket, _)) => socket,
        Err(_error) => {
            safety_shutdown(&client, &dashboard_state, &symbols).await;
            warn!("Aster user WebSocket connection failed (listenKey redacted)");
            bail!("failed to connect Aster user WebSocket (listenKey redacted)");
        }
    };
    let (mut market_write, mut market_read) = market_socket.split();
    let (mut user_write, mut user_read) = user_socket.split();

    let mut tracker = OrderTracker::default();
    let mut rest_orders = Vec::<Order>::new();
    let mut positions = positions;
    let mut control_tick = tokio::time::interval(Duration::from_secs(1));
    let mut refresh_tick = tokio::time::interval(Duration::from_secs(10));
    let mut countdown_tick = tokio::time::interval(Duration::from_secs(30));
    let mut keepalive_tick = tokio::time::interval(Duration::from_secs(30 * 60));
    let shadow_persist_secs = settings
        .get_int("trading.shadow_maker.persist_interval_secs")
        .unwrap_or(10)
        .max(1) as u64;
    let mut shadow_persist_tick = tokio::time::interval(Duration::from_secs(shadow_persist_secs));
    let session_started = Instant::now();
    let mut market_parse_errors = 0_u8;
    let mut user_parse_errors = 0_u8;
    let mut last_account_events = HashMap::<String, (u64, u64)>::new();
    let mut position_sync_pending = HashSet::<String>::new();
    let mut accounting_day = Utc::now().date_naive();
    let mut was_paused = dashboard_state.read().await.trading_paused;
    let mut shadow_collecting_ready = was_paused;
    let mut last_active_markets: HashSet<u32> = dashboard_state
        .read()
        .await
        .active_markets
        .iter()
        .copied()
        .collect();
    let max_open_orders = settings
        .get_int("trading.max_open_orders")
        .unwrap_or(8)
        .max(1) as usize;

    info!(
        "✅ Aster live loop connected for {} configured symbols; paused={}",
        symbols.len(),
        was_paused
    );

    loop {
        // The async block is intentional: `?`/`bail!` inside any select branch
        // must become `event_result`, so every failure passes through the
        // pause-and-cancel shutdown below.
        let event_result: Result<()> = async {
            tokio::select! {
            _ = control_tick.tick() => {
                if session_started.elapsed() >= SAFE_SESSION_AGE {
                    bail!("Aster WebSocket session reached safe restart age");
                }
                consume_dashboard_controls(
                    &client,
                    &symbols,
                    &dashboard_state,
                    &execution_strategy,
                    &mut risk_manager,
                    risk_ceilings,
                    &mut tracker,
                    &mut was_paused,
                    &mut last_active_markets,
                ).await?;
                let paused = dashboard_state.read().await.trading_paused;
                shadow_collecting_ready = paused;
                if let Some(shadow) = shadow_monitor.as_ref() {
                    shadow.lock().await.set_collecting(paused, unix_millis());
                }
                if let Some(lab) = hft_shadow_lab.as_ref() {
                    lab.lock().await.set_collecting(paused, unix_millis());
                }
                publish_shadow_snapshot(&shadow_monitor, &dashboard_state, false).await?;
                publish_hft_shadow_snapshot(&hft_shadow_lab, &dashboard_state, false).await?;
                Ok(())
            }
            _ = shadow_persist_tick.tick() => {
                publish_shadow_snapshot(&shadow_monitor, &dashboard_state, true).await?;
                publish_hft_shadow_snapshot(&hft_shadow_lab, &dashboard_state, true).await
            }
            _ = countdown_tick.tick() => {
                for symbol in &symbols {
                    let response = client.countdown_cancel_all(symbol, COUNTDOWN_MS).await
                        .map_err(|error| safe_aster_error(
                            &format!("Aster countdownCancelAll refresh failed for {symbol}"),
                            &error,
                        ))?;
                    if response.countdown_time != COUNTDOWN_MS {
                        bail!("Aster countdownCancelAll returned unexpected timeout for {symbol}");
                    }
                }
                Ok(())
            }
            _ = keepalive_tick.tick() => {
                client.keepalive_listen_key().await
                    .map_err(|error| safe_aster_error("Aster listenKey keepalive failed", &error))?;
                Ok(())
            }
            _ = refresh_tick.tick() => {
                let today = Utc::now().date_naive();
                if today != accounting_day {
                    let today_key = today.format("%Y-%m-%d").to_string();
                    let mut dashboard = dashboard_state.write().await;
                    dashboard.daily_realized_pnl =
                        dashboard.daily_pnl_map.get(&today_key).copied().unwrap_or(0.0);
                    dashboard.daily_funding_pnl = 0.0;
                    dashboard.save_pnl();
                    accounting_day = today;
                }
                let (new_account, new_positions, new_orders) = tokio::try_join!(
                    async {
                        client.account_with_join_margin().await
                            .map_err(|error| safe_aster_error("Aster account refresh failed", &error))
                    },
                    async {
                        client.position_risk(None).await
                            .map_err(|error| safe_aster_error("Aster position refresh failed", &error))
                    },
                    async { fetch_open_orders(&client, &symbols).await },
                )?;
                if !new_account.can_trade {
                    bail!("Aster account changed to canTrade=false");
                }
                validate_runtime_positions(
                    &new_positions,
                    &symbols,
                    require_isolated_margin,
                )?;
                merge_rest_positions(
                    &mut positions,
                    new_positions,
                    &mut last_account_events,
                );
                for position in &positions {
                    position_sync_pending.remove(&position.symbol);
                }
                rest_orders = new_orders;
                tracker.reconcile_open_orders(&rest_orders);
                update_dashboard_account(
                    &dashboard_state,
                    &new_account,
                    &positions,
                    &rest_orders,
                    &mut risk_manager,
                ).await?;
                reconcile_account_history(
                    &client,
                    &symbols,
                    &dashboard_state,
                    &mut ledger,
                    false,
                ).await?;
                enforce_risk_gates(
                    &client,
                    &symbols,
                    &markets,
                    &market_ids,
                    &dashboard_state,
                    &positions,
                    &mut risk_manager,
                    &mut tracker,
                ).await
            }
            message = market_read.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match AsterWsEvent::parse(&text) {
                            Ok(AsterWsEvent::Bbo(book)) => {
                                market_parse_errors = 0;
                                if let Some(shadow) = shadow_monitor.as_ref() {
                                    let shadow_symbol = book.symbol.to_ascii_uppercase();
                                    let (paused, active_markets) = {
                                        let dashboard = dashboard_state.read().await;
                                        (
                                            dashboard.trading_paused,
                                            dashboard.active_markets.clone(),
                                        )
                                    };
                                    let market_active = market_ids
                                        .get(&shadow_symbol)
                                        .is_some_and(|id| active_markets.contains(id));
                                    let position_pending =
                                        position_sync_pending.contains(&shadow_symbol);
                                    let received_at_ms = unix_millis();
                                    let mut monitor = shadow.lock().await;
                                    let collect = shadow_collecting_ready && paused;
                                    monitor.set_collecting(collect, received_at_ms);
                                    if collect && market_active && !position_pending {
                                        monitor.observe_bbo(
                                            &shadow_symbol,
                                            parse_f64(&book.bid_price)?,
                                            parse_f64(&book.ask_price)?,
                                            book.event_time,
                                            received_at_ms,
                                        );
                                    } else {
                                        monitor.clear_symbol(&shadow_symbol, received_at_ms);
                                    }
                                }
                                if let Some(lab) = hft_shadow_lab.as_ref() {
                                    let shadow_symbol = book.symbol.to_ascii_uppercase();
                                    let (paused, active_markets) = {
                                        let dashboard = dashboard_state.read().await;
                                        (
                                            dashboard.trading_paused,
                                            dashboard.active_markets.clone(),
                                        )
                                    };
                                    let market_active = market_ids
                                        .get(&shadow_symbol)
                                        .is_some_and(|id| active_markets.contains(id));
                                    let position_pending =
                                        position_sync_pending.contains(&shadow_symbol);
                                    let received_at_ms = unix_millis();
                                    let mut lab = lab.lock().await;
                                    let collect = shadow_collecting_ready && paused;
                                    lab.set_collecting(collect, received_at_ms);
                                    if collect && market_active && !position_pending {
                                        lab.observe_bbo(
                                            &shadow_symbol,
                                            parse_f64(&book.bid_price)?,
                                            parse_f64(&book.ask_price)?,
                                            book.event_time,
                                            received_at_ms,
                                        );
                                    } else {
                                        lab.clear_symbol(&shadow_symbol, received_at_ms);
                                    }
                                }
                                if position_sync_pending
                                    .contains(&book.symbol.to_ascii_uppercase())
                                {
                                    Ok(())
                                } else {
                                    handle_bbo(
                                        book,
                                        &client,
                                        &markets,
                                        &market_ids,
                                        &dashboard_state,
                                        &data_store,
                                        &execution_strategy,
                                        &mut risk_manager,
                                        &positions,
                                        &position_sync_pending,
                                        shadow_monitor.as_ref(),
                                        &mut rest_orders,
                                        &mut tracker,
                                        max_open_orders,
                                    ).await
                                }
                            }
                            Ok(AsterWsEvent::Depth(depth)) => {
                                market_parse_errors = 0;
                                if let Some(shadow) = shadow_monitor.as_ref() {
                                    let shadow_symbol = depth.symbol.to_ascii_uppercase();
                                    let (paused, active_markets) = {
                                        let dashboard = dashboard_state.read().await;
                                        (
                                            dashboard.trading_paused,
                                            dashboard.active_markets.clone(),
                                        )
                                    };
                                    let market_active = market_ids
                                        .get(&shadow_symbol)
                                        .is_some_and(|id| active_markets.contains(id));
                                    if shadow_collecting_ready
                                        && paused
                                        && market_active
                                        && !position_sync_pending.contains(&shadow_symbol)
                                    {
                                        let bids = parse_depth_levels(&depth.bids)?;
                                        let asks = parse_depth_levels(&depth.asks)?;
                                        shadow.lock().await.observe_depth(
                                            &shadow_symbol,
                                            &bids,
                                            &asks,
                                            depth.event_time,
                                            unix_millis(),
                                        );
                                    }
                                }
                                if let Some(lab) = hft_shadow_lab.as_ref() {
                                    let shadow_symbol = depth.symbol.to_ascii_uppercase();
                                    let (paused, active_markets) = {
                                        let dashboard = dashboard_state.read().await;
                                        (
                                            dashboard.trading_paused,
                                            dashboard.active_markets.clone(),
                                        )
                                    };
                                    let market_active = market_ids
                                        .get(&shadow_symbol)
                                        .is_some_and(|id| active_markets.contains(id));
                                    if shadow_collecting_ready
                                        && paused
                                        && market_active
                                        && !position_sync_pending.contains(&shadow_symbol)
                                    {
                                        let bids = parse_depth_levels(&depth.bids)?;
                                        let asks = parse_depth_levels(&depth.asks)?;
                                        lab.lock().await.observe_depth(
                                            &shadow_symbol,
                                            &bids,
                                            &asks,
                                            depth.event_time,
                                            unix_millis(),
                                        );
                                    }
                                }
                                Ok(())
                            }
                            Ok(_) => {
                                market_parse_errors = 0;
                                Ok(())
                            }
                            Err(error) => {
                                market_parse_errors = market_parse_errors.saturating_add(1);
                                warn!("Aster market WS parse error ({market_parse_errors}/{PARSE_ERROR_LIMIT}): {error}");
                                if market_parse_errors >= PARSE_ERROR_LIMIT {
                                    bail!("consecutive Aster market WS parse failures");
                                }
                                Ok(())
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        market_write.send(Message::Pong(payload)).await
                            .context("failed to answer Aster market WS ping")
                    }
                    Some(Ok(Message::Close(_))) | None => bail!("Aster market WebSocket disconnected"),
                    Some(Err(error)) => Err(error).context("Aster market WebSocket failed"),
                    Some(Ok(_)) => Ok(()),
                }
            }
            message = user_read.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match AsterWsEvent::parse(&text) {
                            Ok(AsterWsEvent::Order(update)) => {
                                user_parse_errors = 0;
                                tracker.apply_ws_order(&update);
                                if update.order.execution_type.eq_ignore_ascii_case("TRADE")
                                    || parse_f64(&update.order.last_filled_quantity)
                                        .is_ok_and(|quantity| quantity > 0.0)
                                {
                                    position_sync_pending
                                        .insert(update.order.symbol.to_ascii_uppercase());
                                }
                                if is_terminal_status(&update.order.status) {
                                    rest_orders.retain(|order| {
                                        order.order_id != update.order.order_id
                                            && order.client_order_id
                                                != update.order.client_order_id
                                    });
                                }
                                Ok(())
                            }
                            Ok(AsterWsEvent::ListenKeyExpired { .. }) => {
                                bail!("Aster listenKey expired");
                            }
                            Ok(AsterWsEvent::Account(update)) => {
                                user_parse_errors = 0;
                                let last_prices = dashboard_state.read().await.last_prices.clone();
                                apply_ws_account_update(
                                    &mut positions,
                                    &update,
                                    &mut last_account_events,
                                    &last_prices,
                                )?;
                                for changed in &update.positions {
                                    position_sync_pending
                                        .remove(&changed.symbol.to_ascii_uppercase());
                                }
                                validate_runtime_positions(
                                    &positions,
                                    &symbols,
                                    require_isolated_margin,
                                )?;
                                dashboard_state.write().await.positions =
                                    one_way_positions(&positions)?;
                                Ok(())
                            }
                            Ok(_) => {
                                user_parse_errors = 0;
                                Ok(())
                            }
                            Err(error) => {
                                user_parse_errors = user_parse_errors.saturating_add(1);
                                warn!("Aster user WS parse error ({user_parse_errors}/{PARSE_ERROR_LIMIT}): {error}");
                                if user_parse_errors >= PARSE_ERROR_LIMIT {
                                    bail!("consecutive Aster user WS parse failures");
                                }
                                Ok(())
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        user_write.send(Message::Pong(payload)).await
                            .context("failed to answer Aster user WS ping")
                    }
                    Some(Ok(Message::Close(_))) | None => bail!("Aster user WebSocket disconnected"),
                    Some(Err(_error)) => bail!("Aster user WebSocket failed (listenKey redacted)"),
                    Some(Ok(_)) => Ok(()),
                }
            }
            }
        }
        .await;
        if let Err(error) = event_result {
            if let Err(shadow_error) =
                publish_shadow_snapshot(&shadow_monitor, &dashboard_state, true).await
            {
                warn!("failed to persist final Aster shadow snapshot: {shadow_error}");
            }
            if let Err(shadow_error) =
                publish_hft_shadow_snapshot(&hft_shadow_lab, &dashboard_state, true).await
            {
                warn!("failed to persist final Aster HFT shadow snapshot: {shadow_error}");
            }
            safety_shutdown(&client, &dashboard_state, &symbols).await;
            let _ = client.close_listen_key().await;
            return Err(error).context("Aster live loop stopped safely");
        }
    }
}

fn validate_aster_selection(settings: &Config) -> Result<()> {
    let environment = settings
        .get_string("exchange.environment")
        .context("exchange.environment is required for Aster")?;
    if !environment.eq_ignore_ascii_case("mainnet") {
        bail!("Aster live mode supports only exchange.environment=mainnet");
    }
    let selected = env_profiles::selected_venue();
    if selected != LiveVenue::AsterMainnet {
        bail!("selected venue {selected} does not match AsterMainnet");
    }
    Ok(())
}

async fn load_markets(
    client: &AsterClient,
    settings: &Config,
) -> Result<(Vec<String>, HashMap<String, u32>, HashMap<u32, AsterMarket>)> {
    let mut symbols: Vec<String> = settings
        .get("trading.symbols")
        .context("trading.symbols is required for Aster live mode")?;
    symbols.iter_mut().for_each(|symbol| {
        *symbol = symbol.trim().to_ascii_uppercase();
    });
    symbols.sort();
    symbols.dedup();
    if symbols.is_empty() {
        bail!("trading.symbols must not be empty");
    }
    let exchange_info = client
        .exchange_info()
        .await
        .context("failed to load Aster exchangeInfo")?;
    let mut market_ids = HashMap::new();
    let mut markets = HashMap::new();
    for (index, symbol) in symbols.iter().enumerate() {
        let exchange_symbol = exchange_info
            .symbols
            .iter()
            .find(|candidate| candidate.symbol.eq_ignore_ascii_case(symbol))
            .with_context(|| format!("configured Aster symbol {symbol} is missing"))?;
        if exchange_symbol.status != "TRADING"
            || exchange_symbol.contract_type != "PERPETUAL"
            || exchange_symbol.quote_asset != "USDT"
            || !exchange_symbol
                .order_types
                .iter()
                .any(|value| value.eq_ignore_ascii_case("LIMIT"))
            || !exchange_symbol
                .time_in_force
                .iter()
                .any(|value| value.eq_ignore_ascii_case("GTX"))
        {
            bail!("Aster symbol {symbol} must be TRADING PERPETUAL/USDT with LIMIT+GTX support");
        }
        let market_id = u32::try_from(index).context("too many configured Aster symbols")?;
        market_ids.insert(symbol.clone(), market_id);
        markets.insert(
            market_id,
            AsterMarket::try_from(exchange_symbol)
                .with_context(|| format!("invalid Aster market filters for {symbol}"))?,
        );
    }
    Ok((symbols, market_ids, markets))
}

async fn preload_candles(
    client: &AsterClient,
    symbols: &[String],
    store: &Arc<RwLock<data::storage::MarketDataStore>>,
) -> Result<()> {
    for symbol in symbols {
        let candles = client
            .klines(symbol, "1h", None, None, Some(100))
            .await
            .with_context(|| format!("failed to preload Aster candles for {symbol}"))?;
        let mut store = store.write().await;
        for candle in candles {
            let timestamp = Utc
                .timestamp_millis_opt(candle.open_time as i64)
                .single()
                .context("Aster kline timestamp is out of range")?;
            store.add_candle(lighter::types::Candlestick {
                timestamp,
                open: parse_f64(&candle.open)?,
                high: parse_f64(&candle.high)?,
                low: parse_f64(&candle.low)?,
                close: parse_f64(&candle.close)?,
                volume: parse_f64(&candle.volume)?,
                symbol: symbol.clone(),
            });
        }
    }
    Ok(())
}

pub(crate) fn combined_book_ticker_url(symbols: &[String]) -> String {
    let streams = symbols
        .iter()
        .flat_map(|symbol| {
            let symbol = symbol.to_ascii_lowercase();
            [
                format!("{symbol}@bookTicker"),
                format!("{symbol}@depth20@100ms"),
            ]
        })
        .collect::<Vec<_>>()
        .join("/");
    format!("{MAINNET_WS_URL}/stream?streams={streams}")
}

async fn restore_strategy(
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    execution_strategy: &Arc<RwLock<Box<dyn strategy::Strategy>>>,
) -> Result<()> {
    let Some(saved) = dashboard::server::PersistentStrategyConfig::load(NETWORK) else {
        return Ok(());
    };
    if !maker_strategy_allowed(&saved.strategy_name) {
        let mut dashboard = dashboard_state.write().await;
        dashboard.trading_paused = true;
        warn!(
            "persisted Aster strategy {} is not maker_quote; ignored and trading remains paused",
            saved.strategy_name
        );
        return Ok(());
    }
    let params = sorted_params(&saved.strategy_params);
    let replacement = strategy::create_strategy_with_params(
        "maker_quote",
        (!params.is_empty()).then_some(&params),
    )?;
    *execution_strategy.write().await = replacement;
    let mut dashboard = dashboard_state.write().await;
    dashboard.strategy_name = "maker_quote".to_string();
    dashboard.strategy_params = saved.strategy_params;
    Ok(())
}

async fn restore_risk(
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    risk_manager: &mut risk::risk_manager::RiskManager,
    ceilings: RiskCeilings,
) {
    let Some(saved) = dashboard::server::PersistentRiskConfig::load(NETWORK) else {
        return;
    };
    apply_risk_json(risk_manager, &saved.risk_config, ceilings);
    let mut dashboard = dashboard_state.write().await;
    dashboard.risk_config = risk_manager.get_config();
    dashboard.leverage_limit = saved.leverage_limit.min(risk_manager.max_leverage());
}

fn apply_risk_json(
    manager: &mut risk::risk_manager::RiskManager,
    value: &serde_json::Value,
    ceilings: RiskCeilings,
) {
    manager.update_params(
        bounded_json_number(value, "max_drawdown_pct", 0.1, ceilings.max_drawdown_pct),
        bounded_json_number(
            value,
            "daily_loss_limit_pct",
            0.1,
            ceilings.daily_loss_limit_pct,
        ),
        bounded_json_number(value, "max_leverage", 1.0, ceilings.max_leverage),
        bounded_json_number(
            value,
            "position_stop_loss_pct",
            0.1,
            ceilings.position_stop_loss_pct,
        ),
        bounded_json_number(
            value,
            "position_take_profit_pct",
            0.1,
            ceilings.position_take_profit_pct,
        ),
    );
}

fn json_number(value: &serde_json::Value, key: &str) -> Option<f64> {
    value.get(key).and_then(serde_json::Value::as_f64)
}

fn required_json_number(value: &serde_json::Value, key: &str) -> Result<f64> {
    let number = json_number(value, key).with_context(|| format!("missing risk setting {key}"))?;
    if !number.is_finite() || number <= 0.0 {
        bail!("invalid risk setting {key}");
    }
    Ok(number)
}

fn bounded_json_number(
    value: &serde_json::Value,
    key: &str,
    minimum: f64,
    maximum: f64,
) -> Option<f64> {
    json_number(value, key).map(|number| {
        if !number.is_finite() {
            minimum
        } else {
            number.clamp(minimum, maximum)
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn consume_dashboard_controls(
    client: &AsterClient,
    symbols: &[String],
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    execution_strategy: &Arc<RwLock<Box<dyn strategy::Strategy>>>,
    risk_manager: &mut risk::risk_manager::RiskManager,
    risk_ceilings: RiskCeilings,
    tracker: &mut OrderTracker,
    was_paused: &mut bool,
    last_active_markets: &mut HashSet<u32>,
) -> Result<()> {
    let (cancel_requested, risk_update, strategy_update, active_markets) = {
        let mut dashboard = dashboard_state.write().await;
        (
            std::mem::take(&mut dashboard.cancel_all_requested),
            dashboard.risk_update_requested.take(),
            std::mem::take(&mut dashboard.strategy_config_changed),
            dashboard.active_markets.clone(),
        )
    };
    if let Some(update) = risk_update {
        apply_risk_json(risk_manager, &update, risk_ceilings);
        let mut dashboard = dashboard_state.write().await;
        dashboard.risk_config = risk_manager.get_config();
        dashboard.leverage_limit = dashboard.leverage_limit.min(risk_manager.max_leverage());
        dashboard::server::PersistentRiskConfig::save(&dashboard);
    }
    if strategy_update {
        let (name, params) = {
            let dashboard = dashboard_state.read().await;
            (
                dashboard.strategy_name.clone(),
                dashboard.strategy_params.clone(),
            )
        };
        if !maker_strategy_allowed(&name) {
            let mut dashboard = dashboard_state.write().await;
            dashboard.trading_paused = true;
            dashboard.strategy_name = "maker_quote".to_string();
            warn!("rejected non-maker Aster strategy update {name}; trading paused");
        } else {
            let params_string = sorted_params(&params);
            match strategy::create_strategy_with_params(
                "maker_quote",
                (!params_string.is_empty()).then_some(params_string.as_str()),
            ) {
                Ok(replacement) => {
                    *execution_strategy.write().await = replacement;
                    let dashboard = dashboard_state.read().await;
                    dashboard::server::PersistentStrategyConfig::save(&dashboard);
                }
                Err(error) => {
                    dashboard_state.write().await.trading_paused = true;
                    warn!("invalid Aster maker strategy update: {error}; trading paused");
                }
            }
        }
    }
    let configured: HashSet<u32> = dashboard_state
        .read()
        .await
        .available_markets
        .iter()
        .map(|(id, _)| *id)
        .collect();
    if active_markets.iter().any(|id| !configured.contains(id)) {
        dashboard_state.write().await.trading_paused = true;
        bail!("dashboard selected an unknown Aster active market");
    }
    let active_now: HashSet<u32> = active_markets.iter().copied().collect();
    if active_now != *last_active_markets {
        let available = dashboard_state.read().await.available_markets.clone();
        for market_id in last_active_markets.difference(&active_now) {
            let symbol = available
                .iter()
                .find_map(|(id, symbol)| (id == market_id).then_some(symbol))
                .context("deactivated Aster market has no symbol mapping")?;
            client.cancel_all_orders(symbol).await.map_err(|error| {
                safe_aster_error(
                    &format!("failed to cancel deactivated Aster market {symbol}"),
                    &error,
                )
            })?;
        }
        *last_active_markets = active_now;
    }
    let now_paused = dashboard_state.read().await.trading_paused;
    if cancel_requested || (now_paused && !*was_paused) {
        cancel_all_symbols(client, symbols).await?;
        let orders = fetch_open_orders(client, symbols)
            .await
            .context("Aster cancel-all verification failed")?;
        tracker.reconcile_open_orders(&orders);
        if orders.iter().any(|order| symbols.contains(&order.symbol)) {
            bail!("Aster cancel-all verification found configured orders still open");
        }
    }
    if now_paused && !*was_paused {
        info!("Aster trading paused; cancel-all accepted");
    }
    *was_paused = now_paused;
    Ok(())
}

fn sorted_params(params: &HashMap<String, String>) -> String {
    let mut values: Vec<_> = params.iter().collect();
    values.sort_by(|left, right| left.0.cmp(right.0));
    values
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[allow(clippy::too_many_arguments)]
async fn handle_bbo(
    book: multi_venue_quant_bot::aster::BookTicker,
    client: &AsterClient,
    markets: &HashMap<u32, AsterMarket>,
    market_ids: &HashMap<String, u32>,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    data_store: &Arc<RwLock<data::storage::MarketDataStore>>,
    execution_strategy: &Arc<RwLock<Box<dyn strategy::Strategy>>>,
    risk_manager: &mut risk::risk_manager::RiskManager,
    positions: &[PositionRisk],
    position_sync_pending: &HashSet<String>,
    shadow_monitor: Option<&Arc<AsyncMutex<ShadowMakerMonitor>>>,
    rest_orders: &mut Vec<Order>,
    tracker: &mut OrderTracker,
    max_open_orders: usize,
) -> Result<()> {
    let symbol = book.symbol.to_ascii_uppercase();
    let market_id = *market_ids
        .get(&symbol)
        .with_context(|| format!("Aster BBO referenced unconfigured symbol {symbol}"))?;
    let bid = parse_f64(&book.bid_price)?;
    let ask = parse_f64(&book.ask_price)?;
    if !(bid > 0.0 && ask > bid) {
        bail!("invalid Aster BBO for {symbol}");
    }
    let bid_quantity = parse_f64(&book.bid_quantity)?;
    let ask_quantity = parse_f64(&book.ask_quantity)?;
    data_store
        .write()
        .await
        .update_order_book(lighter::types::OrderBook {
            symbol: symbol.clone(),
            market_id,
            bids: vec![lighter::types::PriceLevel {
                price: bid,
                quantity: bid_quantity,
            }],
            asks: vec![lighter::types::PriceLevel {
                price: ask,
                quantity: ask_quantity,
            }],
            timestamp: Utc::now(),
        });
    dashboard_state
        .write()
        .await
        .last_prices
        .insert(symbol.clone(), (bid + ask) / 2.0);
    let mut snapshot = data_store.read().await.get_snapshot();
    for position in positions {
        if !position.position_side.eq_ignore_ascii_case("BOTH") {
            bail!("Aster position mode changed away from one-way");
        }
        let quantity = parse_f64(&position.position_amt)?;
        snapshot.positions.insert(position.symbol.clone(), quantity);
        snapshot
            .position_entry_prices
            .insert(position.symbol.clone(), parse_f64(&position.entry_price)?);
    }
    snapshot.positions_authoritative = true;
    for local in tracker.by_strategy_key.values() {
        snapshot.open_orders.push(lighter::types::OpenOrderRef {
            symbol: local.symbol.clone(),
            client_id: Some(local.strategy_key.clone()),
            side: to_lighter_side(local.side),
            price: local.price,
            quantity: local.quantity,
            status: format!("{:?}", local.status),
        });
    }
    let shadow_paused = dashboard_state.read().await.trading_paused;
    if shadow_paused {
        if let Some(shadow) = shadow_monitor {
            let monitor = shadow.lock().await;
            for (symbol, quantity) in monitor.virtual_positions() {
                *snapshot.positions.entry(symbol.clone()).or_default() += quantity;
            }
            snapshot.open_orders.extend(monitor.open_order_refs());
        }
    }
    snapshot.open_orders_authoritative = true;
    let evaluation_started = Instant::now();
    let evaluated = execution_strategy.read().await.evaluate(&snapshot).await?;
    if let Some(shadow) = shadow_monitor {
        shadow.lock().await.record_strategy_eval(
            evaluation_started
                .elapsed()
                .as_micros()
                .min(u64::MAX as u128) as u64,
        );
    }
    let Some(signals) = evaluated else {
        return Ok(());
    };
    for signal in signals {
        let position_pending = position_sync_pending.contains(&signal.symbol.to_ascii_uppercase());
        let (paused, active) = {
            let dashboard = dashboard_state.read().await;
            (dashboard.trading_paused, dashboard.active_markets.clone())
        };
        if shadow_paused
            && paused
            && (signal.action == lighter::types::SignalAction::Cancel
                || (!position_pending && signal.post_only && active.contains(&signal.market_id)))
        {
            if let Some(shadow) = shadow_monitor {
                shadow.lock().await.apply_signal(&signal, unix_millis());
            }
        }
        if position_pending {
            continue;
        }
        if !signal_allowed(
            signal.action == lighter::types::SignalAction::Cancel,
            paused,
            &active,
            signal.market_id,
            signal.risk_reducing,
            signal.post_only,
        ) {
            continue;
        }
        let signal_market_id = signal.market_id;
        process_signal(
            client,
            dashboard_state,
            markets
                .get(&signal_market_id)
                .context("Aster strategy emitted unknown market id")?,
            signal_market_id,
            signal,
            positions,
            rest_orders,
            tracker,
            risk_manager,
            max_open_orders,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn process_signal(
    client: &AsterClient,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    market: &AsterMarket,
    market_id: u32,
    mut signal: lighter::types::TradeSignal,
    positions: &[PositionRisk],
    rest_orders: &mut Vec<Order>,
    tracker: &mut OrderTracker,
    risk_manager: &mut risk::risk_manager::RiskManager,
    max_open_orders: usize,
) -> Result<()> {
    let strategy_key = signal
        .client_id
        .clone()
        .unwrap_or_else(|| format!("mq_{}_{}", signal.symbol, side_label(signal.side)));
    if signal.action == lighter::types::SignalAction::Cancel {
        if let Some(existing) = tracker.by_strategy_key.get(&strategy_key).cloned() {
            cancel_tracked_order(client, tracker, rest_orders, &existing).await?;
        }
        return Ok(());
    }
    if !signal.post_only && !signal.risk_reducing {
        bail!("Aster rejected a non-maker, non-risk-reducing placement intent");
    }
    let existing = tracker.by_strategy_key.get(&strategy_key).cloned();
    if let Some(existing) = existing.as_ref() {
        match quote_replace_decision(existing, signal.price, signal.post_only) {
            QuoteReplaceDecision::Noop => return Ok(()),
            QuoteReplaceDecision::BlockedUnresolved => {
                bail!("Aster quote replacement blocked by unresolved prior order");
            }
            QuoteReplaceDecision::Amend | QuoteReplaceDecision::CancelThenWait => {}
        }
    }
    if existing.is_none() && !signal.risk_reducing {
        let count = rest_orders.len() + tracker.local_open_count_not_in_rest(rest_orders);
        if count >= max_open_orders {
            return Ok(());
        }
    }
    let side = from_lighter_side(signal.side);
    if signal.risk_reducing {
        let held = positions
            .iter()
            .find(|position| position.symbol == signal.symbol)
            .map(|position| parse_f64(&position.position_amt))
            .transpose()?
            .unwrap_or(0.0);
        let closes =
            (held > 0.0 && side == OrderSide::Sell) || (held < 0.0 && side == OrderSide::Buy);
        if !closes {
            return Ok(());
        }
        signal.quantity = signal.quantity.min(held.abs());
    }
    let exposure = calculate_exposure(
        &exposure_input(positions, rest_orders, tracker)?,
        &signal.symbol,
    );
    if !risk_manager
        .check_signal_with_exposure(&signal, exposure)
        .await?
    {
        return Ok(());
    }
    let quantized = if signal.risk_reducing && !signal.post_only {
        market.quantize_reduce_only(
            &decimal(signal.price),
            &decimal(signal.quantity),
            opposite(side),
        )?
    } else if signal.risk_reducing {
        market.quantize_reduce_only(&decimal(signal.price), &decimal(signal.quantity), side)?
    } else {
        market.quantize_maker(&decimal(signal.price), &decimal(signal.quantity), side)?
    };
    if let Some(existing) = existing {
        let quantized_price = parse_f64(&quantized.price)?;
        match quote_replace_decision(&existing, quantized_price, signal.post_only) {
            QuoteReplaceDecision::Noop => return Ok(()),
            QuoteReplaceDecision::Amend => {
                return amend_tracked_order(
                    client,
                    dashboard_state,
                    market_id,
                    &existing,
                    &quantized.price,
                    &quantized.quantity,
                    rest_orders,
                    tracker,
                    signal.post_only,
                )
                .await;
            }
            QuoteReplaceDecision::CancelThenWait => {
                cancel_tracked_order(client, tracker, rest_orders, &existing).await?;
                return Ok(());
            }
            QuoteReplaceDecision::BlockedUnresolved => {
                bail!("Aster quote replacement reached an impossible state");
            }
        }
    }
    let exchange_client_id = unique_client_id(&strategy_key, market_id, side);
    let mut request = NewOrderRequest::maker_limit(
        &signal.symbol,
        side,
        &quantized.price,
        &quantized.quantity,
        Some(exchange_client_id.clone()),
    );
    request.position_side = Some("BOTH".to_string());
    if signal.risk_reducing {
        request.reduce_only = Some(true);
    }
    if signal.risk_reducing && !signal.post_only {
        request.time_in_force = Some("IOC".to_string());
        request.response_type = Some("RESULT".to_string());
    }
    // Hold the read guard across a maker submission so the pause endpoint
    // cannot return success while a new entry is still being sent.
    let placement_guard = if signal.post_only {
        let dashboard = dashboard_state.read().await;
        if dashboard.trading_paused || !dashboard.active_markets.contains(&market_id) {
            return Ok(());
        }
        Some(dashboard)
    } else {
        None
    };
    tracker.insert(LocalOrder {
        strategy_key: strategy_key.clone(),
        exchange_client_id: exchange_client_id.clone(),
        order_id: None,
        symbol: signal.symbol.clone(),
        side,
        price: parse_f64(&quantized.price)?,
        quantity: parse_f64(&quantized.quantity)?,
        status: LocalOrderStatus::Pending,
        last_event_time: 0,
        last_transaction_time: 0,
    });
    match client.place_order(&request).await {
        Ok(order) => {
            apply_rest_order_result(tracker, &strategy_key, &order);
            if is_open_status(&order.status) && rest_order_is_complete(&order) {
                rest_orders.push(order);
            }
            Ok(())
        }
        Err(error) => {
            if submission_failure_decision(&error) == SubmissionFailureDecision::Reject {
                tracker.remove(&strategy_key);
                return Err(safe_aster_error("Aster order was rejected", &error));
            }
            if let Some(local) = tracker.by_strategy_key.get_mut(&strategy_key) {
                local.status = LocalOrderStatus::Unknown;
            }
            reconcile_unknown_submission(
                client,
                &signal.symbol,
                &exchange_client_id,
                &strategy_key,
                rest_orders,
                tracker,
            )
            .await
            .map_err(|reconcile_error| {
                anyhow::anyhow!(
                    "{}; {}",
                    safe_aster_error("Aster order status remained unknown", &error),
                    reconcile_error
                )
            })
        }
    }?;
    drop(placement_guard);
    Ok(())
}

fn exposure_input(
    positions: &[PositionRisk],
    rest_orders: &[Order],
    tracker: &OrderTracker,
) -> Result<ExposureInput> {
    Ok(ExposureInput {
        positions: positions
            .iter()
            .map(|position| {
                Ok((
                    position.symbol.clone(),
                    parse_f64(&position.position_amt)?,
                    parse_f64(&position.mark_price)?,
                ))
            })
            .collect::<Result<_>>()?,
        rest_orders: rest_orders
            .iter()
            .map(|order| {
                Ok((
                    order.symbol.clone(),
                    order.client_order_id.clone(),
                    Some(order.order_id),
                    parse_order_side(&order.side)?,
                    parse_f64(&order.price)?,
                    parse_f64(&order.orig_qty)?,
                ))
            })
            .collect::<Result<_>>()?,
        local_orders: tracker.by_strategy_key.values().cloned().collect(),
    })
}

async fn cancel_tracked_order(
    client: &AsterClient,
    tracker: &mut OrderTracker,
    rest_orders: &mut Vec<Order>,
    existing: &LocalOrder,
) -> Result<()> {
    match client
        .cancel_order(
            &existing.symbol,
            existing.order_id,
            Some(&existing.exchange_client_id),
        )
        .await
    {
        Ok(order) if is_terminal_status(&order.status) => {
            tracker.remove(&existing.strategy_key);
            remove_rest_order(rest_orders, existing);
            Ok(())
        }
        Ok(order) => {
            apply_rest_order_result(tracker, &existing.strategy_key, &order);
            Ok(())
        }
        Err(cancel_error) => {
            match client
                .query_order(
                    &existing.symbol,
                    existing.order_id,
                    Some(&existing.exchange_client_id),
                )
                .await
            {
                Ok(order) if is_terminal_status(&order.status) => {
                    tracker.remove(&existing.strategy_key);
                    remove_rest_order(rest_orders, existing);
                    Ok(())
                }
                Ok(order) => {
                    apply_rest_order_result(tracker, &existing.strategy_key, &order);
                    warn!(
                        "{}; order remains live and replacement is blocked",
                        safe_aster_error("Aster cancel failed", &cancel_error)
                    );
                    Ok(())
                }
                Err(query_error) => Err(anyhow::anyhow!(
                    "{}; {}",
                    safe_aster_error("Aster cancel failed", &cancel_error),
                    safe_aster_error("Aster status query failed", &query_error)
                )),
            }
        }
    }
}

fn remove_rest_order(rest_orders: &mut Vec<Order>, existing: &LocalOrder) {
    rest_orders.retain(|order| {
        order.client_order_id != existing.exchange_client_id
            && existing.order_id != Some(order.order_id)
    });
}

fn upsert_rest_order(rest_orders: &mut Vec<Order>, existing: &LocalOrder, order: Order) {
    remove_rest_order(rest_orders, existing);
    if is_open_status(&order.status) && rest_order_is_complete(&order) {
        rest_orders.push(order);
    }
}

#[allow(clippy::too_many_arguments)]
async fn amend_tracked_order(
    client: &AsterClient,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    market_id: u32,
    existing: &LocalOrder,
    quantized_price: &str,
    quantized_quantity: &str,
    rest_orders: &mut Vec<Order>,
    tracker: &mut OrderTracker,
    post_only: bool,
) -> Result<()> {
    let placement_guard = if post_only {
        let dashboard = dashboard_state.read().await;
        if dashboard.trading_paused || !dashboard.active_markets.contains(&market_id) {
            return Ok(());
        }
        Some(dashboard)
    } else {
        None
    };
    let request = ModifyOrderRequest::new(
        &existing.symbol,
        existing.order_id,
        Some(existing.exchange_client_id.clone()),
        quantized_price,
        quantized_quantity,
    )
    .map_err(|error| safe_aster_error("invalid Aster modify request", &error))?;
    let result = match client.modify_order(&request).await {
        Ok(order) => {
            apply_rest_order_result(tracker, &existing.strategy_key, &order);
            upsert_rest_order(rest_orders, existing, order);
            Ok(())
        }
        Err(error) if modify_order_is_gone(&error) => {
            tracker.remove(&existing.strategy_key);
            remove_rest_order(rest_orders, existing);
            Ok(())
        }
        Err(error)
            if submission_failure_decision(&error) == SubmissionFailureDecision::Reconcile =>
        {
            if let Some(local) = tracker.by_strategy_key.get_mut(&existing.strategy_key) {
                local.status = LocalOrderStatus::Unknown;
            }
            reconcile_unknown_submission(
                client,
                &existing.symbol,
                &existing.exchange_client_id,
                &existing.strategy_key,
                rest_orders,
                tracker,
            )
            .await
            .map_err(|reconcile_error| {
                anyhow::anyhow!(
                    "{}; {}",
                    safe_aster_error("Aster modify status remained unknown", &error),
                    reconcile_error
                )
            })
        }
        Err(error) => {
            warn!(
                "{}; original order remains",
                safe_aster_error("Aster modify was rejected", &error)
            );
            Ok(())
        }
    };
    drop(placement_guard);
    result
}

async fn reconcile_unknown_submission(
    client: &AsterClient,
    symbol: &str,
    exchange_client_id: &str,
    strategy_key: &str,
    rest_orders: &mut Vec<Order>,
    tracker: &mut OrderTracker,
) -> Result<()> {
    if let Ok(order) = client
        .query_order(symbol, None, Some(exchange_client_id))
        .await
    {
        apply_rest_order_result(tracker, strategy_key, &order);
        if is_open_status(&order.status) && rest_order_is_complete(&order) {
            rest_orders.push(order);
        }
        return Ok(());
    }
    let orders = client.open_orders(Some(symbol)).await.map_err(|error| {
        safe_aster_error(
            "query_order failed and openOrders reconciliation failed",
            &error,
        )
    })?;
    if let Some(order) = orders
        .iter()
        .find(|order| order.client_order_id == exchange_client_id)
    {
        apply_rest_order_result(tracker, strategy_key, order);
        rest_orders.extend(orders);
        return Ok(());
    }
    bail!("query_order and openOrders could not prove the ambiguous order's terminal state")
}

fn apply_rest_order_result(tracker: &mut OrderTracker, strategy_key: &str, order: &Order) {
    if is_terminal_status(&order.status) {
        tracker.remove(strategy_key);
        return;
    }
    if let Some(local) = tracker.by_strategy_key.get_mut(strategy_key) {
        local.order_id = Some(order.order_id);
        local.status = LocalOrderStatus::Live;
        if let Ok(price) = parse_f64(&order.price) {
            if price > 0.0 {
                local.price = price;
            }
        }
        if let Ok(quantity) = parse_f64(&order.orig_qty) {
            if quantity > 0.0 {
                local.quantity = quantity;
            }
        }
        tracker
            .by_order_id
            .insert(order.order_id, strategy_key.to_string());
    }
}

pub(crate) fn rest_order_is_complete(order: &Order) -> bool {
    parse_order_side(&order.side).is_ok()
        && parse_f64(&order.price).is_ok_and(|price| price > 0.0)
        && parse_f64(&order.orig_qty).is_ok_and(|quantity| quantity > 0.0)
}

#[allow(clippy::too_many_arguments)]
async fn enforce_risk_gates(
    client: &AsterClient,
    symbols: &[String],
    markets: &HashMap<u32, AsterMarket>,
    market_ids: &HashMap<String, u32>,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    positions: &[PositionRisk],
    risk_manager: &mut risk::risk_manager::RiskManager,
    tracker: &mut OrderTracker,
) -> Result<()> {
    let daily = dashboard_state.read().await.daily_realized_pnl;
    risk_manager.update_daily_pnl(daily);
    let status = risk_manager.status();
    {
        let mut dashboard = dashboard_state.write().await;
        dashboard.risk_status = Some(serde_json::to_value(&status)?);
    }
    let emergency = risk_manager.is_emergency_triggered() || risk_manager.should_emergency_close();
    let current_prices: HashMap<String, f64> = positions
        .iter()
        .map(|position| Ok((position.symbol.clone(), parse_f64(&position.mark_price)?)))
        .collect::<Result<_>>()?;
    let risk_positions: Vec<lighter::types::Position> = positions
        .iter()
        .filter_map(|position| {
            let amount = parse_f64(&position.position_amt).ok()?;
            (amount.abs() > 1e-12).then(|| {
                Ok(lighter::types::Position {
                    symbol: position.symbol.clone(),
                    side: if amount > 0.0 {
                        lighter::types::Side::Buy
                    } else {
                        lighter::types::Side::Sell
                    },
                    size: amount.abs(),
                    entry_price: parse_f64(&position.entry_price)?,
                    unrealized_pnl: parse_f64(&position.un_realized_profit)?,
                    leverage: parse_f64(&position.leverage)?,
                })
            })
        })
        .collect::<Result<_>>()?;
    let position_closes =
        risk_manager.check_position_stop_loss_take_profit(&risk_positions, &current_prices);
    if status.is_healthy && !emergency && position_closes.is_empty() {
        return Ok(());
    }
    if emergency && !risk_manager.is_emergency_triggered() {
        risk_manager.set_emergency_triggered();
    }
    dashboard_state.write().await.trading_paused = true;
    cancel_all_symbols(client, symbols).await?;

    let close_requests: Vec<(String, OrderSide, f64, f64)> = if emergency {
        positions
            .iter()
            .filter_map(|position| {
                let quantity = parse_f64(&position.position_amt).ok()?;
                (quantity.abs() > 1e-12).then(|| {
                    let side = if quantity > 0.0 {
                        OrderSide::Sell
                    } else {
                        OrderSide::Buy
                    };
                    Some((
                        position.symbol.clone(),
                        side,
                        parse_f64(&position.mark_price).ok()?,
                        quantity.abs(),
                    ))
                })?
            })
            .collect()
    } else {
        position_closes
            .into_iter()
            .map(|close| {
                (
                    close.symbol,
                    from_lighter_side(close.side_to_close),
                    close.current_price,
                    close.size,
                )
            })
            .collect()
    };
    for (symbol, side, mark, quantity) in close_requests {
        let market_id = *market_ids
            .get(&symbol)
            .with_context(|| format!("risk position {symbol} is not configured"))?;
        let aggressive = if side == OrderSide::Buy {
            mark * 1.005
        } else {
            mark * 0.995
        };
        place_risk_exit(
            client,
            markets.get(&market_id).context("missing Aster market")?,
            market_id,
            &symbol,
            side,
            aggressive,
            quantity,
            tracker,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn place_risk_exit(
    client: &AsterClient,
    market: &AsterMarket,
    market_id: u32,
    symbol: &str,
    side: OrderSide,
    price: f64,
    quantity: f64,
    tracker: &mut OrderTracker,
) -> Result<()> {
    let quantized =
        market.quantize_reduce_only(&decimal(price), &decimal(quantity), opposite(side))?;
    let strategy_key = format!("risk_{symbol}_{}", side_label(to_lighter_side(side)));
    if tracker.by_strategy_key.contains_key(&strategy_key) {
        return Ok(());
    }
    let client_id = unique_client_id(&strategy_key, market_id, side);
    let request = NewOrderRequest {
        symbol: symbol.to_string(),
        side,
        order_type: "LIMIT".to_string(),
        position_side: Some("BOTH".to_string()),
        time_in_force: Some("IOC".to_string()),
        quantity: Some(quantized.quantity.clone()),
        reduce_only: Some(true),
        price: Some(quantized.price.clone()),
        client_order_id: Some(client_id.clone()),
        response_type: Some("RESULT".to_string()),
    };
    tracker.insert(LocalOrder {
        strategy_key: strategy_key.clone(),
        exchange_client_id: client_id.clone(),
        order_id: None,
        symbol: symbol.to_string(),
        side,
        price: parse_f64(&quantized.price)?,
        quantity: parse_f64(&quantized.quantity)?,
        status: LocalOrderStatus::Pending,
        last_event_time: 0,
        last_transaction_time: 0,
    });
    match client.place_order(&request).await {
        Ok(order) => {
            apply_rest_order_result(tracker, &strategy_key, &order);
            Ok(())
        }
        Err(error)
            if submission_failure_decision(&error) == SubmissionFailureDecision::Reconcile =>
        {
            if let Some(local) = tracker.by_strategy_key.get_mut(&strategy_key) {
                local.status = LocalOrderStatus::Unknown;
            }
            let mut orders = Vec::new();
            reconcile_unknown_submission(
                client,
                symbol,
                &client_id,
                &strategy_key,
                &mut orders,
                tracker,
            )
            .await
        }
        Err(error) => {
            tracker.remove(&strategy_key);
            Err(safe_aster_error("Aster risk exit failed", &error))
        }
    }
}

async fn update_dashboard_account(
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    account: &Account,
    positions: &[PositionRisk],
    orders: &[Order],
    risk_manager: &mut risk::risk_manager::RiskManager,
) -> Result<()> {
    let (equity, available_balance, unrealized_pnl) = account_totals(account)?;
    risk_manager.update_equity(equity);
    let mut dashboard = dashboard_state.write().await;
    dashboard.equity = equity;
    dashboard.available_balance = available_balance;
    dashboard.unrealized_pnl = unrealized_pnl;
    dashboard.positions = one_way_positions(positions)?;
    dashboard.open_orders = orders.len() as u32;
    dashboard.open_orders_list = orders
        .iter()
        .map(|order| {
            serde_json::json!({
                "id": order.order_id,
                "client_id": order.client_order_id,
                "symbol": order.symbol,
                "side": order.side,
                "price": parse_f64(&order.price).unwrap_or(0.0),
                "quantity": parse_f64(&order.orig_qty).unwrap_or(0.0),
                "filled_quantity": parse_f64(&order.executed_qty).unwrap_or(0.0),
                "status": order.status,
            })
        })
        .collect();
    dashboard.peak_equity = dashboard.peak_equity.max(equity);
    let now = Utc::now().timestamp();
    if dashboard
        .equity_history
        .last()
        .is_none_or(|(timestamp, _)| now - timestamp >= 60)
    {
        dashboard.equity_history.push((now, equity));
        let initial = dashboard.initial_equity;
        dashboard.pnl_history.push((now, equity - initial));
    }
    dashboard.save_pnl();
    Ok(())
}

async fn reconcile_account_history(
    client: &AsterClient,
    symbols: &[String],
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    ledger: &mut AsterLedger,
    baseline_only: bool,
) -> Result<()> {
    let now = unix_millis();
    let first_start = now.saturating_sub(HISTORY_LOOKBACK_MS);
    let mut trades = Vec::new();
    for symbol in symbols {
        let mut from_id = ledger
            .trade_high_water
            .get(symbol)
            .copied()
            .map(|id| id.saturating_add(1));
        let mut start_time = from_id.is_none().then_some(first_start);
        loop {
            let end_time = from_id.is_none().then_some(now);
            let page = client
                .user_trades(&UserTradesQuery {
                    symbol: symbol.clone(),
                    start_time,
                    end_time,
                    from_id,
                    limit: Some(1_000),
                })
                .await
                .map_err(|error| {
                    safe_aster_error(
                        &format!("failed to reconcile Aster userTrades for {symbol}"),
                        &error,
                    )
                })?;
            let page_len = page.len();
            let highest = page.iter().map(|trade| trade.id).max();
            trades.extend(page);
            if page_len < 1_000 {
                break;
            }
            let next = highest.context("Aster userTrades pagination did not advance")? + 1;
            from_id = Some(next);
            start_time = None;
        }
    }
    let start = if ledger.income_high_water_ms == 0 {
        first_start
    } else {
        ledger
            .income_high_water_ms
            .saturating_sub(INCOME_OVERLAP_MS)
    };
    let mut incomes = Vec::new();
    let mut window_start = start;
    while window_start <= now {
        let window_end = window_start
            .saturating_add(HISTORY_LOOKBACK_MS.saturating_sub(1))
            .min(now);
        let mut cursor = window_start;
        loop {
            let page = client
                .income(&IncomeQuery {
                    start_time: Some(cursor),
                    end_time: Some(window_end),
                    limit: Some(1_000),
                    ..IncomeQuery::default()
                })
                .await
                .map_err(|error| safe_aster_error("failed to reconcile Aster income", &error))?;
            let page_len = page.len();
            let highest = page.iter().map(|income| income.time).max();
            incomes.extend(page);
            if page_len < 1_000 {
                break;
            }
            let highest = highest.context("Aster income pagination did not advance")?;
            // Fetch the complete boundary millisecond before advancing; otherwise
            // a full page can silently omit sibling income rows with the same time.
            let boundary = client
                .income(&IncomeQuery {
                    start_time: Some(highest),
                    end_time: Some(highest),
                    limit: Some(1_000),
                    ..IncomeQuery::default()
                })
                .await
                .map_err(|error| {
                    safe_aster_error("failed to reconcile Aster income boundary", &error)
                })?;
            if boundary.len() >= 1_000 {
                bail!("Aster income has at least 1000 rows in one millisecond; pagination is ambiguous");
            }
            incomes.extend(boundary);
            let next = highest.saturating_add(1);
            if next <= cursor {
                bail!("Aster income pagination cursor did not advance");
            }
            cursor = next;
        }
        if window_end == now {
            break;
        }
        window_start = window_end.saturating_add(1);
    }
    trades.sort_by_key(|trade| trade.time);
    incomes.sort_by_key(|income| income.time);
    let mut dashboard = dashboard_state.write().await;
    for trade in trades {
        let is_new = ledger.record_trade_id(&trade.symbol, trade.id);
        if !is_new || baseline_only {
            continue;
        }
        apply_user_trade(&mut dashboard, &trade)?;
    }
    for income in incomes {
        let is_new = ledger.record_income(&income);
        if !is_new || baseline_only {
            continue;
        }
        apply_income(&mut dashboard, &income)?;
    }
    dashboard.total_trades = ledger.seen_trade_ids.len() as u64;
    if baseline_only {
        warn!(
            "Aster ledger was missing while pnl_state existed; initialized seven-day seen-ID baseline without adding historical income"
        );
    }
    ledger.pnl_checkpoint = Some(pnl_snapshot(&dashboard));
    ledger.save_atomic()?;
    dashboard.save_pnl();
    Ok(())
}

fn apply_user_trade(
    dashboard: &mut dashboard::server::DashboardState,
    trade: &UserTrade,
) -> Result<()> {
    let price = parse_f64(&trade.price)?;
    let quantity = parse_f64(&trade.qty)?;
    dashboard.push_trade(serde_json::json!({
        "timestamp": Utc.timestamp_millis_opt(trade.time as i64).single()
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| trade.time.to_string()),
        "trade_id": trade.id,
        "order_id": trade.order_id,
        "symbol": trade.symbol,
        "side": trade.side,
        "price": price,
        "quantity": quantity,
        "maker": trade.maker,
        "pnl": 0.0,
        "action": "Fill",
    }));
    Ok(())
}

fn apply_income(dashboard: &mut dashboard::server::DashboardState, income: &Income) -> Result<()> {
    if !matches!(
        income.income_type.as_str(),
        "REALIZED_PNL" | "COMMISSION" | "FUNDING_FEE"
    ) {
        return Ok(());
    }
    let amount = parse_f64(&income.income)?;
    dashboard.total_realized_pnl += amount;
    let date = Utc
        .timestamp_millis_opt(income.time as i64)
        .single()
        .context("Aster income timestamp out of range")?
        .format("%Y-%m-%d")
        .to_string();
    *dashboard.daily_pnl_map.entry(date.clone()).or_default() += amount;
    if date == Utc::now().format("%Y-%m-%d").to_string() {
        dashboard.daily_realized_pnl += amount;
    }
    if income.income_type == "FUNDING_FEE" {
        dashboard.total_funding_pnl += amount;
        if date == Utc::now().format("%Y-%m-%d").to_string() {
            dashboard.daily_funding_pnl += amount;
        }
    }
    Ok(())
}

async fn cancel_all_symbols(client: &AsterClient, symbols: &[String]) -> Result<()> {
    for symbol in symbols {
        client.cancel_all_orders(symbol).await.map_err(|error| {
            safe_aster_error(&format!("Aster cancel-all failed for {symbol}"), &error)
        })?;
    }
    Ok(())
}

async fn fetch_open_orders(client: &AsterClient, symbols: &[String]) -> Result<Vec<Order>> {
    let mut orders = Vec::new();
    for symbol in symbols {
        let mut symbol_orders = client.open_orders(Some(symbol)).await.map_err(|error| {
            safe_aster_error(
                &format!("Aster openOrders refresh failed for {symbol}"),
                &error,
            )
        })?;
        orders.append(&mut symbol_orders);
    }
    Ok(orders)
}

async fn safety_shutdown(
    client: &AsterClient,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    symbols: &[String],
) {
    {
        let mut dashboard = dashboard_state.write().await;
        dashboard.trading_paused = true;
    }
    for symbol in symbols {
        if let Err(error) = client.cancel_all_orders(symbol).await {
            error!(
                "{}",
                safe_aster_error(
                    &format!("Aster safety cancel-all failed for {symbol}"),
                    &error
                )
            );
        }
    }
}

fn parse_f64(value: &str) -> Result<f64> {
    let parsed = value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal value {value}"))?;
    if !parsed.is_finite() {
        bail!("non-finite decimal value {value}");
    }
    Ok(parsed)
}

fn parse_depth_levels(levels: &[(String, String)]) -> Result<Vec<(f64, f64)>> {
    levels
        .iter()
        .map(|(price, quantity)| {
            let price = parse_f64(price)?;
            let quantity = parse_f64(quantity)?;
            if price <= 0.0 || quantity < 0.0 {
                bail!("invalid Aster depth level");
            }
            Ok((price, quantity))
        })
        .collect()
}

fn decimal(value: f64) -> String {
    format!("{value:.12}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

fn parse_order_side(value: &str) -> Result<OrderSide> {
    match value.to_ascii_uppercase().as_str() {
        "BUY" => Ok(OrderSide::Buy),
        "SELL" => Ok(OrderSide::Sell),
        _ => bail!("invalid Aster order side {value}"),
    }
}

fn from_lighter_side(side: lighter::types::Side) -> OrderSide {
    match side {
        lighter::types::Side::Buy => OrderSide::Buy,
        lighter::types::Side::Sell => OrderSide::Sell,
    }
}

fn to_lighter_side(side: OrderSide) -> lighter::types::Side {
    match side {
        OrderSide::Buy => lighter::types::Side::Buy,
        OrderSide::Sell => lighter::types::Side::Sell,
    }
}

fn opposite(side: OrderSide) -> OrderSide {
    match side {
        OrderSide::Buy => OrderSide::Sell,
        OrderSide::Sell => OrderSide::Buy,
    }
}

fn side_label(side: lighter::types::Side) -> &'static str {
    match side {
        lighter::types::Side::Buy => "buy",
        lighter::types::Side::Sell => "sell",
    }
}

fn is_open_status(status: &str) -> bool {
    matches!(
        status.to_ascii_uppercase().as_str(),
        "NEW" | "PARTIALLY_FILLED"
    )
}

fn is_terminal_status(status: &str) -> bool {
    matches!(
        status.to_ascii_uppercase().as_str(),
        "FILLED" | "CANCELED" | "EXPIRED" | "REJECTED"
    )
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
