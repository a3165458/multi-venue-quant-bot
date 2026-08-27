//! Hyperliquid live control loop with HIP-3 builder-dex support.
//!
//! The exchange protocol adapter (`hyperliquid.rs`) stays separate from live
//! trading policy. Every ambiguous execution outcome fails closed: pause first,
//! cancel every configured coin, then leave the loop. Coins are configured by
//! canonical name; HIP-3 assets (for example the entropy.io markets `io:SNDK`
//! and `io:ANTH`) use the `dex:NAME` form and are resolved to asset ids at
//! startup.

use anyhow::{bail, Context, Result};
use chrono::{TimeZone, Utc};
use config::Config;
use futures::{SinkExt, StreamExt};
use multi_venue_quant_bot::exchange::LiveVenue;
use multi_venue_quant_bot::hyperliquid::{
    cloid_from_key, order_update_is_terminal, ClearinghouseState, Fill, HyperliquidClient,
    HyperliquidCredentials, HyperliquidEnvironment, HyperliquidError, HyperliquidMarket,
    HyperliquidWsEvent, NewOrderRequest, OpenOrder, OrderId, OrderOutcome, PerpPosition,
    SpotClearinghouseState, Subscription, Tif, UserFundingEntry, WsFill, WsOrderUpdate,
    MIN_ORDER_NOTIONAL_USD,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

use crate::aster_live::{
    calculate_exposure, maker_strategy_allowed, signal_allowed, ExposureInput,
};
use crate::{dashboard, data, env_profiles, lighter, risk, strategy};

const LEDGER_FILE: &str = "hyperliquid_ledger.json";
const PARSE_ERROR_LIMIT: u8 = 3;
const SAFE_SESSION_AGE: Duration = Duration::from_secs(23 * 60 * 60 + 50 * 60);
const HISTORY_LOOKBACK_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const HISTORY_OVERLAP_MS: u64 = 60_000;
const WS_PING_INTERVAL: Duration = Duration::from_secs(45);
const BASIS_PROBE_INTERVAL: Duration = Duration::from_secs(30);
const L1_BUDGET_BACKOFF: Duration = Duration::from_secs(30);
static L1_BUDGET_BLOCKED_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
static BASIS_PROBE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

struct BasisProbeGuard;
impl Drop for BasisProbeGuard {
    fn drop(&mut self) {
        BASIS_PROBE_IN_FLIGHT.store(false, Ordering::SeqCst);
    }
}

fn l1_budget_is_blocked() -> bool {
    unix_millis() < L1_BUDGET_BLOCKED_UNTIL_MS.load(Ordering::Relaxed)
}

fn trip_l1_budget_backoff() {
    let until = unix_millis().saturating_add(L1_BUDGET_BACKOFF.as_millis() as u64);
    L1_BUDGET_BLOCKED_UNTIL_MS.store(until, Ordering::Relaxed);
    warn!(
        "Hyperliquid L1 request budget exhausted; signed writes paused for {}s",
        L1_BUDGET_BACKOFF.as_secs()
    );
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
    /// Deterministic Hyperliquid cloid derived from the strategy key.
    pub(crate) cloid: String,
    pub(crate) oid: Option<u64>,
    pub(crate) coin: String,
    pub(crate) is_buy: bool,
    pub(crate) price: f64,
    pub(crate) quantity: f64,
    pub(crate) status: LocalOrderStatus,
    pub(crate) last_status_time: u64,
}

#[derive(Debug, Default)]
pub(crate) struct OrderTracker {
    pub(crate) by_strategy_key: HashMap<String, LocalOrder>,
    by_cloid: HashMap<String, String>,
    by_oid: HashMap<u64, String>,
}

impl OrderTracker {
    pub(crate) fn insert(&mut self, order: LocalOrder) {
        self.remove(&order.strategy_key);
        self.by_cloid
            .insert(order.cloid.clone(), order.strategy_key.clone());
        if let Some(oid) = order.oid {
            self.by_oid.insert(oid, order.strategy_key.clone());
        }
        self.by_strategy_key
            .insert(order.strategy_key.clone(), order);
    }

    pub(crate) fn remove(&mut self, strategy_key: &str) -> Option<LocalOrder> {
        let removed = self.by_strategy_key.remove(strategy_key)?;
        self.by_cloid.remove(&removed.cloid);
        if let Some(oid) = removed.oid {
            self.by_oid.remove(&oid);
        }
        Some(removed)
    }

    fn strategy_key_for(&self, cloid: Option<&str>, oid: u64) -> Option<String> {
        cloid
            .and_then(|cloid| self.by_cloid.get(cloid))
            .or_else(|| self.by_oid.get(&oid))
            .cloned()
    }

    /// Apply a WebSocket order update. Returns true when a tracked order changed.
    pub(crate) fn apply_ws_order(&mut self, update: &WsOrderUpdate) -> bool {
        let Some(strategy_key) =
            self.strategy_key_for(update.order.cloid.as_deref(), update.order.oid)
        else {
            return false;
        };
        let Some(local) = self.by_strategy_key.get_mut(&strategy_key) else {
            return false;
        };
        if update.status_timestamp <= local.last_status_time {
            return false;
        }
        local.last_status_time = update.status_timestamp;
        local.oid = Some(update.order.oid);
        self.by_oid.insert(update.order.oid, strategy_key.clone());
        if order_update_is_terminal(&update.status) {
            self.remove(&strategy_key);
        } else {
            local.status = LocalOrderStatus::Live;
            if let Ok(price) = parse_f64(&update.order.limit_px) {
                if price > 0.0 {
                    local.price = price;
                }
            }
        }
        true
    }

    fn reconcile_open_orders(&mut self, orders: &[OpenOrder]) {
        let mut live_keys = HashSet::new();
        for order in orders {
            let Some(key) = self.strategy_key_for(order.cloid.as_deref(), order.oid) else {
                continue;
            };
            live_keys.insert(key.clone());
            if let Some(local) = self.by_strategy_key.get_mut(&key) {
                local.status = LocalOrderStatus::Live;
                local.oid = Some(order.oid);
                local.price = parse_f64(&order.limit_px).unwrap_or(local.price);
                local.quantity = parse_f64(&order.orig_sz).unwrap_or(local.quantity);
                self.by_oid.insert(order.oid, key);
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

    fn local_open_count_not_in_rest(&self, rest: &[OpenOrder]) -> usize {
        self.by_strategy_key
            .values()
            .filter(|local| {
                !rest.iter().any(|order| {
                    order.cloid.as_deref() == Some(local.cloid.as_str())
                        || local.oid == Some(order.oid)
                })
            })
            .count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmissionFailureDecision {
    Reconcile,
    Reject,
    /// Exchange refused the signed action before it could rest. The local
    /// mirror can be dropped and the loop can continue.
    Skip,
}

pub(crate) fn l1_request_budget_exhausted(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("too many cumulative requests")
}

pub(crate) fn submission_failure_decision(error: &HyperliquidError) -> SubmissionFailureDecision {
    match error {
        // The exchange may have accepted the bytes even when the response never
        // arrived, so these must be reconciled instead of assumed dead.
        HyperliquidError::Transport(_)
        | HyperliquidError::RateLimited { .. }
        | HyperliquidError::InvalidResponse(_) => SubmissionFailureDecision::Reconcile,
        HyperliquidError::ActionRejected { message } if l1_request_budget_exhausted(message) => {
            SubmissionFailureDecision::Skip
        }
        _ => SubmissionFailureDecision::Reject,
    }
}

fn safe_hl_error(operation: &str, error: &HyperliquidError) -> anyhow::Error {
    let detail = match error {
        HyperliquidError::Credentials(_) => "credential validation failed".to_string(),
        HyperliquidError::InvalidRequest(_) => "request validation failed".to_string(),
        HyperliquidError::Signing(_) => "request signing failed".to_string(),
        HyperliquidError::Transport(source) if source.is_timeout() => {
            "transport timeout".to_string()
        }
        HyperliquidError::Transport(source) if source.is_connect() => {
            "transport connection failed".to_string()
        }
        HyperliquidError::Transport(_) => "transport request failed".to_string(),
        HyperliquidError::InvalidResponse(_) => "invalid response".to_string(),
        HyperliquidError::RateLimited { .. } => "HTTP 429 rate limit".to_string(),
        HyperliquidError::ActionRejected { message } => {
            format!("action rejected: {message}")
        }
        HyperliquidError::Api { status, .. } => format!("API error HTTP {status}"),
        HyperliquidError::UnknownMarket(market) => format!("unknown market {market}"),
    };
    anyhow::anyhow!("{operation}: {detail} (signed payload redacted)")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuoteReplaceDecision {
    Noop,
    CancelThenWait,
    BlockedUnresolved,
}

/// Hyperliquid quote replacement is always cancel-then-requote: a fresh order
/// re-enters the book queue, which is acceptable for the maker cadence used
/// here and avoids ambiguous in-flight modify states entirely.
pub(crate) fn quote_replace_decision(
    existing: &LocalOrder,
    new_price: f64,
) -> QuoteReplaceDecision {
    if (existing.price - new_price).abs() <= f64::EPSILON {
        return QuoteReplaceDecision::Noop;
    }
    if existing.status != LocalOrderStatus::Live {
        return QuoteReplaceDecision::BlockedUnresolved;
    }
    QuoteReplaceDecision::CancelThenWait
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

/// A position snapshot merged across every configured dex.
#[derive(Debug, Clone)]
pub(crate) struct TrackedPosition {
    pub(crate) coin: String,
    /// Signed size: positive long, negative short.
    pub(crate) szi: f64,
    pub(crate) entry_px: f64,
    pub(crate) mark_px: f64,
    pub(crate) unrealized_pnl: f64,
    pub(crate) leverage_kind: String,
    pub(crate) leverage_value: f64,
}

pub(crate) fn tracked_position(position: &PerpPosition) -> Result<TrackedPosition> {
    let szi = parse_f64(&position.szi)?;
    let position_value = parse_f64(&position.position_value)?;
    let mark_px = if szi.abs() > 1e-12 {
        position_value / szi.abs()
    } else {
        0.0
    };
    Ok(TrackedPosition {
        coin: position.coin.clone(),
        szi,
        entry_px: position
            .entry_px
            .as_deref()
            .map(parse_f64)
            .transpose()?
            .unwrap_or(0.0),
        mark_px,
        unrealized_pnl: parse_f64(&position.unrealized_pnl)?,
        leverage_kind: position.leverage.kind.clone(),
        leverage_value: f64::from(position.leverage.value),
    })
}

pub(crate) fn positions_json(positions: &[TrackedPosition]) -> Vec<serde_json::Value> {
    positions
        .iter()
        .filter(|position| position.szi.abs() > 1e-12)
        .map(|position| {
            serde_json::json!({
                "symbol": position.coin,
                "side": if position.szi > 0.0 { "Buy" } else { "Sell" },
                "size": position.szi.abs(),
                "entry_price": position.entry_px,
                "mark_price": position.mark_px,
                "unrealized_pnl": position.unrealized_pnl,
                "leverage": position.leverage_value,
            })
        })
        .collect()
}

/// Nonzero positions must stay inside the configured coins, and isolated
/// margin is required where the venue allows a choice (HIP-3 assets are
/// isolated-only by construction).
pub(crate) fn validate_runtime_positions(
    positions: &[TrackedPosition],
    coins: &[String],
    require_isolated_margin: bool,
) -> Result<()> {
    for position in positions {
        if position.szi.abs() <= 1e-12 {
            continue;
        }
        if !coins.iter().any(|coin| coin == &position.coin) {
            bail!(
                "Hyperliquid account has an open position in unconfigured coin {}; refusing to trade",
                position.coin
            );
        }
        if require_isolated_margin && !position.leverage_kind.eq_ignore_ascii_case("isolated") {
            bail!(
                "Hyperliquid coin {} is not in isolated margin mode",
                position.coin
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct HyperliquidLedger {
    /// Fill trade ids (`tid`) already applied to the dashboard PnL.
    #[serde(default)]
    pub(crate) seen_fill_tids: HashSet<u64>,
    #[serde(default)]
    pub(crate) fill_high_water_ms: u64,
    /// Funding entries already applied, keyed by `time:coin:usdc`.
    #[serde(default)]
    pub(crate) seen_funding_keys: HashSet<String>,
    #[serde(default)]
    pub(crate) funding_high_water_ms: u64,
    #[serde(default)]
    pnl_checkpoint: Option<dashboard::server::PersistentPnlData>,
}

impl HyperliquidLedger {
    fn path(network: &str) -> Result<PathBuf> {
        dashboard::runtime_paths::data_file(network, LEDGER_FILE)
    }

    fn load(network: &str) -> Result<Option<Self>> {
        let path = Self::path(network)?;
        if !path.exists() {
            return Ok(None);
        }
        let bytes =
            std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let ledger = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(ledger))
    }

    fn save_atomic(&self, network: &str) -> Result<()> {
        let path = Self::path(network)?;
        let parent = path
            .parent()
            .context("Hyperliquid ledger path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, &path)?;
        Ok(())
    }

    pub(crate) fn record_fill(&mut self, time: u64, tid: u64) -> bool {
        self.fill_high_water_ms = self.fill_high_water_ms.max(time);
        self.seen_fill_tids.insert(tid)
    }

    pub(crate) fn record_funding(&mut self, entry: &UserFundingEntry) -> bool {
        self.funding_high_water_ms = self.funding_high_water_ms.max(entry.time);
        self.seen_funding_keys.insert(funding_key(entry))
    }
}

fn funding_key(entry: &UserFundingEntry) -> String {
    format!(
        "{}:{}:{}:{}",
        entry.time, entry.delta.coin, entry.delta.usdc, entry.hash
    )
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

/// Sum equity/withdrawable/unrealized. Standard/manual accounts keep HIP-3
/// collateral on each builder dex. Unified and portfolio-margin accounts keep
/// USDC in the spot clearinghouse; those dex ledgers are then not meaningful.
pub(crate) fn account_totals(
    states: &[ClearinghouseState],
    unified_spot_usdc: Option<(f64, f64)>,
) -> Result<(f64, f64, f64)> {
    let mut unrealized = 0.0;
    for state in states {
        for entry in &state.asset_positions {
            unrealized += parse_f64(&entry.position.unrealized_pnl)?;
        }
    }
    if let Some((equity, available)) = unified_spot_usdc {
        return Ok((equity, available, unrealized));
    }
    let mut equity = 0.0;
    let mut available = 0.0;
    for state in states {
        equity += parse_f64(&state.margin_summary.account_value)?;
        available += parse_f64(&state.withdrawable)?;
    }
    Ok((equity, available, unrealized))
}

fn spot_usdc_totals(spot: &SpotClearinghouseState) -> Result<(f64, f64)> {
    let usdc = spot
        .balances
        .iter()
        .find(|balance| balance.coin.eq_ignore_ascii_case("USDC") || balance.token == 0);
    let total = match usdc {
        Some(balance) => parse_f64(&balance.total)?,
        None => 0.0,
    };
    let available = if let Some((_, value)) = spot
        .token_to_available_after_maintenance
        .iter()
        .find(|(token, _)| *token == 0)
    {
        parse_f64(value)?
    } else if let Some(balance) = usdc {
        (total - parse_f64(&balance.hold)?).max(0.0)
    } else {
        0.0
    };
    Ok((total, available))
}

async fn fetch_unified_spot_usdc(
    client: &HyperliquidClient,
    user: &str,
) -> Result<Option<(f64, f64)>> {
    let mode = client
        .user_abstraction(user)
        .await
        .map_err(|error| safe_hl_error("Hyperliquid userAbstraction lookup failed", &error))?;
    if !mode.uses_spot_collateral() {
        return Ok(None);
    }
    let spot = client
        .spot_clearinghouse_state(user)
        .await
        .map_err(|error| safe_hl_error("Hyperliquid spot clearinghouse refresh failed", &error))?;
    let totals = spot_usdc_totals(&spot)?;
    info!(
        "Hyperliquid {} collateral from spot USDC: equity={:.4} available={:.4}",
        format!("{:?}", mode),
        totals.0,
        totals.1
    );
    Ok(Some(totals))
}

pub(crate) async fn run_hyperliquid_live_trading(settings: Config) -> Result<()> {
    let venue = validate_hyperliquid_selection(&settings)?;
    let network = venue.as_str().to_string();
    let environment = match venue {
        LiveVenue::HyperliquidMainnet => HyperliquidEnvironment::Mainnet,
        LiveVenue::HyperliquidTestnet => HyperliquidEnvironment::Testnet,
        _ => bail!("{venue} is not a Hyperliquid venue"),
    };
    let (loaded, credential_path) = env_profiles::load_hyperliquid_credentials(venue)?;
    let credentials =
        HyperliquidCredentials::new(&loaded.account_address, &loaded.signer_private_key)
            .map_err(|error| safe_hl_error("invalid Hyperliquid credentials", &error))?;
    drop(loaded);
    let account_tail =
        &credentials.account_address()[credentials.account_address().len().saturating_sub(8)..];
    info!(
        "🔐 Using Hyperliquid account …{} (signer …{}) from {}",
        account_tail,
        &credentials.signer_address()[credentials.signer_address().len().saturating_sub(8)..],
        credential_path.display()
    );
    let user = credentials.account_address().to_string();
    let client = Arc::new(HyperliquidClient::authenticated(credentials, environment));

    let (coins, dexs, market_ids, markets) = load_markets(&client, &settings).await?;
    info!(
        "🧭 Hyperliquid markets resolved: {}",
        coins
            .iter()
            .map(|coin| format!("{coin}#{}", market_ids[coin]))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let execution_strategy: Arc<RwLock<Box<dyn strategy::Strategy>>> =
        Arc::new(RwLock::new(strategy::create_strategy(&settings)?));
    if !maker_strategy_allowed(execution_strategy.read().await.name()) {
        bail!("Hyperliquid live mode requires the maker_quote strategy");
    }
    if let Some(saved) = dashboard::server::PersistentStrategyConfig::load(&network) {
        if !maker_strategy_allowed(&saved.strategy_name) {
            bail!(
                "persisted Hyperliquid strategy {} is not maker_quote; refusing startup",
                saved.strategy_name
            );
        }
        let params = sorted_params(&saved.strategy_params);
        strategy::create_strategy_with_params(
            "maker_quote",
            (!params.is_empty()).then_some(params.as_str()),
        )
        .context("persisted Hyperliquid maker strategy is invalid; refusing startup")?;
    }
    let mut risk_manager = risk::risk_manager::RiskManager::new(&settings)?;
    risk_manager.override_profitability_schedule(
        risk::profitability::HIP3_GROWTH_MAKER_FEE_BPS,
        risk::profitability::HIP3_GROWTH_TAKER_FEE_BPS,
        risk::profitability::HIP3_GROWTH_ADVERSE_BPS,
    )?;
    tracing::info!(
        "HIP-3 growth-mode profitability: maker {:.2} bps taker {:.2} bps adverse {:.2} bps (yaml 1.5/4.5 unchanged)",
        risk::profitability::HIP3_GROWTH_MAKER_FEE_BPS,
        risk::profitability::HIP3_GROWTH_TAKER_FEE_BPS,
        risk::profitability::HIP3_GROWTH_ADVERSE_BPS,
    );
    let mut user_add_rate_bps = None;
    let mut user_cross_rate_bps = None;
    match client.user_fees(&user).await {
        Ok(fees) => match (
            fees.user_add_rate.parse::<f64>(),
            fees.user_cross_rate.parse::<f64>(),
        ) {
            (Ok(add), Ok(cross)) => {
                let add_bps = (add * 10_000.0 * 100.0).round() / 100.0;
                let cross_bps = (cross * 10_000.0 * 100.0).round() / 100.0;
                user_add_rate_bps = Some(add_bps);
                user_cross_rate_bps = Some(cross_bps);
                if add_bps <= 0.0 {
                    info!(
                        "HL userFees add={add_bps:.2} bps cross={cross_bps:.2} bps (T4 maker 0)"
                    );
                } else {
                    warn!(
                        "HL userFees add={add_bps:.2} bps cross={cross_bps:.2} bps — not T4 (T4 maker is 0); HIP-3 growth still ~0.29/0.86"
                    );
                }
            }
            _ => warn!(
                "HL userFees unparseable add={} cross={}",
                fees.user_add_rate, fees.user_cross_rate
            ),
        },
        Err(error) => warn!("HL userFees lookup skipped: {error}"),
    }
    let risk_config = risk_manager.get_config();
    let risk_ceilings = RiskCeilings::from_config(&risk_config)?;
    let require_isolated_margin = settings
        .get_bool("trading.require_isolated_margin")
        .unwrap_or(true);

    let states = fetch_clearinghouse_states(&client, &user, &dexs).await?;
    let mut positions = collect_positions(&states)?;
    validate_runtime_positions(&positions, &coins, require_isolated_margin)
        .context("Hyperliquid account safety validation failed")?;

    let start_paused = settings.get_bool("trading.start_paused").unwrap_or(true);
    if start_paused {
        info!(
            "Skipping isolated leverage update because trading.start_paused=true (preserve L1 budget)"
        );
    } else {
        apply_isolated_leverage(&client, &settings, &markets, &positions).await?;
    }

    let startup_orders = fetch_open_orders(&client, &user, &dexs).await?;
    let unknown: Vec<&str> = startup_orders
        .iter()
        .filter(|order| !market_ids.contains_key(&order.coin))
        .map(|order| order.coin.as_str())
        .collect();
    if !unknown.is_empty() {
        bail!(
            "Hyperliquid account has open orders in unconfigured coins {unknown:?} on configured dexs; refusing to start"
        );
    }
    cancel_open_orders(&client, &market_ids, &startup_orders)
        .await
        .context("Hyperliquid startup cancel-all failed; refusing to start")?;
    let remaining = fetch_open_orders(&client, &user, &dexs).await?;
    if remaining
        .iter()
        .any(|order| market_ids.contains_key(&order.coin))
    {
        bail!("Hyperliquid startup cancel-all did not clear every configured coin");
    }

    let unified_spot_usdc = fetch_unified_spot_usdc(&client, &user).await?;
    let (equity, available_balance, unrealized_pnl) = account_totals(&states, unified_spot_usdc)?;
    risk_manager.update_equity(equity);
    let configured_ids: Vec<u32> = coins.iter().map(|coin| market_ids[coin]).collect();
    let dashboard_state = Arc::new(RwLock::new(dashboard::server::DashboardState {
        network_name: network.clone(),
        rest_url: environment.rest_url().to_string(),
        ws_url: environment.websocket_url().to_string(),
        equity,
        available_balance,
        unrealized_pnl,
        strategy_name: "maker_quote".to_string(),
        initial_equity: equity,
        peak_equity: equity,
        equity_history: vec![(Utc::now().timestamp(), equity)],
        active_markets: configured_ids.clone(),
        trading_paused: start_paused,
        available_markets: coins
            .iter()
            .map(|coin| (market_ids[coin], coin.clone()))
            .collect(),
        positions: positions_json(&positions),
        strategy_params: maker_dashboard_params(&settings),
        risk_config,
        leverage_limit: risk_manager.max_leverage(),
        shadow_metrics: Some(serde_json::json!({"enabled": false})),
        hft_shadow_metrics: Some(serde_json::json!({"enabled": false})),
        quant_agent: dashboard::quant_agent::AgentLedger::load(&network),
        user_add_rate_bps,
        user_cross_rate_bps,
        ..dashboard::server::DashboardState::default()
    }));

    let loaded_ledger = HyperliquidLedger::load(&network)?;
    let ledger_existed = loaded_ledger.is_some();
    let mut ledger = loaded_ledger.unwrap_or_default();
    let saved_pnl = ledger
        .pnl_checkpoint
        .clone()
        .or_else(|| dashboard::server::PersistentPnlData::load(&network));
    if let Some(saved) = saved_pnl.as_ref() {
        dashboard_state.write().await.restore_pnl(saved);
    }
    {
        let dashboard = dashboard_state.read().await;
        risk_manager.restore_equity_baseline(dashboard.initial_equity, equity);
        risk_manager.update_daily_pnl(dashboard.daily_realized_pnl);
    }
    restore_risk(&network, &dashboard_state, &mut risk_manager, risk_ceilings).await;
    restore_strategy(&network, &dashboard_state, &execution_strategy).await?;

    reconcile_account_history(
        &client,
        &user,
        &network,
        &coins,
        &dashboard_state,
        &mut ledger,
        !ledger_existed && saved_pnl.is_some(),
    )
    .await?;

    let data_store = Arc::new(RwLock::new(data::storage::MarketDataStore::new()));
    preload_candles(&client, &coins, &data_store).await?;
    let dashboard_host = settings
        .get_string("dashboard.host")
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let dashboard_port = settings.get_int("dashboard.port").unwrap_or(4029) as u16;
    let dashboard_for_server = dashboard_state.clone();
    tokio::spawn(async move {
        if let Err(error) = dashboard::server::start_with_state(
            &dashboard_host,
            dashboard_port,
            dashboard_for_server,
        )
        .await
        {
            error!("Hyperliquid dashboard failed: {error}");
        }
    });

    let ws_url = environment.websocket_url();
    let socket = match connect_async(ws_url).await {
        Ok((socket, _)) => socket,
        Err(connect_error) => {
            safety_shutdown(&client, &user, &dexs, &market_ids, &dashboard_state).await;
            warn!("Hyperliquid WebSocket connection failed: {connect_error}");
            bail!("failed to connect Hyperliquid WebSocket");
        }
    };
    let (mut ws_write, mut ws_read) = socket.split();
    for message in subscription_messages(&coins, &user) {
        if let Err(subscribe_error) = ws_write.send(Message::Text(message)).await {
            safety_shutdown(&client, &user, &dexs, &market_ids, &dashboard_state).await;
            warn!("Hyperliquid WebSocket subscribe failed: {subscribe_error}");
            bail!("failed to subscribe Hyperliquid WebSocket streams");
        }
    }

    let mut tracker = OrderTracker::default();
    let mut rest_orders = Vec::<OpenOrder>::new();
    let mut control_tick = tokio::time::interval(Duration::from_secs(1));
    let mut refresh_tick = tokio::time::interval(Duration::from_secs(10));
    let mut ping_tick = tokio::time::interval(WS_PING_INTERVAL);
    let mut basis_tick = tokio::time::interval(BASIS_PROBE_INTERVAL);
    let session_started = Instant::now();
    let mut ws_parse_errors = 0_u8;
    let mut position_sync_pending = HashSet::<String>::new();
    let mut accounting_day = Utc::now().date_naive();
    let mut was_paused = dashboard_state.read().await.trading_paused;
    let mut last_active_markets: HashSet<u32> = dashboard_state
        .read()
        .await
        .active_markets
        .iter()
        .copied()
        .collect();
    let max_open_orders = settings
        .get_int("trading.max_open_orders")
        .unwrap_or(4)
        .max(1) as usize;

    info!(
        "✅ Hyperliquid live loop connected for {} configured coins; paused={}",
        coins.len(),
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
                    bail!("Hyperliquid WebSocket session reached safe restart age");
                }
                consume_dashboard_controls(
                    &client,
                    &user,
                    &network,
                    &dexs,
                    &market_ids,
                    &dashboard_state,
                    &execution_strategy,
                    &mut risk_manager,
                    risk_ceilings,
                    &mut tracker,
                    &mut was_paused,
                    &mut last_active_markets,
                ).await
            }
            _ = basis_tick.tick() => {
                if BASIS_PROBE_IN_FLIGHT
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let probe_client = client.clone();
                    let probe_dashboard = dashboard_state.clone();
                    tokio::spawn(async move {
                        let _guard = BasisProbeGuard;
                        probe_io_xyz_sndk_basis(&probe_client, &probe_dashboard).await;
                    });
                }
                Ok(())
            }
            _ = ping_tick.tick() => {
                ws_write
                    .send(Message::Text(
                        multi_venue_quant_bot::hyperliquid::ws_ping_message(),
                    ))
                    .await
                    .context("failed to send Hyperliquid WebSocket ping")
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
                let states = fetch_clearinghouse_states(&client, &user, &dexs).await?;
                let unified_spot_usdc = fetch_unified_spot_usdc(&client, &user).await?;
                positions = collect_positions(&states)?;
                validate_runtime_positions(&positions, &coins, require_isolated_margin)?;
                position_sync_pending.clear();
                rest_orders = fetch_open_orders(&client, &user, &dexs)
                    .await?
                    .into_iter()
                    .filter(|order| market_ids.contains_key(&order.coin))
                    .collect();
                tracker.reconcile_open_orders(&rest_orders);
                update_dashboard_account(
                    &dashboard_state,
                    &states,
                    unified_spot_usdc,
                    &positions,
                    &rest_orders,
                    &mut risk_manager,
                ).await?;
                reconcile_account_history(
                    &client,
                    &user,
                    &network,
                    &coins,
                    &dashboard_state,
                    &mut ledger,
                    false,
                ).await?;
                enforce_risk_gates(
                    &client,
                    &user,
                    &dexs,
                    &markets,
                    &market_ids,
                    &dashboard_state,
                    &positions,
                    &mut risk_manager,
                    &mut tracker,
                ).await
            }
            message = ws_read.next() => {
                match message {
                    Some(Ok(Message::Text(text))) => {
                        match HyperliquidWsEvent::parse(&text) {
                            Ok(HyperliquidWsEvent::Bbo(update)) => {
                                ws_parse_errors = 0;
                                if position_sync_pending.contains(&update.coin) {
                                    Ok(())
                                } else {
                                    handle_bbo(
                                        update,
                                        &client,
                                        &user,
                                        &markets,
                                        &market_ids,
                                        &dashboard_state,
                                        &data_store,
                                        &execution_strategy,
                                        &mut risk_manager,
                                        &positions,
                                        &position_sync_pending,
                                        &mut rest_orders,
                                        &mut tracker,
                                        max_open_orders,
                                    ).await
                                }
                            }
                            Ok(HyperliquidWsEvent::OrderUpdates(updates)) => {
                                ws_parse_errors = 0;
                                for update in &updates {
                                    tracker.apply_ws_order(update);
                                    if order_update_is_terminal(&update.status) {
                                        rest_orders.retain(|order| {
                                            order.oid != update.order.oid
                                                && (order.cloid.is_none()
                                                    || order.cloid != update.order.cloid)
                                        });
                                    }
                                    if update.status == "filled" {
                                        position_sync_pending.insert(update.order.coin.clone());
                                    }
                                }
                                Ok(())
                            }
                            Ok(HyperliquidWsEvent::UserFills(event)) => {
                                ws_parse_errors = 0;
                                if !event.is_snapshot {
                                    let mut dashboard = dashboard_state.write().await;
                                    for fill in &event.fills {
                                        position_sync_pending.insert(fill.coin.clone());
                                        if !market_ids.contains_key(&fill.coin) {
                                            continue;
                                        }
                                        if ledger.record_fill(fill.time, fill.tid) {
                                            apply_ws_fill(&mut dashboard, fill)?;
                                        }
                                    }
                                    dashboard.total_trades =
                                        ledger.seen_fill_tids.len() as u64;
                                    dashboard.save_pnl();
                                    drop(dashboard);
                                    ledger.pnl_checkpoint =
                                        Some(pnl_snapshot(&*dashboard_state.read().await));
                                    ledger.save_atomic(&network)?;
                                }
                                Ok(())
                            }
                            Ok(HyperliquidWsEvent::Error(message)) => {
                                bail!("Hyperliquid WebSocket returned an error: {message}");
                            }
                            Ok(_) => {
                                ws_parse_errors = 0;
                                Ok(())
                            }
                            Err(parse_error) => {
                                ws_parse_errors = ws_parse_errors.saturating_add(1);
                                warn!(
                                    "Hyperliquid WS parse error ({ws_parse_errors}/{PARSE_ERROR_LIMIT}): {parse_error}"
                                );
                                if ws_parse_errors >= PARSE_ERROR_LIMIT {
                                    bail!("consecutive Hyperliquid WS parse failures");
                                }
                                Ok(())
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        ws_write.send(Message::Pong(payload)).await
                            .context("failed to answer Hyperliquid WS ping")
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        bail!("Hyperliquid WebSocket disconnected")
                    }
                    Some(Err(ws_error)) => Err(ws_error).context("Hyperliquid WebSocket failed"),
                    Some(Ok(_)) => Ok(()),
                }
            }
            }
        }
        .await;
        if let Err(loop_error) = event_result {
            safety_shutdown(&client, &user, &dexs, &market_ids, &dashboard_state).await;
            return Err(loop_error).context("Hyperliquid live loop stopped safely");
        }
    }
}

fn validate_hyperliquid_selection(settings: &Config) -> Result<LiveVenue> {
    let environment = settings
        .get_string("exchange.environment")
        .context("exchange.environment is required for Hyperliquid")?;
    let expected = match environment.to_ascii_lowercase().as_str() {
        "mainnet" => LiveVenue::HyperliquidMainnet,
        "testnet" => LiveVenue::HyperliquidTestnet,
        other => bail!("unsupported Hyperliquid environment {other}"),
    };
    let selected = env_profiles::selected_venue();
    if selected != expected {
        bail!("selected venue {selected} does not match configured {expected}");
    }
    Ok(expected)
}

type LoadedMarkets = (
    Vec<String>,
    Vec<String>,
    HashMap<String, u32>,
    HashMap<u32, HyperliquidMarket>,
);

async fn load_markets(client: &HyperliquidClient, settings: &Config) -> Result<LoadedMarkets> {
    let mut coins: Vec<String> = settings
        .get("trading.symbols")
        .context("trading.symbols is required for Hyperliquid live mode")?;
    coins
        .iter_mut()
        .for_each(|coin| *coin = coin.trim().to_string());
    coins.sort();
    coins.dedup();
    if coins.is_empty() {
        bail!("trading.symbols must not be empty");
    }
    let resolved = client.fetch_markets(&coins).await.map_err(|market_error| {
        safe_hl_error("failed to resolve Hyperliquid markets", &market_error)
    })?;
    if resolved.len() != coins.len() {
        bail!("Hyperliquid market resolution returned an unexpected coin count");
    }
    let mut dexs: Vec<String> = Vec::new();
    let mut market_ids = HashMap::new();
    let mut markets = HashMap::new();
    for market in resolved {
        if !dexs.contains(&market.dex) {
            dexs.push(market.dex.clone());
        }
        market_ids.insert(market.coin.clone(), market.asset);
        markets.insert(market.asset, market);
    }
    Ok((coins, dexs, market_ids, markets))
}

/// Apply the configured isolated leverage to every coin without an open
/// position. Coins holding a position keep their existing leverage: changing
/// margin on live risk is never done automatically.
async fn apply_isolated_leverage(
    client: &HyperliquidClient,
    settings: &Config,
    markets: &HashMap<u32, HyperliquidMarket>,
    positions: &[TrackedPosition],
) -> Result<()> {
    let configured = settings.get_int("trading.isolated_leverage").unwrap_or(1);
    let configured = u32::try_from(configured.max(1)).unwrap_or(1);
    for market in markets.values() {
        let has_position = positions
            .iter()
            .any(|position| position.coin == market.coin && position.szi.abs() > 1e-12);
        if has_position {
            continue;
        }
        let leverage = configured.min(market.max_leverage.max(1));
        match client.update_leverage(market.asset, false, leverage).await {
            Ok(()) => {}
            Err(leverage_error)
                if matches!(
                    &leverage_error,
                    HyperliquidError::ActionRejected { message }
                        if l1_request_budget_exhausted(message)
                ) =>
            {
                warn!(
                    "Skipping isolated leverage update for {} because the L1 request budget is exhausted",
                    market.coin
                );
                return Ok(());
            }
            Err(leverage_error) => {
                return Err(safe_hl_error(
                    &format!("failed to set isolated leverage for {}", market.coin),
                    &leverage_error,
                ));
            }
        }
    }
    Ok(())
}

async fn fetch_clearinghouse_states(
    client: &HyperliquidClient,
    user: &str,
    dexs: &[String],
) -> Result<Vec<ClearinghouseState>> {
    let mut states = Vec::with_capacity(dexs.len());
    for dex in dexs {
        let state = client
            .clearinghouse_state(user, dex)
            .await
            .map_err(|state_error| {
                safe_hl_error(
                    &format!("Hyperliquid clearinghouse refresh failed for dex {dex:?}"),
                    &state_error,
                )
            })?;
        states.push(state);
    }
    Ok(states)
}

fn collect_positions(states: &[ClearinghouseState]) -> Result<Vec<TrackedPosition>> {
    let mut positions = Vec::new();
    for state in states {
        for entry in &state.asset_positions {
            positions.push(tracked_position(&entry.position)?);
        }
    }
    Ok(positions)
}

async fn fetch_open_orders(
    client: &HyperliquidClient,
    user: &str,
    dexs: &[String],
) -> Result<Vec<OpenOrder>> {
    let mut orders = Vec::new();
    for dex in dexs {
        let mut dex_orders = client
            .open_orders(user, dex)
            .await
            .map_err(|orders_error| {
                safe_hl_error(
                    &format!("Hyperliquid openOrders refresh failed for dex {dex:?}"),
                    &orders_error,
                )
            })?;
        orders.append(&mut dex_orders);
    }
    Ok(orders)
}

/// Cancel every listed order that belongs to a configured coin.
async fn cancel_open_orders(
    client: &HyperliquidClient,
    market_ids: &HashMap<String, u32>,
    orders: &[OpenOrder],
) -> Result<()> {
    let cancels: Vec<(u32, u64)> = orders
        .iter()
        .filter_map(|order| market_ids.get(&order.coin).map(|asset| (*asset, order.oid)))
        .collect();
    if cancels.is_empty() {
        return Ok(());
    }
    let results = client
        .cancel_orders(&cancels)
        .await
        .map_err(|cancel_error| safe_hl_error("Hyperliquid cancel batch failed", &cancel_error))?;
    for ((_, oid), result) in cancels.iter().zip(results) {
        if let Err(message) = result {
            // "already canceled or filled" style rejections mean the order is
            // gone, which is exactly what a cancel-all wants.
            if !cancel_error_means_gone(&message) {
                bail!("Hyperliquid cancel for oid {oid} was rejected: {message}");
            }
        }
    }
    Ok(())
}

pub(crate) fn cancel_error_means_gone(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("never placed") || lower.contains("already canceled") || lower.contains("filled")
}

async fn cancel_all_configured(
    client: &HyperliquidClient,
    user: &str,
    dexs: &[String],
    market_ids: &HashMap<String, u32>,
) -> Result<()> {
    let orders = fetch_open_orders(client, user, dexs).await?;
    cancel_open_orders(client, market_ids, &orders).await
}

async fn preload_candles(
    client: &HyperliquidClient,
    coins: &[String],
    store: &Arc<RwLock<data::storage::MarketDataStore>>,
) -> Result<()> {
    let now = unix_millis();
    let start = now.saturating_sub(100 * 60 * 60 * 1_000);
    for coin in coins {
        let candles = client
            .candle_snapshot(coin, "1h", start, now)
            .await
            .map_err(|candle_error| {
                safe_hl_error(
                    &format!("failed to preload Hyperliquid candles for {coin}"),
                    &candle_error,
                )
            })?;
        let mut store = store.write().await;
        for candle in candles {
            let timestamp = Utc
                .timestamp_millis_opt(candle.open_time as i64)
                .single()
                .context("Hyperliquid candle timestamp is out of range")?;
            store.add_candle(lighter::types::Candlestick {
                timestamp,
                open: parse_f64(&candle.open)?,
                high: parse_f64(&candle.high)?,
                low: parse_f64(&candle.low)?,
                close: parse_f64(&candle.close)?,
                volume: parse_f64(&candle.volume)?,
                symbol: coin.clone(),
            });
        }
    }
    Ok(())
}

pub(crate) fn subscription_messages(coins: &[String], user: &str) -> Vec<String> {
    let mut messages: Vec<String> = coins
        .iter()
        .map(|coin| Subscription::Bbo { coin: coin.clone() }.subscribe_message())
        .collect();
    messages.push(
        Subscription::OrderUpdates {
            user: user.to_string(),
        }
        .subscribe_message(),
    );
    messages.push(
        Subscription::UserFills {
            user: user.to_string(),
        }
        .subscribe_message(),
    );
    messages
}

async fn restore_strategy(
    network: &str,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    execution_strategy: &Arc<RwLock<Box<dyn strategy::Strategy>>>,
) -> Result<()> {
    let Some(saved) = dashboard::server::PersistentStrategyConfig::load(network) else {
        return Ok(());
    };
    if !maker_strategy_allowed(&saved.strategy_name) {
        let mut dashboard = dashboard_state.write().await;
        dashboard.trading_paused = true;
        warn!(
            "persisted Hyperliquid strategy {} is not maker_quote; ignored and trading remains paused",
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
    network: &str,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    risk_manager: &mut risk::risk_manager::RiskManager,
    ceilings: RiskCeilings,
) {
    let Some(saved) = dashboard::server::PersistentRiskConfig::load(network) else {
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
    client: &HyperliquidClient,
    user: &str,
    network: &str,
    dexs: &[String],
    market_ids: &HashMap<String, u32>,
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
            warn!("rejected non-maker Hyperliquid strategy update {name}; trading paused");
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
                Err(strategy_error) => {
                    dashboard_state.write().await.trading_paused = true;
                    warn!(
                        "invalid Hyperliquid maker strategy update: {strategy_error}; trading paused"
                    );
                }
            }
        }
    }
    let _ = network;
    let configured: HashSet<u32> = dashboard_state
        .read()
        .await
        .available_markets
        .iter()
        .map(|(id, _)| *id)
        .collect();
    if active_markets.iter().any(|id| !configured.contains(id)) {
        dashboard_state.write().await.trading_paused = true;
        bail!("dashboard selected an unknown Hyperliquid active market");
    }
    let active_now: HashSet<u32> = active_markets.iter().copied().collect();
    if active_now != *last_active_markets {
        let deactivated: Vec<u32> = last_active_markets
            .difference(&active_now)
            .copied()
            .collect();
        if !deactivated.is_empty() {
            let orders = fetch_open_orders(client, user, dexs).await?;
            let deactivated_orders: Vec<OpenOrder> = orders
                .into_iter()
                .filter(|order| {
                    market_ids
                        .get(&order.coin)
                        .is_some_and(|asset| deactivated.contains(asset))
                })
                .collect();
            cancel_open_orders(client, market_ids, &deactivated_orders).await?;
        }
        *last_active_markets = active_now;
    }
    let now_paused = dashboard_state.read().await.trading_paused;
    if cancel_requested || (now_paused && !*was_paused) {
        cancel_all_configured(client, user, dexs, market_ids).await?;
        let orders = fetch_open_orders(client, user, dexs)
            .await
            .context("Hyperliquid cancel-all verification failed")?;
        let remaining: Vec<OpenOrder> = orders
            .into_iter()
            .filter(|order| market_ids.contains_key(&order.coin))
            .collect();
        tracker.reconcile_open_orders(&remaining);
        if !remaining.is_empty() {
            bail!("Hyperliquid cancel-all verification found configured orders still open");
        }
    }
    if now_paused && !*was_paused {
        info!("Hyperliquid trading paused; cancel-all accepted");
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

fn maker_dashboard_params(settings: &Config) -> HashMap<String, String> {
    let prefix = "trading.strategies.maker_quote";
    let mut params = HashMap::new();
    let floats = [
        ("spread_bps", 30.0),
        ("per_quote_notional", 12.0),
        ("requote_threshold_bps", 2.0),
        ("soft_cap_notional", 30.0),
        ("hard_cap_notional", 60.0),
        ("trend_block_bps", 6.0),
        ("min_quote_notional", 10.0),
        ("vol_multiplier", 0.5),
        ("jump_circuit_breaker_bps", 20.0),
        ("min_book_spread_bps", 0.0),
        ("max_book_spread_bps", 40.0),
        ("wide_book_size_mult", 1.0),
        ("max_bbo_imbalance", 0.0),
        ("max_skew_bps", 3.0),
        ("total_quote_budget", 24.0),
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
        ("join_inside_ticks", 2),
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
    params.insert(
        "quote_mode".to_string(),
        settings
            .get_string(&format!("{prefix}.quote_mode"))
            .unwrap_or_else(|_| "mid_spread".to_string()),
    );
    params
}

#[allow(clippy::too_many_arguments)]
async fn handle_bbo(
    update: multi_venue_quant_bot::hyperliquid::BboUpdate,
    client: &HyperliquidClient,
    user: &str,
    markets: &HashMap<u32, HyperliquidMarket>,
    market_ids: &HashMap<String, u32>,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    data_store: &Arc<RwLock<data::storage::MarketDataStore>>,
    execution_strategy: &Arc<RwLock<Box<dyn strategy::Strategy>>>,
    risk_manager: &mut risk::risk_manager::RiskManager,
    positions: &[TrackedPosition],
    position_sync_pending: &HashSet<String>,
    rest_orders: &mut Vec<OpenOrder>,
    tracker: &mut OrderTracker,
    max_open_orders: usize,
) -> Result<()> {
    let coin = update.coin.clone();
    let market_id = *market_ids
        .get(&coin)
        .with_context(|| format!("Hyperliquid BBO referenced unconfigured coin {coin}"))?;
    let (Some(bid_level), Some(ask_level)) = (update.bid(), update.ask()) else {
        // One-sided books happen on thin HIP-3 markets; never quote into them.
        return Ok(());
    };
    let bid = parse_f64(&bid_level.px)?;
    let ask = parse_f64(&ask_level.px)?;
    if !(bid > 0.0 && ask > bid) {
        bail!("invalid Hyperliquid BBO for {coin}");
    }
    let bid_quantity = parse_f64(&bid_level.sz)?;
    let ask_quantity = parse_f64(&ask_level.sz)?;
    data_store
        .write()
        .await
        .update_order_book(lighter::types::OrderBook {
            symbol: coin.clone(),
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
        .insert(coin.clone(), (bid + ask) / 2.0);
    // Keep the book/UI fresh, but do not evaluate or sign while the L1
    // request budget is in backoff. Otherwise join-best retries every BBO.
    if l1_budget_is_blocked() {
        return Ok(());
    }
    let mut snapshot = data_store.read().await.get_snapshot();
    for position in positions {
        snapshot
            .positions
            .insert(position.coin.clone(), position.szi);
        snapshot
            .position_entry_prices
            .insert(position.coin.clone(), position.entry_px);
    }
    snapshot.positions_authoritative = true;
    for local in tracker.by_strategy_key.values() {
        snapshot.open_orders.push(lighter::types::OpenOrderRef {
            symbol: local.coin.clone(),
            client_id: Some(local.strategy_key.clone()),
            side: if local.is_buy {
                lighter::types::Side::Buy
            } else {
                lighter::types::Side::Sell
            },
            price: local.price,
            quantity: local.quantity,
            status: format!("{:?}", local.status),
        });
    }
    snapshot.open_orders_authoritative = true;
    let evaluated = execution_strategy.read().await.evaluate(&snapshot).await?;
    let Some(signals) = evaluated else {
        return Ok(());
    };
    for signal in signals {
        let position_pending = position_sync_pending.contains(&signal.symbol);
        if position_pending {
            continue;
        }
        let (paused, active) = {
            let dashboard = dashboard_state.read().await;
            (dashboard.trading_paused, dashboard.active_markets.clone())
        };
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
            user,
            dashboard_state,
            markets
                .get(&signal_market_id)
                .context("Hyperliquid strategy emitted unknown market id")?,
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
    client: &HyperliquidClient,
    user: &str,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    market: &HyperliquidMarket,
    market_id: u32,
    mut signal: lighter::types::TradeSignal,
    positions: &[TrackedPosition],
    rest_orders: &mut Vec<OpenOrder>,
    tracker: &mut OrderTracker,
    risk_manager: &mut risk::risk_manager::RiskManager,
    max_open_orders: usize,
) -> Result<()> {
    if l1_budget_is_blocked() {
        return Ok(());
    }
    let is_buy = signal.side == lighter::types::Side::Buy;
    let strategy_key = signal.client_id.clone().unwrap_or_else(|| {
        format!(
            "mq_{}_{}",
            signal.symbol,
            if is_buy { "buy" } else { "sell" }
        )
    });
    if signal.action == lighter::types::SignalAction::Cancel {
        if let Some(existing) = tracker.by_strategy_key.get(&strategy_key).cloned() {
            cancel_tracked_order(
                client,
                user,
                tracker,
                rest_orders,
                &existing,
                &market.coin,
                market.asset,
            )
            .await?;
        }
        return Ok(());
    }
    if !signal.post_only && !signal.risk_reducing {
        bail!("Hyperliquid rejected a non-maker, non-risk-reducing placement intent");
    }
    let existing = tracker.by_strategy_key.get(&strategy_key).cloned();
    if let Some(existing) = existing.as_ref() {
        match quote_replace_decision(existing, signal.price) {
            QuoteReplaceDecision::Noop => return Ok(()),
            QuoteReplaceDecision::BlockedUnresolved => {
                bail!("Hyperliquid quote replacement blocked by unresolved prior order");
            }
            QuoteReplaceDecision::CancelThenWait => {}
        }
    }
    if existing.is_none() && !signal.risk_reducing {
        let count = rest_orders.len() + tracker.local_open_count_not_in_rest(rest_orders);
        if count >= max_open_orders {
            return Ok(());
        }
    }
    if signal.risk_reducing {
        let held = positions
            .iter()
            .find(|position| position.coin == signal.symbol)
            .map(|position| position.szi)
            .unwrap_or(0.0);
        let closes = (held > 0.0 && !is_buy) || (held < 0.0 && is_buy);
        if !closes {
            return Ok(());
        }
        signal.quantity = signal.quantity.min(held.abs());
    }
    let exposure = calculate_exposure(
        &exposure_input(positions, rest_orders, tracker),
        &signal.symbol,
    );
    if !risk_manager
        .check_signal_with_exposure(&signal, exposure)
        .await?
    {
        return Ok(());
    }
    // Maker bids floor, maker asks ceil (never cross); IOC exits round toward
    // the aggressive side so quantization cannot weaken the exit.
    let round_up = signal.post_only != is_buy;
    let price_text = market
        .quantize_price(signal.price, round_up)
        .map_err(|quant_error| {
            safe_hl_error("Hyperliquid price quantization failed", &quant_error)
        })?;
    let size_text = match market.quantize_size(signal.quantity) {
        Ok(text) => text,
        Err(_) => return Ok(()),
    };
    let quantized_price = parse_f64(&price_text)?;
    let quantized_size = parse_f64(&size_text)?;
    if signal.post_only && quantized_price * quantized_size < MIN_ORDER_NOTIONAL_USD {
        return Ok(());
    }
    if let Some(existing) = existing {
        match quote_replace_decision(&existing, quantized_price) {
            QuoteReplaceDecision::Noop => return Ok(()),
            QuoteReplaceDecision::CancelThenWait => {
                cancel_tracked_order(
                    client,
                    user,
                    tracker,
                    rest_orders,
                    &existing,
                    &market.coin,
                    market.asset,
                )
                .await?;
                return Ok(());
            }
            QuoteReplaceDecision::BlockedUnresolved => {
                bail!("Hyperliquid quote replacement reached an impossible state");
            }
        }
    }
    let cloid = cloid_from_key(&format!("{strategy_key}:{}", unix_millis()));
    let request = NewOrderRequest {
        asset: market.asset,
        is_buy,
        price: price_text.clone(),
        size: size_text.clone(),
        reduce_only: signal.risk_reducing,
        tif: if signal.post_only { Tif::Alo } else { Tif::Ioc },
        cloid: Some(cloid.clone()),
    };
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
        cloid: cloid.clone(),
        oid: None,
        coin: signal.symbol.clone(),
        is_buy,
        price: quantized_price,
        quantity: quantized_size,
        status: LocalOrderStatus::Pending,
        last_status_time: 0,
    });
    let result = match client.place_order(&request).await {
        Ok(OrderOutcome::Resting { oid, .. }) => {
            if let Some(local) = tracker.by_strategy_key.get_mut(&strategy_key) {
                local.oid = Some(oid);
                local.status = LocalOrderStatus::Live;
            }
            tracker.by_oid.insert(oid, strategy_key.clone());
            rest_orders.push(OpenOrder {
                coin: signal.symbol.clone(),
                side: if is_buy { "B" } else { "A" }.to_string(),
                limit_px: price_text,
                sz: size_text.clone(),
                oid,
                timestamp: unix_millis(),
                orig_sz: size_text,
                cloid: Some(cloid.clone()),
                reduce_only: signal.risk_reducing,
                order_type: "Limit".to_string(),
            });
            Ok(())
        }
        Ok(OrderOutcome::Filled { .. }) => {
            // IOC exits fill immediately; position sync follows via userFills.
            tracker.remove(&strategy_key);
            Ok(())
        }
        Ok(OrderOutcome::Error(message)) => {
            // Per-order rejections (post-only would cross, margin, min size)
            // are normal churn: drop the local mirror and wait for the next tick.
            tracker.remove(&strategy_key);
            warn!(
                "Hyperliquid rejected order for {}: {message}",
                signal.symbol
            );
            Ok(())
        }
        Err(submit_error) => {
            match submission_failure_decision(&submit_error) {
                SubmissionFailureDecision::Skip => {
                    tracker.remove(&strategy_key);
                    trip_l1_budget_backoff();
                    warn!(
                        "{}",
                        safe_hl_error(
                            &format!(
                                "Hyperliquid skipped order for {} after L1 request-budget reject",
                                signal.symbol
                            ),
                            &submit_error,
                        )
                    );
                    return Ok(());
                }
                SubmissionFailureDecision::Reject => {
                    tracker.remove(&strategy_key);
                    return Err(safe_hl_error(
                        "Hyperliquid order was rejected",
                        &submit_error,
                    ));
                }
                SubmissionFailureDecision::Reconcile => {}
            }
            if let Some(local) = tracker.by_strategy_key.get_mut(&strategy_key) {
                local.status = LocalOrderStatus::Unknown;
            }
            reconcile_unknown_submission(client, user, &cloid, &strategy_key, rest_orders, tracker)
                .await
                .map_err(|reconcile_error| {
                    anyhow::anyhow!(
                        "{}; {}",
                        safe_hl_error("Hyperliquid order status remained unknown", &submit_error),
                        reconcile_error
                    )
                })
        }
    };
    drop(placement_guard);
    result
}

fn exposure_input(
    positions: &[TrackedPosition],
    rest_orders: &[OpenOrder],
    tracker: &OrderTracker,
) -> ExposureInput {
    ExposureInput {
        positions: positions
            .iter()
            .map(|position| (position.coin.clone(), position.szi, position.mark_px))
            .collect(),
        rest_orders: rest_orders
            .iter()
            .map(|order| {
                (
                    order.coin.clone(),
                    order.cloid.clone().unwrap_or_default(),
                    Some(order.oid),
                    if order.is_buy() {
                        multi_venue_quant_bot::aster::OrderSide::Buy
                    } else {
                        multi_venue_quant_bot::aster::OrderSide::Sell
                    },
                    parse_f64(&order.limit_px).unwrap_or(0.0),
                    parse_f64(&order.orig_sz).unwrap_or(0.0),
                )
            })
            .collect(),
        local_orders: tracker
            .by_strategy_key
            .values()
            .map(|local| crate::aster_live::LocalOrder {
                strategy_key: local.strategy_key.clone(),
                exchange_client_id: local.cloid.clone(),
                order_id: local.oid,
                symbol: local.coin.clone(),
                side: if local.is_buy {
                    multi_venue_quant_bot::aster::OrderSide::Buy
                } else {
                    multi_venue_quant_bot::aster::OrderSide::Sell
                },
                price: local.price,
                quantity: local.quantity,
                status: match local.status {
                    LocalOrderStatus::Pending => crate::aster_live::LocalOrderStatus::Pending,
                    LocalOrderStatus::Live => crate::aster_live::LocalOrderStatus::Live,
                    LocalOrderStatus::Unknown => crate::aster_live::LocalOrderStatus::Unknown,
                },
                last_event_time: 0,
                last_transaction_time: 0,
            })
            .collect(),
    }
}

pub(crate) fn cancel_asset_for_tracked_order(
    existing: &LocalOrder,
    market_coin: &str,
    market_asset: u32,
) -> Result<u32> {
    if existing.coin != market_coin {
        bail!(
            "tracked Hyperliquid order coin {} does not match market {}",
            existing.coin,
            market_coin
        );
    }
    Ok(market_asset)
}

async fn cancel_tracked_order(
    client: &HyperliquidClient,
    user: &str,
    tracker: &mut OrderTracker,
    rest_orders: &mut Vec<OpenOrder>,
    existing: &LocalOrder,
    market_coin: &str,
    market_asset: u32,
) -> Result<()> {
    let asset = cancel_asset_for_tracked_order(existing, market_coin, market_asset)?;
    let cancel_result = match existing.oid {
        Some(oid) => client.cancel_orders(&[(asset, oid)]).await,
        None => {
            client
                .cancel_orders_by_cloid(&[(asset, existing.cloid.clone())])
                .await
        }
    };
    match cancel_result {
        Ok(results) => match results.into_iter().next() {
            Some(Ok(())) => {
                tracker.remove(&existing.strategy_key);
                remove_rest_order(rest_orders, existing);
                Ok(())
            }
            Some(Err(message)) if cancel_error_means_gone(&message) => {
                tracker.remove(&existing.strategy_key);
                remove_rest_order(rest_orders, existing);
                Ok(())
            }
            Some(Err(message)) => {
                warn!(
                        "Hyperliquid cancel for {} was rejected: {message}; order remains and replacement is blocked",
                        existing.coin
                    );
                Ok(())
            }
            None => bail!("Hyperliquid cancel returned no status"),
        },
        Err(cancel_error) => {
            // The cancel may or may not have landed: prove the order's state.
            match client
                .order_status(user, OrderId::Cloid(existing.cloid.clone()))
                .await
            {
                Ok(status) if status.status == "order" => {
                    let is_open = status
                        .order
                        .as_ref()
                        .is_some_and(|entry| entry.status == "open");
                    if is_open {
                        warn!(
                            "{}; order remains live and replacement is blocked",
                            safe_hl_error("Hyperliquid cancel failed", &cancel_error)
                        );
                    } else {
                        tracker.remove(&existing.strategy_key);
                        remove_rest_order(rest_orders, existing);
                    }
                    Ok(())
                }
                Ok(_) => {
                    tracker.remove(&existing.strategy_key);
                    remove_rest_order(rest_orders, existing);
                    Ok(())
                }
                Err(status_error) => Err(anyhow::anyhow!(
                    "{}; {}",
                    safe_hl_error("Hyperliquid cancel failed", &cancel_error),
                    safe_hl_error("Hyperliquid status query failed", &status_error)
                )),
            }
        }
    }
}

fn remove_rest_order(rest_orders: &mut Vec<OpenOrder>, existing: &LocalOrder) {
    rest_orders.retain(|order| {
        order.cloid.as_deref() != Some(existing.cloid.as_str()) && existing.oid != Some(order.oid)
    });
}

async fn reconcile_unknown_submission(
    client: &HyperliquidClient,
    user: &str,
    cloid: &str,
    strategy_key: &str,
    rest_orders: &mut Vec<OpenOrder>,
    tracker: &mut OrderTracker,
) -> Result<()> {
    let status = client
        .order_status(user, OrderId::Cloid(cloid.to_string()))
        .await
        .map_err(|status_error| {
            safe_hl_error(
                "Hyperliquid orderStatus reconciliation failed",
                &status_error,
            )
        })?;
    if status.status == "order" {
        let Some(entry) = status.order else {
            bail!("Hyperliquid orderStatus returned no order body");
        };
        if entry.status == "open" {
            if let Some(local) = tracker.by_strategy_key.get_mut(strategy_key) {
                local.oid = Some(entry.order.oid);
                local.status = LocalOrderStatus::Live;
            }
            tracker
                .by_oid
                .insert(entry.order.oid, strategy_key.to_string());
            rest_orders.push(OpenOrder {
                coin: entry.order.coin.clone(),
                side: entry.order.side.clone(),
                limit_px: entry.order.limit_px.clone(),
                sz: entry.order.sz.clone(),
                oid: entry.order.oid,
                timestamp: entry.order.timestamp,
                orig_sz: entry.order.orig_sz.clone(),
                cloid: entry.order.cloid.clone(),
                reduce_only: false,
                order_type: "Limit".to_string(),
            });
        } else {
            tracker.remove(strategy_key);
        }
        return Ok(());
    }
    // unknownOid: the exchange has no record. The submission may still be in
    // flight, which cannot be distinguished here - fail closed.
    tracker.remove(strategy_key);
    bail!("Hyperliquid could not prove the ambiguous order's state (unknownOid)")
}

#[allow(clippy::too_many_arguments)]
async fn enforce_risk_gates(
    client: &HyperliquidClient,
    user: &str,
    dexs: &[String],
    markets: &HashMap<u32, HyperliquidMarket>,
    market_ids: &HashMap<String, u32>,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    positions: &[TrackedPosition],
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
        .map(|position| (position.coin.clone(), position.mark_px))
        .collect();
    let risk_positions: Vec<lighter::types::Position> = positions
        .iter()
        .filter(|position| position.szi.abs() > 1e-12)
        .map(|position| lighter::types::Position {
            symbol: position.coin.clone(),
            side: if position.szi > 0.0 {
                lighter::types::Side::Buy
            } else {
                lighter::types::Side::Sell
            },
            size: position.szi.abs(),
            entry_price: position.entry_px,
            unrealized_pnl: position.unrealized_pnl,
            leverage: position.leverage_value,
        })
        .collect();
    let position_closes =
        risk_manager.check_position_stop_loss_take_profit(&risk_positions, &current_prices);
    if status.is_healthy && !emergency && position_closes.is_empty() {
        return Ok(());
    }
    if emergency && !risk_manager.is_emergency_triggered() {
        risk_manager.set_emergency_triggered();
    }
    dashboard_state.write().await.trading_paused = true;
    cancel_all_configured(client, user, dexs, market_ids).await?;

    let close_requests: Vec<(String, bool, f64, f64)> = if emergency {
        positions
            .iter()
            .filter(|position| position.szi.abs() > 1e-12)
            .map(|position| {
                (
                    position.coin.clone(),
                    position.szi < 0.0,
                    position.mark_px,
                    position.szi.abs(),
                )
            })
            .collect()
    } else {
        position_closes
            .into_iter()
            .map(|close| {
                (
                    close.symbol,
                    close.side_to_close == lighter::types::Side::Buy,
                    close.current_price,
                    close.size,
                )
            })
            .collect()
    };
    for (coin, exit_is_buy, mark, quantity) in close_requests {
        let market_id = *market_ids
            .get(&coin)
            .with_context(|| format!("risk position {coin} is not configured"))?;
        let aggressive = if exit_is_buy {
            mark * 1.005
        } else {
            mark * 0.995
        };
        place_risk_exit(
            client,
            markets
                .get(&market_id)
                .context("missing Hyperliquid market")?,
            &coin,
            exit_is_buy,
            aggressive,
            quantity,
            tracker,
            user,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn place_risk_exit(
    client: &HyperliquidClient,
    market: &HyperliquidMarket,
    coin: &str,
    is_buy: bool,
    price: f64,
    quantity: f64,
    tracker: &mut OrderTracker,
    user: &str,
) -> Result<()> {
    let strategy_key = format!("risk_{coin}_{}", if is_buy { "buy" } else { "sell" });
    if tracker.by_strategy_key.contains_key(&strategy_key) {
        return Ok(());
    }
    let price_text = market
        .quantize_price(price, is_buy)
        .map_err(|quant_error| safe_hl_error("Hyperliquid risk-exit price failed", &quant_error))?;
    let size_text = market
        .quantize_size(quantity)
        .map_err(|quant_error| safe_hl_error("Hyperliquid risk-exit size failed", &quant_error))?;
    let cloid = cloid_from_key(&format!("{strategy_key}:{}", unix_millis()));
    let request = NewOrderRequest {
        asset: market.asset,
        is_buy,
        price: price_text.clone(),
        size: size_text.clone(),
        reduce_only: true,
        tif: Tif::Ioc,
        cloid: Some(cloid.clone()),
    };
    tracker.insert(LocalOrder {
        strategy_key: strategy_key.clone(),
        cloid: cloid.clone(),
        oid: None,
        coin: coin.to_string(),
        is_buy,
        price: parse_f64(&price_text)?,
        quantity: parse_f64(&size_text)?,
        status: LocalOrderStatus::Pending,
        last_status_time: 0,
    });
    match client.place_order(&request).await {
        Ok(OrderOutcome::Filled { .. }) | Ok(OrderOutcome::Resting { .. }) => {
            tracker.remove(&strategy_key);
            Ok(())
        }
        Ok(OrderOutcome::Error(message)) => {
            tracker.remove(&strategy_key);
            let lower = message.to_ascii_lowercase();
            // Dust below the venue minimum cannot be force-closed; leave the
            // remainder for manual handling instead of wedging the loop.
            if lower.contains("minimum") || lower.contains("reduce only") {
                warn!("Hyperliquid risk exit for {coin} was rejected: {message}");
                return Ok(());
            }
            bail!("Hyperliquid risk exit for {coin} was rejected: {message}");
        }
        Err(exit_error)
            if submission_failure_decision(&exit_error) == SubmissionFailureDecision::Reconcile =>
        {
            if let Some(local) = tracker.by_strategy_key.get_mut(&strategy_key) {
                local.status = LocalOrderStatus::Unknown;
            }
            let mut orders = Vec::new();
            reconcile_unknown_submission(client, user, &cloid, &strategy_key, &mut orders, tracker)
                .await
        }
        Err(exit_error)
            if submission_failure_decision(&exit_error) == SubmissionFailureDecision::Skip =>
        {
            tracker.remove(&strategy_key);
            trip_l1_budget_backoff();
            warn!(
                "{}",
                safe_hl_error(
                    &format!(
                        "Hyperliquid skipped risk exit for {coin} after L1 request-budget reject"
                    ),
                    &exit_error,
                )
            );
            Ok(())
        }
        Err(exit_error) => {
            tracker.remove(&strategy_key);
            Err(safe_hl_error("Hyperliquid risk exit failed", &exit_error))
        }
    }
}

async fn update_dashboard_account(
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    states: &[ClearinghouseState],
    unified_spot_usdc: Option<(f64, f64)>,
    positions: &[TrackedPosition],
    orders: &[OpenOrder],
    risk_manager: &mut risk::risk_manager::RiskManager,
) -> Result<()> {
    let (equity, available_balance, unrealized_pnl) = account_totals(states, unified_spot_usdc)?;
    risk_manager.update_equity(equity);
    let mut dashboard = dashboard_state.write().await;
    dashboard.equity = equity;
    dashboard.available_balance = available_balance;
    dashboard.unrealized_pnl = unrealized_pnl;
    dashboard.positions = positions_json(positions);
    dashboard.open_orders = orders.len() as u32;
    dashboard.open_orders_list = orders
        .iter()
        .map(|order| {
            serde_json::json!({
                "id": order.oid,
                "client_id": order.cloid.clone().unwrap_or_default(),
                "symbol": order.coin,
                "side": if order.is_buy() { "BUY" } else { "SELL" },
                "price": parse_f64(&order.limit_px).unwrap_or(0.0),
                "quantity": parse_f64(&order.orig_sz).unwrap_or(0.0),
                "filled_quantity": parse_f64(&order.orig_sz).unwrap_or(0.0)
                    - parse_f64(&order.sz).unwrap_or(0.0),
                "status": "NEW",
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

#[allow(clippy::too_many_arguments)]
async fn reconcile_account_history(
    client: &HyperliquidClient,
    user: &str,
    network: &str,
    coins: &[String],
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    ledger: &mut HyperliquidLedger,
    baseline_only: bool,
) -> Result<()> {
    let now = unix_millis();
    let first_start = now.saturating_sub(HISTORY_LOOKBACK_MS);
    let fills_start = if ledger.fill_high_water_ms == 0 {
        first_start
    } else {
        ledger.fill_high_water_ms.saturating_sub(HISTORY_OVERLAP_MS)
    };
    let mut fills: Vec<Fill> = Vec::new();
    let mut cursor = fills_start;
    loop {
        let page = client
            .user_fills_by_time(user, cursor, None)
            .await
            .map_err(|fills_error| {
                safe_hl_error("failed to reconcile Hyperliquid userFills", &fills_error)
            })?;
        let page_len = page.len();
        let highest = page.iter().map(|fill| fill.time).max();
        fills.extend(page);
        // userFillsByTime returns at most 2000 rows per request.
        if page_len < 2_000 {
            break;
        }
        let highest = highest.context("Hyperliquid fill pagination did not advance")?;
        if highest <= cursor {
            bail!("Hyperliquid fill pagination cursor did not advance");
        }
        // Restart at the boundary millisecond; tid dedupe absorbs the overlap.
        cursor = highest;
    }
    let funding_start = if ledger.funding_high_water_ms == 0 {
        first_start
    } else {
        ledger
            .funding_high_water_ms
            .saturating_sub(HISTORY_OVERLAP_MS)
    };
    let mut funding: Vec<UserFundingEntry> = Vec::new();
    let mut funding_cursor = funding_start;
    loop {
        let page = client
            .user_funding(user, funding_cursor)
            .await
            .map_err(|funding_error| {
                safe_hl_error("failed to reconcile Hyperliquid funding", &funding_error)
            })?;
        let page_len = page.len();
        let highest = page.iter().map(|entry| entry.time).max();
        funding.extend(page);
        if page_len < 500 {
            break;
        }
        let highest = highest.context("Hyperliquid funding pagination did not advance")?;
        if highest <= funding_cursor {
            bail!("Hyperliquid funding pagination cursor did not advance");
        }
        funding_cursor = highest;
    }
    fills.sort_by_key(|fill| fill.time);
    funding.sort_by_key(|entry| entry.time);
    let mut dashboard = dashboard_state.write().await;
    for fill in fills {
        if !coins.contains(&fill.coin) {
            continue;
        }
        let is_new = ledger.record_fill(fill.time, fill.tid);
        if !is_new || baseline_only {
            continue;
        }
        apply_rest_fill(&mut dashboard, &fill)?;
    }
    for entry in funding {
        if !coins.contains(&entry.delta.coin) {
            continue;
        }
        let is_new = ledger.record_funding(&entry);
        if !is_new || baseline_only {
            continue;
        }
        apply_funding(&mut dashboard, &entry)?;
    }
    dashboard.total_trades = ledger.seen_fill_tids.len() as u64;
    if baseline_only {
        warn!(
            "Hyperliquid ledger was missing while pnl_state existed; initialized seven-day seen-ID baseline without adding historical PnL"
        );
    }
    ledger.pnl_checkpoint = Some(pnl_snapshot(&dashboard));
    ledger.save_atomic(network)?;
    dashboard.save_pnl();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_fill_amounts(
    dashboard: &mut dashboard::server::DashboardState,
    coin: &str,
    is_buy: bool,
    crossed: bool,
    px: f64,
    sz: f64,
    closed_pnl: f64,
    fee: f64,
    time: u64,
    tid: u64,
    oid: u64,
) -> Result<()> {
    let net = closed_pnl - fee;
    dashboard.push_trade(serde_json::json!({
        "timestamp": Utc.timestamp_millis_opt(time as i64).single()
            .map(|value| value.to_rfc3339())
            .unwrap_or_else(|| time.to_string()),
        "trade_id": tid,
        "order_id": oid,
        "symbol": coin,
        "side": if is_buy { "BUY" } else { "SELL" },
        "price": px,
        "quantity": sz,
        "maker": !crossed,
        "pnl": net,
        "action": "Fill",
    }));
    dashboard.total_realized_pnl += net;
    let date = Utc
        .timestamp_millis_opt(time as i64)
        .single()
        .context("Hyperliquid fill timestamp out of range")?
        .format("%Y-%m-%d")
        .to_string();
    *dashboard.daily_pnl_map.entry(date.clone()).or_default() += net;
    if date == Utc::now().format("%Y-%m-%d").to_string() {
        dashboard.daily_realized_pnl += net;
    }
    Ok(())
}

fn apply_rest_fill(dashboard: &mut dashboard::server::DashboardState, fill: &Fill) -> Result<()> {
    apply_fill_amounts(
        dashboard,
        &fill.coin,
        fill.is_buy(),
        fill.crossed,
        parse_f64(&fill.px)?,
        parse_f64(&fill.sz)?,
        parse_f64(&fill.closed_pnl)?,
        parse_f64(&fill.fee)?,
        fill.time,
        fill.tid,
        fill.oid,
    )
}

fn apply_ws_fill(dashboard: &mut dashboard::server::DashboardState, fill: &WsFill) -> Result<()> {
    apply_fill_amounts(
        dashboard,
        &fill.coin,
        fill.is_buy(),
        fill.crossed,
        parse_f64(&fill.px)?,
        parse_f64(&fill.sz)?,
        parse_f64(&fill.closed_pnl)?,
        parse_f64(&fill.fee)?,
        fill.time,
        fill.tid,
        fill.oid,
    )
}

fn apply_funding(
    dashboard: &mut dashboard::server::DashboardState,
    entry: &UserFundingEntry,
) -> Result<()> {
    let amount = parse_f64(&entry.delta.usdc)?;
    dashboard.total_realized_pnl += amount;
    dashboard.total_funding_pnl += amount;
    let date = Utc
        .timestamp_millis_opt(entry.time as i64)
        .single()
        .context("Hyperliquid funding timestamp out of range")?
        .format("%Y-%m-%d")
        .to_string();
    *dashboard.daily_pnl_map.entry(date.clone()).or_default() += amount;
    if date == Utc::now().format("%Y-%m-%d").to_string() {
        dashboard.daily_realized_pnl += amount;
        dashboard.daily_funding_pnl += amount;
    }
    Ok(())
}

async fn safety_shutdown(
    client: &HyperliquidClient,
    user: &str,
    dexs: &[String],
    market_ids: &HashMap<String, u32>,
    dashboard_state: &Arc<RwLock<dashboard::server::DashboardState>>,
) {
    {
        let mut dashboard = dashboard_state.write().await;
        dashboard.trading_paused = true;
    }
    if let Err(shutdown_error) = cancel_all_configured(client, user, dexs, market_ids).await {
        error!("Hyperliquid safety cancel-all failed: {shutdown_error}");
    }
}

async fn probe_io_xyz_sndk_basis(
    client: &HyperliquidClient,
    dashboard_state: &std::sync::Arc<tokio::sync::RwLock<dashboard::server::DashboardState>>,
) {
    let io = match client.l2_book("io:SNDK").await {
        Ok(book) => book,
        Err(error) => {
            warn!("io/xyz SNDK basis probe skipped: {error}");
            return;
        }
    };
    let xyz = match client.l2_book("xyz:SNDK").await {
        Ok(book) => book,
        Err(error) => {
            warn!("io/xyz SNDK basis probe skipped: {error}");
            return;
        }
    };
    let Some((bid_a, ask_a)) = best_bid_ask(&io.levels) else {
        warn!("io/xyz SNDK basis probe skipped: io book empty");
        return;
    };
    let Some((bid_b, ask_b)) = best_bid_ask(&xyz.levels) else {
        warn!("io/xyz SNDK basis probe skipped: xyz book empty");
        return;
    };
    let Some(basis) = strategy::cross_dex_basis::crossed_basis_bps(bid_a, ask_a, bid_b, ask_b)
    else {
        warn!("io/xyz SNDK basis probe skipped: invalid BBO");
        return;
    };
    match strategy::cross_dex_basis::tradeable_edge_bps(
        basis,
        strategy::cross_dex_basis::hip3_cross_dex_taker_cost_bps(),
    ) {
        None => tracing::debug!(
            "io:SNDK vs xyz:SNDK crossed basis not tradeable after two taker fees (buy_io_sell_xyz={:.2} buy_xyz_sell_io={:.2} bps)",
            basis.buy_a_sell_b_bps, basis.buy_b_sell_a_bps
        ),
        Some((side, net)) => {
            warn!(
                "io:SNDK vs xyz:SNDK TRADEABLE {side} net {net:.2} bps io={bid_a}/{ask_a} xyz={bid_b}/{ask_b} — not armed, xyz is not a configured coin"
            );
            let mut dashboard = dashboard_state.write().await;
            dashboard.last_cross_dex_net_bps = Some((net * 100.0).round() / 100.0);
            dashboard.last_cross_dex_side = Some(side.to_string());
        }
    }
}

fn best_bid_ask(levels: &[Vec<multi_venue_quant_bot::hyperliquid::L2Level>]) -> Option<(f64, f64)> {
    let bid = levels.first()?.first()?.px.parse::<f64>().ok()?;
    let ask = levels.get(1)?.first()?.px.parse::<f64>().ok()?;
    if bid > 0.0 && ask > bid {
        Some((bid, ask))
    } else {
        None
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

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
