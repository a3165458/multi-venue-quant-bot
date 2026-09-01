use anyhow::{bail, Result};
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use futures::{SinkExt, StreamExt};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};
use tracing::{info, warn};

const PNL_STATE_FILE: &str = "pnl_state.json";
const OMP_COLLAB_URL_FILE: &str = ".omp/collab-url";

/// Max fills kept in the live trade history buffer (also used by /api/pnl).
/// Order-placement and close-event paths must share this constant — previously
/// one path kept 100 and the other 200, so older closes were silently dropped.
pub const TRADE_HISTORY_LIMIT: usize = 500;

/// Persistent PnL data that survives restarts
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersistentPnlData {
    pub total_realized_pnl: f64,
    /// Funding payments included in total realized PnL.
    #[serde(default)]
    pub total_funding_pnl: f64,
    #[serde(default)]
    pub daily_funding_pnl: f64,
    pub initial_equity: f64,
    pub peak_equity: f64,
    pub equity_history: Vec<(i64, f64)>,
    pub pnl_history: Vec<(i64, f64)>,
    pub trade_history: Vec<serde_json::Value>,
    /// Per-day realized PnL: key = "YYYY-MM-DD", value = realized pnl that day
    pub daily_pnl_map: std::collections::HashMap<String, f64>,
    /// Lifetime notional volume Σ|price × quantity| across every recorded fill.
    /// Survives the trade-history ring buffer so the History card does not
    /// under-report after old fills are dropped.
    #[serde(default)]
    pub total_volume: f64,
    /// Lifetime count of close events (Partial/Full/Stop/…).
    #[serde(default)]
    pub total_closed_trades: u64,
}

impl PersistentPnlData {
    pub fn load(network: &str) -> Option<Self> {
        let path = super::runtime_paths::data_file(network, PNL_STATE_FILE).ok()?;
        let data = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&data) {
            Ok(state) => {
                info!("📂 Loaded PnL state from {}", path.display());
                Some(state)
            }
            Err(e) => {
                warn!("⚠️ Failed to parse PnL state file: {}", e);
                None
            }
        }
    }

    pub fn save(&self, network: &str) {
        let Ok(path) = super::runtime_paths::data_file(network, PNL_STATE_FILE) else {
            warn!("⚠️ Refusing to save PnL for invalid network {network}");
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!("⚠️ Failed to save PnL state: {}", e);
                }
            }
            Err(e) => warn!("⚠️ Failed to serialize PnL state: {}", e),
        }
    }
}

const STRATEGY_CONFIG_FILE: &str = "strategy_config.json";
const RISK_CONFIG_FILE: &str = "risk_config.json";

/// Persistent strategy configuration that survives restarts
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersistentStrategyConfig {
    pub strategy_name: String,
    pub strategy_params: std::collections::HashMap<String, String>,
    /// Last dashboard pause/resume. `None` on old files falls back to yaml `start_paused`.
    #[serde(default)]
    pub trading_paused: Option<bool>,
}

impl PersistentStrategyConfig {
    pub fn save(ds: &DashboardState) {
        let Ok(path) = super::runtime_paths::data_file(&ds.network_name, STRATEGY_CONFIG_FILE)
        else {
            warn!(
                "⚠️ Refusing to save strategy for invalid network {}",
                ds.network_name
            );
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let config = PersistentStrategyConfig {
            strategy_name: ds.strategy_name.clone(),
            strategy_params: ds.strategy_params.clone(),
            trading_paused: Some(ds.trading_paused),
        };
        match serde_json::to_string_pretty(&config) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!("⚠️ Failed to save strategy config: {}", e);
                }
            }
            Err(e) => warn!("⚠️ Failed to serialize strategy config: {}", e),
        }
    }

    pub fn load(network: &str) -> Option<Self> {
        let path = super::runtime_paths::data_file(network, STRATEGY_CONFIG_FILE).ok()?;
        let data = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&data) {
            Ok(config) => {
                info!("📂 Loaded strategy config from {}", path.display());
                Some(config)
            }
            Err(e) => {
                warn!("⚠️ Failed to parse strategy config file: {}", e);
                None
            }
        }
    }

    pub fn exists(network: &str) -> bool {
        super::runtime_paths::data_file(network, STRATEGY_CONFIG_FILE)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }
}

/// Persistent risk configuration that survives restarts
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersistentRiskConfig {
    pub risk_config: serde_json::Value,
    pub leverage_limit: f64,
}

impl PersistentRiskConfig {
    pub fn save(ds: &DashboardState) {
        let Ok(path) = super::runtime_paths::data_file(&ds.network_name, RISK_CONFIG_FILE) else {
            warn!(
                "⚠️ Refusing to save risk config for invalid network {}",
                ds.network_name
            );
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let config = PersistentRiskConfig {
            risk_config: ds.risk_config.clone(),
            leverage_limit: ds.leverage_limit,
        };
        match serde_json::to_string_pretty(&config) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!("⚠️ Failed to save risk config: {}", e);
                }
            }
            Err(e) => warn!("⚠️ Failed to serialize risk config: {}", e),
        }
    }

    pub fn load(network: &str) -> Option<Self> {
        let path = super::runtime_paths::data_file(network, RISK_CONFIG_FILE).ok()?;
        let data = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&data) {
            Ok(config) => {
                info!("📂 Loaded risk config from {}", path.display());
                Some(config)
            }
            Err(e) => {
                warn!("⚠️ Failed to parse risk config file: {}", e);
                None
            }
        }
    }
}

/// Shared dashboard state
#[derive(Clone, Default)]
pub struct DashboardState {
    /// Network used by this process. Switching profiles requires a restart.
    pub network_name: String,
    pub rest_url: String,
    pub ws_url: String,
    pub chain_id: i32,
    pub equity: f64,
    pub available_balance: f64,
    pub unrealized_pnl: f64,
    pub strategy_name: String,
    pub total_trades: u64,
    pub open_orders: u32,
    pub open_orders_list: Vec<serde_json::Value>,
    pub positions: Vec<serde_json::Value>,
    pub trade_history: Vec<serde_json::Value>,
    pub event_history: Vec<super::event_log::DashboardEvent>,
    pub risk_status: Option<serde_json::Value>,
    // PnL tracking
    pub daily_realized_pnl: f64,
    pub total_realized_pnl: f64,
    pub daily_funding_pnl: f64,
    pub total_funding_pnl: f64,
    pub initial_equity: f64,
    pub peak_equity: f64,
    pub equity_history: Vec<(i64, f64)>, // (unix_ts, equity) — for chart
    pub pnl_history: Vec<(i64, f64)>,    // (unix_ts, cumulative_pnl)
    /// Lifetime notional volume Σ|price × quantity| (survives ring-buffer trim).
    pub total_volume: f64,
    /// Lifetime close-event count (survives ring-buffer trim).
    pub total_closed_trades: u64,
    // Strategy config (can be modified from dashboard)
    pub strategy_params: std::collections::HashMap<String, String>,
    pub strategy_config_changed: bool,
    // Per-day PnL tracking (persisted)
    pub daily_pnl_map: std::collections::HashMap<String, f64>,
    // Trading controls (runtime)
    pub active_markets: Vec<u32>,   // Markets currently being traded
    pub trading_paused: bool,       // Pause all trading signals
    pub cancel_all_requested: bool, // Request to cancel all open orders
    pub available_markets: Vec<(u32, String)>, // All known markets: (id, symbol)
    // Risk config (runtime-editable from dashboard)
    pub risk_config: serde_json::Value, // Cached risk config for display
    /// Random per-process credential used by mutation-route middleware.
    pub dashboard_auth_token: String,
    pub risk_update_requested: Option<serde_json::Value>, // Pending risk update
    pub leverage_limit: f64, // Runtime leverage limit (used by main loop)
    /// symbol -> 最新盘口中间价。由主循环每个 tick 从 `snapshot.order_books` 注入。
    /// 独立于 positions：空仓时 positions 里没有任何价格，面板就没有行情可显示。
    pub last_prices: std::collections::HashMap<String, f64>,
    /// Aster maker shadow-mode metrics. No exchange orders are represented here.
    pub shadow_metrics: Option<serde_json::Value>,
    /// Multi-profile near-BBO HFT shadow comparison.
    pub hft_shadow_metrics: Option<serde_json::Value>,
    /// Server-side proposal ledger. The model cannot mutate live strategy state directly.
    pub quant_agent: super::quant_agent::AgentLedger,
    /// Hyperliquid userAddRate in bps when known.
    pub user_add_rate_bps: Option<f64>,
    /// Hyperliquid userCrossRate in bps when known.
    pub user_cross_rate_bps: Option<f64>,
    /// Last io vs xyz SNDK net bps that cleared the tradeable floor (not armed).
    pub last_cross_dex_net_bps: Option<f64>,
    pub last_cross_dex_side: Option<String>,
}

impl DashboardState {
    /// Save current PnL state to disk
    pub fn save_pnl(&self) {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut daily_map = self.daily_pnl_map.clone();
        daily_map.insert(today, self.daily_realized_pnl);

        let persistent = PersistentPnlData {
            total_realized_pnl: self.total_realized_pnl,
            total_funding_pnl: self.total_funding_pnl,
            daily_funding_pnl: self.daily_funding_pnl,
            initial_equity: self.initial_equity,
            peak_equity: self.peak_equity,
            equity_history: self.equity_history.clone(),
            pnl_history: self.pnl_history.clone(),
            trade_history: self.trade_history.clone(),
            daily_pnl_map: daily_map,
            total_volume: self.total_volume,
            total_closed_trades: self.total_closed_trades,
        };
        persistent.save(&self.network_name);
    }

    /// Restore PnL state from persistent data
    pub fn restore_pnl(&mut self, data: &PersistentPnlData) {
        self.total_realized_pnl = data.total_realized_pnl;
        self.total_funding_pnl = data.total_funding_pnl;
        self.daily_funding_pnl = data.daily_funding_pnl;
        // Only restore initial_equity if it was set (non-zero)
        if data.initial_equity > 0.0 {
            self.initial_equity = data.initial_equity;
        }
        if data.peak_equity > self.peak_equity {
            self.peak_equity = data.peak_equity;
        }
        // Merge equity history: keep persisted + add current
        if !data.equity_history.is_empty() {
            self.equity_history = data.equity_history.clone();
        }
        if !data.pnl_history.is_empty() {
            self.pnl_history = data.pnl_history.clone();
        }
        // Restore trade history
        if !data.trade_history.is_empty() {
            self.trade_history = data.trade_history.clone();
        }
        // Lifetime volume / close counts: prefer persisted values; if missing
        // (old state files), recompute from the retained buffer as a floor.
        let (buf_vol, buf_closes) = Self::stats_from_trades(&self.trade_history);
        self.total_volume = if data.total_volume > 0.0 {
            data.total_volume
        } else {
            buf_vol
        };
        self.total_closed_trades = if data.total_closed_trades > 0 {
            data.total_closed_trades
        } else {
            buf_closes
        };
        // Restore today's daily PnL
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        self.daily_realized_pnl = data.daily_pnl_map.get(&today).copied().unwrap_or(0.0);
        self.daily_pnl_map = data.daily_pnl_map.clone();
        info!(
            "📂 Restored PnL: total={:.4}, daily={:.4}, peak={:.2}, trades={}, volume={:.2}, closed={}",
            self.total_realized_pnl,
            self.daily_realized_pnl,
            self.peak_equity,
            self.trade_history.len(),
            self.total_volume,
            self.total_closed_trades
        );
    }

    /// Notional volume and close-event count from a trade list.
    pub fn stats_from_trades(trades: &[serde_json::Value]) -> (f64, u64) {
        let mut volume = 0.0_f64;
        let mut closes = 0_u64;
        for t in trades {
            let price = t.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let qty = t.get("quantity").and_then(|v| v.as_f64()).unwrap_or(0.0);
            volume += (price * qty).abs();
            let action = t
                .get("action")
                .or_else(|| t.get("close_type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if action_is_close(action) {
                closes += 1;
            }
        }
        (volume, closes)
    }

    /// Append a fill to the ring buffer and update lifetime volume/close counters.
    pub fn push_trade(&mut self, trade: serde_json::Value) {
        let price = trade.get("price").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let qty = trade
            .get("quantity")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        self.total_volume += (price * qty).abs();
        let action = trade
            .get("action")
            .or_else(|| trade.get("close_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if action_is_close(action) {
            self.total_closed_trades += 1;
        }
        self.trade_history.push(trade);
        let len = self.trade_history.len();
        if len > TRADE_HISTORY_LIMIT {
            self.trade_history.drain(..len - TRADE_HISTORY_LIMIT);
        }
    }
}

fn action_is_close(action: &str) -> bool {
    let lower = action.to_ascii_lowercase();
    lower.contains("close")
        || lower.contains("stop")
        || lower.contains("emergency")
        || lower.contains("liquidat")
}

pub type SharedDashboardState = Arc<RwLock<DashboardState>>;

/// Start Dashboard Web server
pub async fn start(host: &str, port: u16) -> Result<()> {
    let state: SharedDashboardState = Arc::new(RwLock::new(DashboardState {
        network_name: crate::env_profiles::selected_network(),
        ..DashboardState::default()
    }));
    start_with_state(host, port, state).await
}

pub async fn start_with_state(host: &str, port: u16, state: SharedDashboardState) -> Result<()> {
    let configured_token = std::env::var("DASHBOARD_AUTH_TOKEN").ok();
    if configured_token
        .as_deref()
        .is_some_and(|token| token.len() < 32)
    {
        bail!("DASHBOARD_AUTH_TOKEN must contain at least 32 characters");
    }
    let auth_token = configured_token.unwrap_or_else(|| {
        rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(48)
            .map(char::from)
            .collect()
    });
    state.write().await.dashboard_auth_token = auth_token;
    super::event_log::restore_event_history(&state).await;
    super::event_log::spawn_event_monitor(state.clone());

    let protected = Router::new()
        .route("/api/env", post(env_update_handler))
        .route("/api/network", post(network_update_handler))
        .route("/api/strategy", post(strategy_update_handler))
        .route("/api/backtest", post(backtest_handler))
        .route("/api/backtest/optimize", post(backtest_optimize_handler))
        .route("/api/agent/proposals", post(agent_proposal_handler))
        .route("/api/agent/proposals/:id/apply", post(agent_apply_handler))
        .route(
            "/api/backtest/opencode-optimize",
            post(opencode_optimize_handler),
        )
        .route("/api/trading/markets", post(markets_update_handler))
        .route("/api/trading/pause", post(trading_pause_handler))
        .route("/api/trading/resume", post(trading_resume_handler))
        .route("/api/trading/cancel-all", post(cancel_all_handler))
        .route("/api/risk/config", post(risk_config_update_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_mutation_auth,
        ));

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/app.js", get(js_handler))
        .route("/ai", get(ai_page_handler))
        .route("/health", get(health_handler))
        .route("/ws", get(ws_handler))
        .route("/api/status", get(status_handler))
        .route("/api/positions", get(positions_handler))
        .route("/api/trades", get(trades_handler))
        .route("/api/shadow", get(shadow_handler))
        .route("/api/hft-shadow", get(hft_shadow_handler))
        .route("/api/events", get(events_handler))
        .route("/api/env", get(env_get_handler))
        .route("/api/network", get(network_get_handler))
        .route("/api/pnl", get(pnl_handler))
        .route("/api/strategy", get(strategy_get_handler))
        .route("/api/backtest/datasets", get(backtest_datasets_handler))
        .route("/api/agent/status", get(agent_status_handler))
        .route("/api/agent/audit", get(agent_audit_handler))
        .route("/api/trading/markets", get(markets_get_handler))
        .route("/api/risk/config", get(risk_config_get_handler))
        .merge(protected)
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    info!("Dashboard running at: http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

fn request_is_authorized(headers: &HeaderMap, token: &str) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if bearer.is_some_and(|candidate| constant_time_eq(candidate, token)) {
        return true;
    }
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == "quant_bot_auth").then_some(value)
            })
        })
        .is_some_and(|candidate| constant_time_eq(candidate, token))
}

async fn require_mutation_auth(
    State(state): State<SharedDashboardState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let token = state.read().await.dashboard_auth_token.clone();
    if token.is_empty() || !request_is_authorized(request.headers(), &token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

async fn authenticated_html(state: SharedDashboardState, html: &'static str) -> impl IntoResponse {
    let token = state.read().await.dashboard_auth_token.clone();
    (
        [(
            header::SET_COOKIE,
            format!("quant_bot_auth={token}; HttpOnly; SameSite=Strict; Path=/"),
        )],
        Html(html),
    )
}
async fn index_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    authenticated_html(state, include_str!("ui/index.html")).await
}

async fn js_handler() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        include_str!("ui/app.js"),
    )
}

async fn health_handler() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "status": "ok",
        "uptime": "running"
    }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<SharedDashboardState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_connection(socket, state))
}

async fn handle_ws_connection(mut socket: WebSocket, state: SharedDashboardState) {
    info!("New dashboard WebSocket connection");

    let welcome = serde_json::json!({
        "type": "welcome",
        "message": "Connected to Multi-Venue Quant Bot Dashboard"
    });
    let _ = socket.send(Message::Text(welcome.to_string())).await;

    // Auto-push state every 3 seconds alongside handling client requests
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let state_push = state.clone();
    let push_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
        loop {
            interval.tick().await;
            let ds = state_push.read().await;
            let status_msg = serde_json::json!({
                "type": "status",
                "data": {
                    "running": true,
                    "strategy": ds.strategy_name,
                    "total_trades": ds.total_trades,
                    "equity": ds.equity,
                    "unrealized_pnl": ds.unrealized_pnl,
                    "available_balance": ds.available_balance,
                    "open_orders": ds.open_orders,
                    "daily_realized_pnl": ds.daily_realized_pnl,
                    "total_realized_pnl": ds.total_realized_pnl,
                    "total_volume": ds.total_volume,
                    "total_closed_trades": ds.total_closed_trades,
                    "initial_equity": ds.initial_equity,
                    "peak_equity": ds.peak_equity,
                    "total_return_pct": if ds.initial_equity > 0.0 {
                        (ds.equity - ds.initial_equity) / ds.initial_equity * 100.0
                    } else { 0.0 },
                    "trading_paused": ds.trading_paused,
                    "active_markets": ds.active_markets,
                    "last_prices": ds.last_prices,
                    "network": ds.network_name,
                    "user_add_rate_bps": ds.user_add_rate_bps,
                    "user_cross_rate_bps": ds.user_cross_rate_bps,
                    "fee_tier_is_t4": ds.user_add_rate_bps.map(|bps| bps <= 0.0),
                    "last_cross_dex_net_bps": ds.last_cross_dex_net_bps,
                    "last_cross_dex_side": ds.last_cross_dex_side.clone(),
                    "strategy_overlay": PersistentStrategyConfig::exists(&ds.network_name),
                    "quote_mode": ds.strategy_params.get("quote_mode"),
                    "flatten_only": ds.strategy_params.get("flatten_only"),
                }
            });
            let positions_msg = serde_json::json!({
                "type": "positions",
                "data": ds.positions
            });
            let risk_msg = {
                let risk = ds.risk_status.clone().unwrap_or(serde_json::json!({
                    "drawdown_pct": 0.0,
                    "daily_loss_pct": 0.0,
                    "max_drawdown_limit": 15.0,
                    "daily_loss_limit": 8.0,
                    "is_healthy": true
                }));
                serde_json::json!({ "type": "risk", "data": risk })
            };
            let trades_msg = serde_json::json!({
                "type": "recent_trades",
                "data": ds.trade_history.iter().rev().take(20).collect::<Vec<_>>()
            });
            let events_msg = serde_json::json!({
                "type": "events",
                "data": ds.event_history.iter().rev().collect::<Vec<_>>()
            });
            let orders_msg = serde_json::json!({
                "type": "open_orders",
                "data": ds.open_orders_list
            });
            drop(ds);

            if ws_sender
                .send(Message::Text(status_msg.to_string()))
                .await
                .is_err()
            {
                break;
            }
            let _ = ws_sender
                .send(Message::Text(positions_msg.to_string()))
                .await;
            let _ = ws_sender.send(Message::Text(risk_msg.to_string())).await;
            let _ = ws_sender.send(Message::Text(trades_msg.to_string())).await;
            let _ = ws_sender.send(Message::Text(events_msg.to_string())).await;
            let _ = ws_sender.send(Message::Text(orders_msg.to_string())).await;
        }
    });

    // Handle incoming client requests (orderbook queries, etc.)
    while let Some(msg) = ws_receiver.next().await {
        match msg {
            Ok(axum::extract::ws::Message::Text(_text)) => {
                // Client-initiated requests are handled by the auto-push above.
                // Only orderbook needs client request (to specify market_id).
                // The push task handles status/positions/risk automatically.
            }
            Ok(axum::extract::ws::Message::Close(_)) => {
                info!("Dashboard WebSocket closed");
                break;
            }
            Err(e) => {
                tracing::error!("Dashboard WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }

    push_handle.abort();
}

async fn status_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    let ds = state.read().await;
    axum::Json(serde_json::json!({
        "status": "running",
        "version": env!("CARGO_PKG_VERSION"),
        "strategy": ds.strategy_name,
        "trading_paused": ds.trading_paused,
        "open_orders": ds.open_orders,
        "total_trades": ds.total_trades,
        "equity": ds.equity,
        "total_pnl": ds.total_realized_pnl + ds.unrealized_pnl,
        "daily_realized_pnl": ds.daily_realized_pnl,
        "total_realized_pnl": ds.total_realized_pnl,
        "daily_funding_pnl": ds.daily_funding_pnl,
        "total_funding_pnl": ds.total_funding_pnl,
        "last_prices": ds.last_prices,
        "network": ds.network_name,
        "rest_url": ds.rest_url,
        "ws_url": ds.ws_url,
        "chain_id": ds.chain_id,
        "shadow": ds.shadow_metrics,
        "hft_shadow": ds.hft_shadow_metrics,
        "user_add_rate_bps": ds.user_add_rate_bps,
        "user_cross_rate_bps": ds.user_cross_rate_bps,
        "fee_tier_is_t4": ds.user_add_rate_bps.map(|bps| bps <= 0.0),
        "last_cross_dex_net_bps": ds.last_cross_dex_net_bps,
        "last_cross_dex_side": ds.last_cross_dex_side,
        "strategy_overlay": PersistentStrategyConfig::exists(&ds.network_name),
        "quote_mode": ds.strategy_params.get("quote_mode"),
        "flatten_only": ds.strategy_params.get("flatten_only"),
    }))
}

async fn positions_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    let ds = state.read().await;
    axum::Json(serde_json::json!({
        "positions": ds.positions
    }))
}

async fn trades_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    let ds = state.read().await;
    axum::Json(serde_json::json!({
        "trades": ds.trade_history
    }))
}

async fn shadow_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    let ds = state.read().await;
    axum::Json(
        ds.shadow_metrics
            .clone()
            .unwrap_or_else(|| serde_json::json!({"enabled": false})),
    )
}

async fn hft_shadow_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    let ds = state.read().await;
    axum::Json(
        ds.hft_shadow_metrics
            .clone()
            .unwrap_or_else(|| serde_json::json!({"enabled": false})),
    )
}

const ENV_SHARED_PUBLIC_KEYS: [&str; 2] = ["RUST_LOG", "TOKIO_WORKER_THREADS"];

#[derive(Clone, Copy)]
struct CredentialField {
    api_key: &'static str,
    suffix: &'static str,
    secret: bool,
}

const LIGHTER_CREDENTIAL_FIELDS: [CredentialField; 3] = [
    CredentialField {
        api_key: "LIGHTER_ACCOUNT_INDEX",
        suffix: "ACCOUNT_INDEX",
        secret: false,
    },
    CredentialField {
        api_key: "LIGHTER_API_KEY_INDEX",
        suffix: "API_KEY_INDEX",
        secret: false,
    },
    CredentialField {
        api_key: "LIGHTER_SECRET_KEY",
        suffix: "SECRET_KEY",
        secret: true,
    },
];
const ARCUS_CREDENTIAL_FIELDS: [CredentialField; 4] = [
    CredentialField {
        api_key: "ARCUS_API_KEY",
        suffix: "API_KEY",
        secret: false,
    },
    CredentialField {
        api_key: "ARCUS_ADDRESS",
        suffix: "ADDRESS",
        secret: false,
    },
    CredentialField {
        api_key: "ARCUS_ACCOUNT_INDEX",
        suffix: "ACCOUNT_INDEX",
        secret: false,
    },
    CredentialField {
        api_key: "ARCUS_SIGNING_KEY",
        suffix: "SIGNING_KEY",
        secret: true,
    },
];
const ASTER_CREDENTIAL_FIELDS: [CredentialField; 2] = [
    CredentialField {
        api_key: "ASTER_SIGNER_ADDRESS",
        suffix: "SIGNER_ADDRESS",
        secret: false,
    },
    CredentialField {
        api_key: "ASTER_SIGNER_PRIVATE_KEY",
        suffix: "SIGNER_PRIVATE_KEY",
        secret: true,
    },
];

static HYPERLIQUID_CREDENTIAL_FIELDS: [CredentialField; 2] = [
    CredentialField {
        api_key: "HYPERLIQUID_ACCOUNT_ADDRESS",
        suffix: "ACCOUNT_ADDRESS",
        secret: false,
    },
    CredentialField {
        api_key: "HYPERLIQUID_SIGNER_PRIVATE_KEY",
        suffix: "SIGNER_PRIVATE_KEY",
        secret: true,
    },
];

fn credential_fields(
    venue: multi_venue_quant_bot::exchange::LiveVenue,
) -> &'static [CredentialField] {
    use multi_venue_quant_bot::exchange::ExchangeKind;
    match venue.exchange() {
        ExchangeKind::Lighter => &LIGHTER_CREDENTIAL_FIELDS,
        ExchangeKind::Arcus => &ARCUS_CREDENTIAL_FIELDS,
        ExchangeKind::Aster => &ASTER_CREDENTIAL_FIELDS,
        ExchangeKind::Hyperliquid => &HYPERLIQUID_CREDENTIAL_FIELDS,
    }
}

fn masked_secret(value: &str) -> serde_json::Value {
    serde_json::json!({
        "configured": !value.is_empty(),
    })
}

fn shared_env_file_path() -> std::path::PathBuf {
    std::path::PathBuf::from(".env")
}

fn selected_credential_env_path() -> std::path::PathBuf {
    std::path::PathBuf::from(".env")
}

fn venue_label(venue: multi_venue_quant_bot::exchange::LiveVenue) -> &'static str {
    use multi_venue_quant_bot::exchange::LiveVenue;
    match venue {
        LiveVenue::LighterMainnet => "Lighter Mainnet",
        LiveVenue::LighterRobinhood => "Robinhood Chain",
        LiveVenue::ArcusMainnet => "Arcus Mainnet",
        LiveVenue::ArcusTestnet => "Arcus Testnet",
        LiveVenue::AsterMainnet => "Aster Mainnet",
        LiveVenue::HyperliquidMainnet => "Hyperliquid Mainnet",
        LiveVenue::HyperliquidTestnet => "Hyperliquid Testnet",
    }
}

fn venue_quote_asset(venue: multi_venue_quant_bot::exchange::LiveVenue) -> &'static str {
    use multi_venue_quant_bot::exchange::LiveVenue;
    match venue {
        LiveVenue::LighterMainnet => "USDC",
        LiveVenue::LighterRobinhood => "USDG",
        LiveVenue::ArcusMainnet | LiveVenue::ArcusTestnet => "USD",
        LiveVenue::AsterMainnet => "USDT",
        LiveVenue::HyperliquidMainnet | LiveVenue::HyperliquidTestnet => "USDC",
    }
}

async fn network_get_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    use multi_venue_quant_bot::exchange::LiveVenue;
    let ds = state.read().await;
    let selected = crate::env_profiles::selected_network();
    let profiles = LiveVenue::ALL
        .into_iter()
        .map(|venue| {
            (
                venue.as_str().to_string(),
                serde_json::json!({
                    "label": venue_label(venue),
                    "quote_asset": venue_quote_asset(venue),
                    "rest_url": venue.rest_url(),
                    "ws_url": venue.websocket_url(),
                    "chain_id": venue.chain_id(),
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    axum::Json(serde_json::json!({
        "active": ds.network_name,
        "selected": selected,
        "rest_url": ds.rest_url,
        "ws_url": ds.ws_url,
        "chain_id": ds.chain_id,
        "requires_restart": true,
        "profiles": profiles
    }))
}

async fn network_update_handler(
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let network = body.get("network").and_then(|v| v.as_str()).unwrap_or("");
    let Ok(venue) = network.parse::<multi_venue_quant_bot::exchange::LiveVenue>() else {
        return axum::Json(serde_json::json!({
            "status":"error", "message":"unsupported live venue"
        }));
    };
    let normalized = venue.as_str();
    let updates =
        std::collections::HashMap::from([("TRADING_VENUE".to_string(), normalized.to_string())]);
    match write_env_keys_to(&shared_env_file_path(), &updates) {
        Ok(()) => axum::Json(serde_json::json!({
            "status":"ok", "network":normalized, "requires_restart":true
        })),
        Err(e) => axum::Json(serde_json::json!({"status":"error","message":e.to_string()})),
    }
}

/// 保留注释与未知键，只替换已知键的值；文件不存在时新建。
fn write_env_keys_to(
    path: &std::path::Path,
    updates: &std::collections::HashMap<String, String>,
) -> std::io::Result<()> {
    let original = std::fs::read_to_string(path).unwrap_or_default();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = String::new();

    for line in original.lines() {
        let trimmed = line.trim_start();
        // 注释和空行原样保留
        if trimmed.starts_with('#') || trimmed.is_empty() {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        match trimmed.split_once('=') {
            Some((k, _)) if updates.contains_key(k.trim()) => {
                let key = k.trim();
                out.push_str(&format!("{}={}\n", key, updates[key]));
                seen.insert(updates.get_key_value(key).unwrap().0.as_str());
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    // 文件里原本没有的键追加到末尾
    let missing: Vec<_> = updates
        .keys()
        .filter(|k| !seen.contains(k.as_str()))
        .collect();
    if !missing.is_empty() {
        out.push_str("\n# —— 由面板写入 ——\n");
        for k in missing {
            out.push_str(&format!("{}={}\n", k, updates[k]));
        }
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(out.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, out)
    }
}

#[derive(Default, Deserialize)]
struct EnvQuery {
    venue: Option<String>,
}

fn parse_credential_venue(
    requested: Option<&str>,
) -> std::result::Result<multi_venue_quant_bot::exchange::LiveVenue, String> {
    requested.map_or_else(
        || Ok(crate::env_profiles::selected_venue()),
        |value| {
            value
                .parse::<multi_venue_quant_bot::exchange::LiveVenue>()
                .map_err(|error| error.to_string())
        },
    )
}

async fn env_get_handler(Query(query): Query<EnvQuery>) -> impl IntoResponse {
    let Ok(venue) = parse_credential_venue(query.venue.as_deref()) else {
        return axum::Json(serde_json::json!({
            "status": "error",
            "message": "unsupported live venue"
        }));
    };
    let credential_path = selected_credential_env_path();
    let shared_path = shared_env_file_path();
    let mut public = serde_json::Map::new();
    let mut secrets = serde_json::Map::new();
    for field in credential_fields(venue) {
        let stored_key = venue.credential_key(field.suffix);
        let value =
            crate::env_profiles::read_env_value(&credential_path, &stored_key).unwrap_or_default();
        if field.secret {
            secrets.insert(field.api_key.to_string(), masked_secret(&value));
        } else {
            public.insert(field.api_key.to_string(), serde_json::json!(value));
        }
    }
    for key in ENV_SHARED_PUBLIC_KEYS {
        public.insert(
            key.to_string(),
            serde_json::json!(
                crate::env_profiles::read_env_value(&shared_path, key).unwrap_or_default()
            ),
        );
    }
    axum::Json(serde_json::json!({
        "venue": venue.as_str(),
        "public": public,
        "secrets": secrets,
        "env_path": credential_path.to_string_lossy(),
        // 环境变量在进程启动时读入，改完必须重启才会生效
        "requires_restart": true,
    }))
}

async fn env_update_handler(axum::Json(body): axum::Json<serde_json::Value>) -> impl IntoResponse {
    let Some(obj) = body.as_object() else {
        return axum::Json(
            serde_json::json!({"status":"error","message":"body must be an object"}),
        );
    };
    let requested_venue = obj.get("venue").and_then(|value| value.as_str());
    let Ok(venue) = parse_credential_venue(requested_venue) else {
        return axum::Json(serde_json::json!({
            "status": "error",
            "message": "unsupported live venue"
        }));
    };
    let fields = credential_fields(venue);
    let mut updates = std::collections::HashMap::new();
    let mut rejected = Vec::new();
    for (k, v) in obj {
        if k == "venue" {
            continue;
        }
        let allowed = fields.iter().any(|field| field.api_key == k)
            || ENV_SHARED_PUBLIC_KEYS.contains(&k.as_str());
        if !allowed {
            rejected.push(k.clone());
            continue;
        }
        let val = v.as_str().unwrap_or_default().trim().to_string();
        // 空字符串视为"不修改"，避免前端把掩码占位符写回去把密钥清空
        if val.is_empty() {
            continue;
        }
        updates.insert(k.clone(), val);
    }
    if updates.is_empty() {
        return axum::Json(serde_json::json!({
            "status":"error","message":"no writable keys in request","rejected":rejected}));
    }
    let credential_updates: std::collections::HashMap<_, _> = updates
        .iter()
        .filter_map(|(key, value)| {
            fields
                .iter()
                .find(|field| field.api_key == key)
                .map(|field| (venue.credential_key(field.suffix), value.clone()))
        })
        .collect();
    let shared_updates: std::collections::HashMap<_, _> = updates
        .iter()
        .filter(|(key, _)| ENV_SHARED_PUBLIC_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let credential_path = selected_credential_env_path();
    let write_result = if !credential_updates.is_empty() {
        write_env_keys_to(&credential_path, &credential_updates)
    } else {
        Ok(())
    }
    .and_then(|()| {
        if shared_updates.is_empty() {
            Ok(())
        } else {
            write_env_keys_to(&shared_env_file_path(), &shared_updates)
        }
    });
    match write_result {
        Ok(()) => {
            let keys: Vec<_> = updates.keys().cloned().collect();
            info!("环境变量已写入网络隔离配置: {:?}（重启后生效）", keys);
            axum::Json(serde_json::json!({
                "status":"ok",
                "venue": venue.as_str(),
                "updated": keys,
                "rejected": rejected,
                "requires_restart": true,
                "env_path": credential_path.to_string_lossy(),
                "message":"Saved to the selected network profile. Restart the bot for changes to take effect."
            }))
        }
        Err(e) => axum::Json(serde_json::json!({"status":"error","message":e.to_string()})),
    }
}

async fn events_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    let ds = state.read().await;
    axum::Json(serde_json::json!({
        "events": ds.event_history.iter().rev().collect::<Vec<_>>()
    }))
}

async fn pnl_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    let ds = state.read().await;
    // Prefer lifetime counters; fall back to the retained buffer so old
    // processes without these fields still return something sensible.
    let (buf_vol, buf_closes) = DashboardState::stats_from_trades(&ds.trade_history);
    let total_volume = if ds.total_volume > 0.0 {
        ds.total_volume
    } else {
        buf_vol
    };
    let total_closed_trades = if ds.total_closed_trades > 0 {
        ds.total_closed_trades
    } else {
        buf_closes
    };
    axum::Json(serde_json::json!({
        "daily_realized_pnl": ds.daily_realized_pnl,
        "total_realized_pnl": ds.total_realized_pnl,
        "daily_funding_pnl": ds.daily_funding_pnl,
        "total_funding_pnl": ds.total_funding_pnl,
        "unrealized_pnl": ds.unrealized_pnl,
        "equity": ds.equity,
        "initial_equity": ds.initial_equity,
        "peak_equity": ds.peak_equity,
        "total_volume": total_volume,
        "total_closed_trades": total_closed_trades,
        "total_trades": ds.total_trades,
        "trade_history_limit": TRADE_HISTORY_LIMIT,
        "trade_history_len": ds.trade_history.len(),
        "total_return_pct": if ds.initial_equity > 0.0 {
            (ds.equity - ds.initial_equity) / ds.initial_equity * 100.0
        } else { 0.0 },
        "equity_history": ds.equity_history.iter()
            .map(|(ts, eq)| serde_json::json!({"t": ts, "v": eq}))
            .collect::<Vec<_>>(),
        "pnl_history": ds.pnl_history.iter()
            .map(|(ts, pnl)| serde_json::json!({"t": ts, "v": pnl}))
            .collect::<Vec<_>>(),
        "daily_pnl_map": ds.daily_pnl_map,
        // Newest first; full retained buffer (limit is TRADE_HISTORY_LIMIT).
        "trades": ds.trade_history.iter().rev().collect::<Vec<_>>(),
    }))
}

async fn strategy_get_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    let ds = state.read().await;
    axum::Json(serde_json::json!({
        "strategy": ds.strategy_name,
        "params": ds.strategy_params,
    }))
}

fn agent_policy(ds: &DashboardState) -> super::quant_agent::PolicySnapshot {
    let emergency_triggered = ds
        .risk_status
        .as_ref()
        .and_then(|risk| risk.get("emergency_triggered"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let max_drawdown_pct = ds
        .risk_config
        .get("max_drawdown_pct")
        .and_then(|value| value.as_f64())
        .unwrap_or(10.0)
        .clamp(0.1, 25.0);
    super::quant_agent::PolicySnapshot {
        equity: ds.equity,
        trading_paused: ds.trading_paused,
        emergency_triggered,
        max_drawdown_pct,
        // This is deliberately a backend constant, not a model-supplied parameter.
        max_notional_pct: 25.0,
    }
}

async fn agent_status_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    let ds = state.read().await;
    let policy = agent_policy(&ds);
    let pending = ds
        .quant_agent
        .proposals
        .iter()
        .filter(|proposal| proposal.status == "pending")
        .count();
    axum::Json(serde_json::json!({
        "status": if policy.emergency_triggered || !policy.equity.is_finite() || policy.equity <= 0.0 { "blocked" } else { "ready" },
        "mode": "proposal_only",
        "model_authority": "research_and_propose",
        "execution_authority": "human_approval_required",
        "policy": policy,
        "pending_proposals": pending,
    }))
}

async fn agent_audit_handler(State(state): State<SharedDashboardState>) -> impl IntoResponse {
    let ds = state.read().await;
    let records = ds
        .quant_agent
        .proposals
        .iter()
        .rev()
        .take(50)
        .collect::<Vec<_>>();
    axum::Json(serde_json::json!({"status":"ok", "records": records}))
}

async fn agent_proposal_handler(
    State(state): State<SharedDashboardState>,
    axum::Json(mut input): axum::Json<super::quant_agent::ProposalInput>,
) -> impl IntoResponse {
    // Evidence supplied by a model/browser is untrusted. Re-run the exact candidate on
    // the server and replace every metric before the deterministic policy sees it.
    let mut param_pairs = input
        .params
        .iter()
        .map(|(key, value)| {
            let value = value
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| value.to_string());
            format!("{key}={value}")
        })
        .collect::<Vec<_>>();
    param_pairs.sort();
    let params = param_pairs.join(",");
    let verified = match run_validated_backtest_result(
        &input.strategy,
        &params,
        &input.evidence.data_file,
        input.evidence.capital,
        &input.evidence.start,
        &input.evidence.end,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return axum::Json(serde_json::json!({
                "status":"error", "message":format!("server verification backtest failed: {error}")
            }))
        }
    };
    input.evidence.total_return_pct = verified["total_return_pct"].as_f64().unwrap_or(f64::NAN);
    input.evidence.sharpe_ratio = verified["sharpe_ratio"].as_f64().unwrap_or(f64::NAN);
    input.evidence.max_drawdown_pct = verified["max_drawdown_pct"].as_f64().unwrap_or(f64::NAN);
    input.evidence.total_trades = verified["total_trades"].as_u64().unwrap_or(0);
    input.evidence.peak_notional_pct = verified["peak_notional_pct"].as_f64().unwrap_or(f64::NAN);
    input.evidence.validation_return_pct = verified["validation_return_pct"]
        .as_f64()
        .unwrap_or(f64::NAN);
    input.evidence.validation_sharpe_ratio = verified["validation_sharpe_ratio"]
        .as_f64()
        .unwrap_or(f64::NAN);
    input.evidence.validation_max_drawdown_pct = verified["validation_max_drawdown_pct"]
        .as_f64()
        .unwrap_or(f64::NAN);
    input.evidence.validation_total_trades =
        verified["validation_total_trades"].as_u64().unwrap_or(0);
    input.evidence.validation_peak_notional_pct = verified["validation_peak_notional_pct"]
        .as_f64()
        .unwrap_or(f64::NAN);
    input.evidence.rolling_days = verified["rolling_days"].as_u64().unwrap_or(0);
    input.evidence.rolling_profitable_days =
        verified["rolling_profitable_days"].as_u64().unwrap_or(0);
    input.evidence.cash_open_return_pct = verified["cash_open_return_pct"]
        .as_f64()
        .unwrap_or(f64::NAN);
    input.evidence.cash_open_trades = verified["cash_open_trades"].as_u64().unwrap_or(0);
    input.evidence.validation_market_count =
        verified["validation_market_count"].as_u64().unwrap_or(0);
    input.evidence.validation_profitable_market_count = verified
        ["validation_profitable_market_count"]
        .as_u64()
        .unwrap_or(0);

    let mut ds = state.write().await;
    let policy = agent_policy(&ds);
    let decision = super::quant_agent::evaluate_proposal(&input, &policy);
    let id: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(18)
        .map(char::from)
        .collect();
    let approval_phrase = format!("APPLY {id}");
    let proposal = super::quant_agent::AgentProposal {
        id,
        created_at: chrono::Utc::now().to_rfc3339(),
        status: if decision.allowed {
            "pending"
        } else {
            "rejected"
        }
        .to_string(),
        input,
        policy,
        decision,
        approval_phrase,
    };
    ds.quant_agent.record(proposal.clone());
    if let Err(error) = ds.quant_agent.save(&ds.network_name) {
        warn!("Failed to persist Quant Agent audit: {}", error);
    }
    axum::Json(serde_json::json!({"status":"ok", "proposal": proposal}))
}

#[derive(Deserialize)]
struct AgentApproval {
    approval_phrase: String,
}

async fn agent_apply_handler(
    State(state): State<SharedDashboardState>,
    Path(id): Path<String>,
    axum::Json(approval): axum::Json<AgentApproval>,
) -> impl IntoResponse {
    let mut ds = state.write().await;
    let Some(index) = ds
        .quant_agent
        .proposals
        .iter()
        .position(|proposal| proposal.id == id)
    else {
        return axum::Json(serde_json::json!({"status":"error","message":"proposal not found"}));
    };

    let proposal = ds.quant_agent.proposals[index].clone();
    if proposal.status != "pending" || approval.approval_phrase != proposal.approval_phrase {
        return axum::Json(serde_json::json!({
            "status":"error",
            "message":"proposal is not pending or approval phrase does not match"
        }));
    }
    // Re-evaluate against current runtime state so stale approvals cannot bypass a new pause/emergency.
    let current_policy = agent_policy(&ds);
    let current_decision = super::quant_agent::evaluate_proposal(&proposal.input, &current_policy);
    if !current_decision.allowed {
        ds.quant_agent.proposals[index].status = "blocked_at_apply".to_string();
        ds.quant_agent.proposals[index].decision = current_decision.clone();
        let _ = ds.quant_agent.save(&ds.network_name);
        return axum::Json(serde_json::json!({
            "status":"error", "message":"current policy blocked apply", "decision":current_decision
        }));
    }

    ds.strategy_name = match proposal.input.strategy.as_str() {
        "grid" => "grid_trading",
        "trend" => "trend_following",
        other => other,
    }
    .to_string();
    ds.strategy_params = proposal
        .input
        .params
        .iter()
        .map(|(key, value)| {
            let normalized = value
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| value.to_string());
            (key.clone(), normalized)
        })
        .collect();
    ds.strategy_config_changed = true;
    ds.quant_agent.proposals[index].status = "applied".to_string();
    PersistentStrategyConfig::save(&ds);
    if let Err(error) = ds.quant_agent.save(&ds.network_name) {
        warn!("Failed to persist Quant Agent apply audit: {}", error);
    }
    info!(
        "Quant Agent proposal {} applied after explicit approval",
        id
    );
    axum::Json(serde_json::json!({
        "status":"ok", "message":"approved proposal applied", "proposal_id":id,
        "strategy":ds.strategy_name, "params":ds.strategy_params
    }))
}

async fn strategy_update_handler(
    State(state): State<SharedDashboardState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let mut ds = state.write().await;
    // Allow switching strategy name
    if let Some(name) = body.get("strategy").and_then(|s| s.as_str()) {
        ds.strategy_name = name.to_string();
        ds.strategy_config_changed = true;
        info!("Strategy switched to: {}", name);
    }
    if let Some(params) = body.get("params").and_then(|p| p.as_object()) {
        for (k, v) in params {
            ds.strategy_params.insert(
                k.clone(),
                v.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| v.as_bool().map(|b| b.to_string()))
                    .or_else(|| v.as_f64().map(|n| n.to_string()))
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
                    .unwrap_or_default(),
            );
        }
        ds.strategy_config_changed = true;
        info!(
            "Strategy params updated from dashboard: {:?}",
            ds.strategy_params
        );
    }
    PersistentStrategyConfig::save(&ds);
    axum::Json(serde_json::json!({
        "status": "ok",
        "message": "Strategy config updated. Changes will apply shortly.",
        "strategy": ds.strategy_name,
        "params": ds.strategy_params,
    }))
}

/// List CSVs under `backtests/data/` with their actual first/last candle dates
/// so the AI Lab date pickers stay aligned with real data (not hardcoded months).
async fn backtest_datasets_handler() -> impl IntoResponse {
    let dir = std::path::Path::new("backtests/data");
    let mut items: Vec<serde_json::Value> = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            return axum::Json(serde_json::json!({
                "status": "error",
                "message": format!("Cannot read backtests/data: {}", e),
                "datasets": []
            }));
        }
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("csv") {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let path_str = path.to_string_lossy().to_string();
        match crate::data::loader::load_csv_data(&path_str) {
            Ok(candles) if !candles.is_empty() => {
                let start = candles.first().unwrap().timestamp.date_naive().to_string();
                let end = candles.last().unwrap().timestamp.date_naive().to_string();
                items.push(serde_json::json!({
                    "file": name,
                    "start": start,
                    "end": end,
                    "candles": candles.len(),
                    "label": format!("{} ({} → {}, {} bars)", name, start, end, candles.len()),
                }));
            }
            Ok(_) => {
                items.push(serde_json::json!({
                    "file": name,
                    "start": serde_json::Value::Null,
                    "end": serde_json::Value::Null,
                    "candles": 0,
                    "label": format!("{} (empty)", name),
                }));
            }
            Err(e) => {
                warn!("Skip dataset {}: {}", name, e);
            }
        }
    }
    // Newest end date first, then longest series — preferred default is recent mainnet.
    items.sort_by(|a, b| {
        let ae = a.get("end").and_then(|v| v.as_str()).unwrap_or("");
        let be = b.get("end").and_then(|v| v.as_str()).unwrap_or("");
        be.cmp(ae).then_with(|| {
            let ac = a.get("candles").and_then(|v| v.as_u64()).unwrap_or(0);
            let bc = b.get("candles").and_then(|v| v.as_u64()).unwrap_or(0);
            bc.cmp(&ac)
        })
    });
    let default_file = items
        .first()
        .and_then(|d| d.get("file"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    axum::Json(serde_json::json!({
        "status": "ok",
        "datasets": items,
        "default": default_file,
        "today": chrono::Utc::now().date_naive().to_string(),
    }))
}

async fn backtest_handler(axum::Json(body): axum::Json<serde_json::Value>) -> impl IntoResponse {
    let strategy = body
        .get("strategy")
        .and_then(|s| s.as_str())
        .unwrap_or("grid");
    let params = body.get("params").and_then(|s| s.as_str()).unwrap_or("");
    let data_file = body.get("data_file").and_then(|s| s.as_str()).unwrap_or("");
    let capital = body
        .get("capital")
        .and_then(|c| c.as_f64())
        .unwrap_or(125.0);
    // Accept start/end and the legacy start_date/end_date aliases used by older AI Lab JS.
    let start = body
        .get("start")
        .or_else(|| body.get("start_date"))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let end = body
        .get("end")
        .or_else(|| body.get("end_date"))
        .and_then(|s| s.as_str())
        .unwrap_or("");

    match run_validated_backtest_result(strategy, params, data_file, capital, start, end).await {
        Ok(result) => axum::Json(result),
        Err(e) => axum::Json(serde_json::json!({"status": "error", "message": e.to_string()})),
    }
}

/// Trend defaults to notional=$1000 in create_strategy_with_params, but the AI Lab
/// often runs with $125–$200 capital. The engine then silently skips every open
/// (cost > capital) while the strategy still records a phantom position → 0 trades.
/// Clamp / inject an affordable notional for backtests only.
fn normalize_backtest_params(strategy: &str, params: &str, capital: f64) -> String {
    let is_trend = matches!(strategy, "trend" | "trend_following");
    let is_dca = strategy == "dca";
    if !is_trend && !is_dca {
        return params.to_string();
    }

    let mut map: std::collections::BTreeMap<String, String> = params
        .split(',')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let k = parts.next()?.trim();
            let v = parts.next()?.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.to_string()))
        })
        .collect();

    if is_trend {
        let cap = capital.max(1.0);
        // leave ~5% for fees/slippage headroom
        let max_affordable = (cap * 0.90).max(10.0);
        let default_n = (cap * 0.50).clamp(10.0, max_affordable);
        let n = map
            .get("notional")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(default_n);
        let floor = 10.0_f64.min(max_affordable);
        let clamped = n.min(max_affordable).max(floor);
        if map.get("notional").map(|s| s.as_str()) != Some(&format!("{clamped}")) {
            if (n - clamped).abs() > 1e-9 {
                warn!(
                    "回测 notional 从 {} 调整为 {:.2}（资金 {:.2}，否则趋势策略会 0 成交）",
                    map.get("notional")
                        .map(|s| s.as_str())
                        .unwrap_or("<default 1000>"),
                    clamped,
                    capital
                );
            }
            map.insert("notional".to_string(), format!("{clamped:.4}"));
        }
    }

    if is_dca {
        let cap = capital.max(1.0);
        let max_amt = (cap * 0.5).max(1.0);
        let amt = map
            .get("amount")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(5.0)
            .min(max_amt)
            .max(1.0);
        map.insert("amount".to_string(), format!("{amt:.4}"));
    }

    map.into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn strategy_uses_maker_execution_model(strategy: &str) -> bool {
    matches!(strategy, "maker_quote" | "maker")
}

fn backtest_data_path(data_file: &str) -> String {
    if data_file.starts_with('/') || data_file.starts_with("backtests/") {
        data_file.to_string()
    } else {
        format!("backtests/data/{data_file}")
    }
}

fn split_chronological_holdout(
    candles: &[crate::lighter::types::Candlestick],
    train_ratio: f64,
) -> Result<(
    Vec<crate::lighter::types::Candlestick>,
    Vec<crate::lighter::types::Candlestick>,
)> {
    if candles.len() < 4 || !(0.5..0.9).contains(&train_ratio) {
        anyhow::bail!("At least four candles and a 0.5–0.9 train ratio are required");
    }
    if candles
        .windows(2)
        .any(|pair| pair[0].timestamp > pair[1].timestamp)
    {
        anyhow::bail!("Candles must be sorted chronologically");
    }
    let target = ((candles.len() as f64) * train_ratio).floor() as usize;
    let target = target.clamp(1, candles.len() - 1);
    let validation_timestamp = candles[target].timestamp;
    let split = candles.partition_point(|candle| candle.timestamp < validation_timestamp);
    if split == 0 || split == candles.len() {
        anyhow::bail!("Dataset does not contain enough distinct timestamps for holdout validation");
    }
    Ok((candles[..split].to_vec(), candles[split..].to_vec()))
}

#[derive(Clone, Copy, Debug)]
struct CashOpenSummary {
    trades: u64,
    net_pnl: f64,
    return_pct: f64,
}

fn group_candles_by_new_york_date(
    candles: &[crate::lighter::types::Candlestick],
) -> Vec<Vec<crate::lighter::types::Candlestick>> {
    use chrono_tz::America::New_York;
    let mut days = std::collections::BTreeMap::new();
    for candle in candles {
        let date = candle.timestamp.with_timezone(&New_York).date_naive();
        days.entry(date)
            .or_insert_with(Vec::new)
            .push(candle.clone());
    }
    days.into_values().collect()
}

fn summarize_cash_open_trades(
    trades: &[crate::backtest::results::BacktestTrade],
    initial_capital: f64,
) -> CashOpenSummary {
    use chrono::{Datelike, Timelike, Weekday};
    use chrono_tz::America::New_York;

    let mut summary = CashOpenSummary {
        trades: 0,
        net_pnl: 0.0,
        return_pct: 0.0,
    };
    for trade in trades {
        let local = trade.timestamp.with_timezone(&New_York);
        if matches!(local.weekday(), Weekday::Sat | Weekday::Sun) {
            continue;
        }
        let minute = local.hour() * 60 + local.minute();
        if (9 * 60 + 25..=9 * 60 + 50).contains(&minute) {
            summary.trades += 1;
            summary.net_pnl += trade.pnl - trade.commission;
        }
    }
    if initial_capital > 0.0 {
        summary.return_pct = summary.net_pnl / initial_capital * 100.0;
    }
    summary
}

/// HIP-3 growth-mode all-in rates for this wallet:
/// maker 0.29 bps = 1.5 * 0.1 * 2.0 * 0.96, taker 0.86 bps likewise.
/// Adverse 0.73 bps is the observed maker residual after fees on io: fills.
/// Live yaml fee numbers stay 1.5/4.5 (operator-confirmed); only the maker
/// backtest cost model uses the growth-mode effective rates.
fn maker_backtest_fee_bps(config_path: &str, settings: &config::Config) -> (f64, f64, f64) {
    let yaml_maker = settings
        .get_float("profitability.entry_fee_bps")
        .unwrap_or(0.0);
    let yaml_taker = settings
        .get_float("profitability.exit_fee_bps")
        .unwrap_or(2.25);
    let yaml_adverse = settings
        .get_float("profitability.adverse_selection_bps")
        .unwrap_or(1.0);
    if config_path.contains("hyperliquid") {
        (
            crate::risk::profitability::HIP3_GROWTH_MAKER_FEE_BPS,
            crate::risk::profitability::HIP3_GROWTH_TAKER_FEE_BPS,
            crate::risk::profitability::HIP3_GROWTH_ADVERSE_BPS,
        )
    } else {
        (yaml_maker, yaml_taker, yaml_adverse)
    }
}

fn configure_maker_backtest_engine(
    engine: crate::backtest::engine::BacktestEngine,
) -> Result<crate::backtest::engine::BacktestEngine> {
    let path = crate::env_profiles::selected_venue().config_path();
    let settings = config::Config::builder()
        .add_source(config::File::with_name(path))
        .build()
        .map_err(|error| anyhow::anyhow!("Failed to load maker backtest config: {error}"))?;
    let (maker_fee, taker_fee, adverse) = maker_backtest_fee_bps(path, &settings);
    let taker_slippage = settings
        .get_float("profitability.exit_slippage_bps")
        .unwrap_or(0.0);
    let fill_ratio = settings
        .get_float("profitability.maker_fill_ratio")
        .unwrap_or(0.5);
    let penetration = settings
        .get_float("profitability.maker_min_penetration_bps")
        .unwrap_or(2.0);
    let stop_loss = settings
        .get_float("risk.stop_loss.position_stop_loss_percent")
        .unwrap_or(3.0)
        / 100.0;
    let take_profit = settings
        .get_float("risk.stop_loss.position_take_profit_percent")
        .unwrap_or(5.0)
        / 100.0;
    let max_position_notional = settings
        .get_float("risk.position_limit.max_position_size")
        .unwrap_or(100.0);
    let max_total_notional_pct = settings
        .get_float("trading.position.max_total_position_percent")
        .unwrap_or(25.0)
        / 100.0;

    engine
        .with_execution_costs(
            maker_fee / 10_000.0,
            taker_fee / 10_000.0,
            taker_slippage / 10_000.0,
        )?
        .with_conservative_maker_model(fill_ratio, penetration, adverse)?
        .with_position_risk(stop_loss, take_profit)?
        .with_max_position_notional(max_position_notional)?
        .with_max_total_notional_pct(max_total_notional_pct)
}

async fn execute_backtest_on_data(
    strategy: &str,
    params: &str,
    capital: f64,
    historical_data: Vec<crate::lighter::types::Candlestick>,
) -> Result<(String, crate::backtest::results::BacktestResults)> {
    if historical_data.is_empty() {
        anyhow::bail!("No candles available for backtest");
    }
    let normalized = normalize_backtest_params(strategy, params, capital);
    let bt_strategy = crate::strategy::create_strategy_with_params(
        strategy,
        if normalized.is_empty() {
            None
        } else {
            Some(normalized.as_str())
        },
    )
    .map_err(|error| anyhow::anyhow!("Invalid strategy: {error}"))?;
    let mut engine = crate::backtest::engine::BacktestEngine::new(capital, historical_data);
    if strategy_uses_maker_execution_model(strategy) {
        engine = configure_maker_backtest_engine(engine.with_maker_fills(true))?;
    }
    let results = engine
        .run(bt_strategy)
        .await
        .map_err(|error| anyhow::anyhow!("Backtest failed: {error}"))?;
    Ok((normalized, results))
}

async fn run_backtest_on_data(
    strategy: &str,
    params: &str,
    data_file: &str,
    capital: f64,
    historical_data: Vec<crate::lighter::types::Candlestick>,
) -> Result<serde_json::Value> {
    let candle_count = historical_data.len();
    let (normalized, results) =
        execute_backtest_on_data(strategy, params, capital, historical_data).await?;
    let cash_open = summarize_cash_open_trades(&results.trades, results.initial_capital);

    Ok(serde_json::json!({
        "status": "ok",
        "strategy": strategy,
        "data_file": data_file,
        "params": normalized,
        "candles": candle_count,
        "execution_model": if strategy_uses_maker_execution_model(strategy) {
            "maker_next_bar_cross"
        } else {
            "market_at_close"
        },
        "total_return_pct": results.total_return * 100.0,
        "sharpe_ratio": results.sharpe_ratio,
        "max_drawdown_pct": results.max_drawdown * 100.0,
        "total_trades": results.total_trades,
        "winning_trades": results.winning_trades,
        "losing_trades": results.losing_trades,
        "win_rate_pct": results.win_rate * 100.0,
        "profit_factor": results.profit_factor,
        "avg_profit": results.avg_profit,
        "avg_loss": results.avg_loss,
        "initial_capital": results.initial_capital,
        "final_capital": results.final_capital,
        "total_commission": results.total_commission,
        "total_adverse_selection": results.total_adverse_selection,
        "peak_notional": results.peak_notional,
        "peak_notional_pct": results.peak_leverage * 100.0,
        "blocked_by_position_limit": results.blocked_by_position_limit,
        "blocked_by_total_position_limit": results.blocked_by_total_position_limit,
        "stop_loss_exits": results.stop_loss_exits,
        "take_profit_exits": results.take_profit_exits,
        "cash_open_return_pct": cash_open.return_pct,
        "cash_open_trades": cash_open.trades,
        "equity_curve": results.equity_curve.iter()
            .map(|(ts, eq)| serde_json::json!({"t": ts.timestamp(), "v": eq}))
            .collect::<Vec<_>>(),
        "trades": results.trades.iter().take(100)
            .map(|trade| serde_json::json!({
                "timestamp": trade.timestamp.to_rfc3339(),
                "symbol": trade.symbol,
                "side": format!("{:?}", trade.side),
                "price": trade.price,
                "quantity": trade.quantity,
                "pnl": trade.pnl,
                "commission": trade.commission,
            }))
            .collect::<Vec<_>>(),
    }))
}

async fn run_validated_backtest_result(
    strategy: &str,
    params: &str,
    data_file: &str,
    capital: f64,
    start: &str,
    end: &str,
) -> Result<serde_json::Value> {
    if data_file.is_empty() || start.is_empty() || end.is_empty() {
        anyhow::bail!("Missing required fields: data_file, start, end");
    }
    let historical_data =
        crate::data::loader::load_csv_data_in_range(&backtest_data_path(data_file), start, end)
            .map_err(|error| anyhow::anyhow!("Data load failed: {error}"))?;
    let (_, validation_data) = split_chronological_holdout(&historical_data, 0.70)?;
    let validation_days = if strategy_uses_maker_execution_model(strategy) {
        let days = group_candles_by_new_york_date(&validation_data);
        days.into_iter()
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let validation_markets = if strategy_uses_maker_execution_model(strategy) {
        let mut markets = std::collections::BTreeMap::new();
        for candle in &validation_data {
            markets
                .entry(candle.symbol.clone())
                .or_insert_with(Vec::new)
                .push(candle.clone());
        }
        markets.into_values().collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let mut full =
        run_backtest_on_data(strategy, params, data_file, capital, historical_data).await?;
    let validation = run_backtest_on_data(
        strategy,
        params,
        data_file,
        capital,
        validation_data.clone(),
    )
    .await?;

    let mut rolling_profitable_days = 0_u64;
    for day in &validation_days {
        let (_, result) = execute_backtest_on_data(strategy, params, capital, day.clone()).await?;
        if result.total_return > 0.0 {
            rolling_profitable_days += 1;
        }
    }
    let mut validation_profitable_market_count = 0_u64;
    for market in &validation_markets {
        let (_, result) =
            execute_backtest_on_data(strategy, params, capital, market.clone()).await?;
        if result.total_return > 0.0 {
            validation_profitable_market_count += 1;
        }
    }
    let object = full
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Backtest result was not an object"))?;
    object.insert("validation".to_string(), validation.clone());
    object.insert(
        "validation_return_pct".to_string(),
        validation["total_return_pct"].clone(),
    );
    object.insert(
        "validation_sharpe_ratio".to_string(),
        validation["sharpe_ratio"].clone(),
    );
    object.insert(
        "validation_max_drawdown_pct".to_string(),
        validation["max_drawdown_pct"].clone(),
    );
    object.insert(
        "validation_total_trades".to_string(),
        validation["total_trades"].clone(),
    );
    object.insert(
        "validation_peak_notional_pct".to_string(),
        validation["peak_notional_pct"].clone(),
    );
    object.insert(
        "rolling_days".to_string(),
        serde_json::json!(validation_days.len()),
    );
    object.insert(
        "rolling_profitable_days".to_string(),
        serde_json::json!(rolling_profitable_days),
    );
    object.insert(
        "cash_open_return_pct".to_string(),
        validation["cash_open_return_pct"].clone(),
    );
    object.insert(
        "cash_open_trades".to_string(),
        validation["cash_open_trades"].clone(),
    );
    object.insert(
        "validation_market_count".to_string(),
        serde_json::json!(validation_markets.len()),
    );
    object.insert(
        "validation_profitable_market_count".to_string(),
        serde_json::json!(validation_profitable_market_count),
    );
    Ok(full)
}

/// Score a sweep row for ranking. Higher is better for all goals.
fn score_optimize_row(goal: &str, ret: f64, sharpe: f64, max_dd: f64, trades: u64) -> f64 {
    if trades == 0 {
        return f64::NEG_INFINITY;
    }
    match goal {
        "return" => ret,
        "drawdown" => -max_dd, // lower drawdown wins
        "balanced" => ret - max_dd * 0.5 + sharpe * 2.0,
        _ => sharpe, // default sharpe
    }
}

fn build_param_grid(strategy: &str, mode: &str, capital: f64) -> Result<Vec<String>> {
    let quick = mode != "full";
    match strategy {
        "grid" | "grid_trading" => {
            let grid_counts: &[i32] = if quick {
                &[6, 8, 10, 12, 16]
            } else {
                &[4, 6, 8, 10, 12, 14, 20]
            };
            // Keep per-grid investment affordable relative to capital.
            let inv_pool: Vec<f64> = if quick {
                vec![8.0, 15.0, 30.0, 50.0]
            } else {
                vec![5.0, 8.0, 12.0, 16.0, 30.0, 50.0]
            };
            let mut investments: Vec<f64> = Vec::new();
            for v in inv_pool {
                let clamped = v.min((capital * 0.4).max(3.0));
                if investments.iter().all(|x| (*x - clamped).abs() > 1e-9) {
                    investments.push(clamped);
                }
            }
            let deviations: &[f64] = if quick {
                &[0.004, 0.008, 0.012, 0.02]
            } else {
                &[0.003, 0.005, 0.008, 0.012, 0.016, 0.02, 0.03]
            };
            let mut sets = Vec::new();
            for &gc in grid_counts {
                for &inv in &investments {
                    for &dev in deviations {
                        sets.push(format!("grid_count={gc},investment={inv},deviation={dev}"));
                    }
                }
            }
            Ok(sets)
        }
        "trend" | "trend_following" => {
            let fast: &[i32] = if quick {
                &[5, 7, 10, 14]
            } else {
                &[5, 7, 10, 14, 21]
            };
            let slow: &[i32] = if quick {
                &[21, 30, 50]
            } else {
                &[14, 21, 30, 50, 80]
            };
            let sls: &[f64] = if quick {
                &[0.03, 0.05]
            } else {
                &[0.02, 0.03, 0.05]
            };
            let tps: &[f64] = if quick {
                &[0.06, 0.10]
            } else {
                &[0.04, 0.06, 0.10]
            };
            // Critical: embed affordable notional so opens can fill under small capital.
            let max_n = (capital * 0.90).max(10.0);
            let notionals: Vec<f64> = if quick {
                vec![
                    (capital * 0.45).clamp(10.0, max_n),
                    (capital * 0.75).clamp(10.0, max_n),
                ]
            } else {
                vec![
                    (capital * 0.30).clamp(10.0, max_n),
                    (capital * 0.50).clamp(10.0, max_n),
                    (capital * 0.80).clamp(10.0, max_n),
                ]
            };
            let mut sets = Vec::new();
            for &f in fast {
                for &s in slow {
                    if f >= s {
                        continue;
                    }
                    for &sl in sls {
                        for &tp in tps {
                            if tp <= sl {
                                continue;
                            }
                            for &n in &notionals {
                                sets.push(format!(
                                    "fast_ma={f},slow_ma={s},stop_loss={sl},take_profit={tp},trailing_stop=0,notional={n:.2}"
                                ));
                            }
                        }
                    }
                }
            }
            Ok(sets)
        }
        "maker_quote" | "maker" => {
            let spreads: &[f64] = if quick {
                &[4.0, 6.0, 8.0, 12.0]
            } else {
                &[3.0, 4.0, 6.0, 8.0, 10.0, 12.0, 16.0]
            };
            let volatility_multipliers: &[f64] = if quick {
                &[0.0, 0.5]
            } else {
                &[0.0, 0.25, 0.5, 1.0]
            };
            let trend_blocks: &[f64] = if quick {
                &[3.0, 6.0]
            } else {
                &[3.0, 6.0, 10.0]
            };
            let policy_budget = (capital * 0.20).max(5.0);
            let quote_sizes = [
                (policy_budget * 0.25).max(5.0).min(policy_budget),
                (policy_budget * 0.50).max(5.0).min(policy_budget),
            ];
            let soft_cap = policy_budget * 0.60;
            let hard_cap = policy_budget * 0.80;
            let min_quote = 5.0_f64.min(quote_sizes[0]);
            let mut sets = Vec::new();
            for spread in spreads {
                for quote in quote_sizes {
                    for volatility_multiplier in volatility_multipliers {
                        for trend_block in trend_blocks {
                            sets.push(format!(
                                "spread_bps={spread},per_quote_notional={quote:.2},requote_threshold_bps=2,requote_cooldown_secs=5,soft_cap_notional={soft_cap:.2},hard_cap_notional={hard_cap:.2},trend_filter=1,ema_period=20,trend_block_bps={trend_block},min_quote_notional={min_quote:.2},feature_interval_secs=60,total_quote_budget={policy_budget:.2},vol_window=24,vol_multiplier={volatility_multiplier},max_skew_bps=3"
                            ));
                        }
                    }
                }
            }
            Ok(sets)
        }
        "dca" => {
            // DCA has fewer knobs; keep a compact grid.
            let intervals = [2.0, 4.0, 8.0, 12.0];
            let amounts = [5.0, 10.0, 20.0];
            let dips = [0.01, 0.02, 0.03];
            let mut sets = Vec::new();
            for &iv in &intervals {
                for &amt in &amounts {
                    for &dip in &dips {
                        sets.push(format!(
                            "interval={iv},amount={amt},dip_threshold={}",
                            dip * 100.0
                        ));
                    }
                }
            }
            Ok(sets)
        }
        other => anyhow::bail!("Unsupported strategy for optimize: {other}"),
    }
}

/// Local parameter sweep — no external AI required.
/// Loads candles once, runs many param combos, returns top rows + full best backtest.
async fn backtest_optimize_handler(
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let strategy = body
        .get("strategy")
        .and_then(|s| s.as_str())
        .unwrap_or("grid");
    let data_file = body.get("data_file").and_then(|s| s.as_str()).unwrap_or("");
    let capital = body
        .get("capital")
        .and_then(|c| c.as_f64())
        .unwrap_or(125.0);
    let start = body
        .get("start")
        .or_else(|| body.get("start_date"))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let end = body
        .get("end")
        .or_else(|| body.get("end_date"))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let goal = body
        .get("goal")
        .and_then(|s| s.as_str())
        .unwrap_or("sharpe");
    let mode = body.get("mode").and_then(|s| s.as_str()).unwrap_or("quick");
    let baseline_params = body
        .get("params")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();

    if data_file.is_empty() || start.is_empty() || end.is_empty() {
        return axum::Json(serde_json::json!({
            "status": "error",
            "message": "Missing required fields: data_file, start, end"
        }));
    }

    let param_sets = match build_param_grid(strategy, mode, capital) {
        Ok(v) => v,
        Err(e) => {
            return axum::Json(serde_json::json!({"status":"error","message": e.to_string()}))
        }
    };

    let data_path = backtest_data_path(data_file);
    let historical_data = match crate::data::loader::load_csv_data_in_range(&data_path, start, end)
    {
        Ok(d) if !d.is_empty() => d,
        Ok(_) => {
            return axum::Json(serde_json::json!({
                "status": "error",
                "message": format!("No candles in range {start} → {end} for {data_file}")
            }))
        }
        Err(e) => {
            return axum::Json(serde_json::json!({
                "status": "error",
                "message": format!("Data load failed: {e}")
            }))
        }
    };
    let candle_count = historical_data.len();
    let (training_data, validation_data) = match split_chronological_holdout(&historical_data, 0.70)
    {
        Ok(parts) => parts,
        Err(error) => {
            return axum::Json(serde_json::json!({
                "status": "error",
                "message": format!("Holdout split failed: {error}")
            }))
        }
    };

    // Optional baseline (user's current params)
    let mut baseline_json = None;
    if !baseline_params.trim().is_empty() {
        if let Ok(base) = run_validated_backtest_result(
            strategy,
            &baseline_params,
            data_file,
            capital,
            start,
            end,
        )
        .await
        {
            baseline_json = Some(base);
        }
    }

    #[derive(Clone)]
    struct Row {
        params: String,
        training_return_pct: f64,
        training_sharpe_ratio: f64,
        training_max_drawdown_pct: f64,
        training_total_trades: u64,
        total_return_pct: f64,
        sharpe_ratio: f64,
        max_drawdown_pct: f64,
        total_trades: u64,
        win_rate_pct: f64,
        profit_factor: f64,
        score: f64,
    }

    let mut rows: Vec<Row> = Vec::with_capacity(param_sets.len());
    for params in &param_sets {
        let (normalized, training) = match execute_backtest_on_data(
            strategy,
            params,
            capital,
            training_data.clone(),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => continue,
        };
        let (_, validation) =
            match execute_backtest_on_data(strategy, &normalized, capital, validation_data.clone())
                .await
            {
                Ok(result) => result,
                Err(_) => continue,
            };
        let training_return_pct = training.total_return * 100.0;
        let training_max_drawdown_pct = training.max_drawdown * 100.0;
        let training_trades = training.total_trades as u64;
        let validation_return_pct = validation.total_return * 100.0;
        let validation_max_drawdown_pct = validation.max_drawdown * 100.0;
        let validation_trades = validation.total_trades as u64;
        let eligible = training_return_pct.is_finite()
            && training_return_pct > 0.0
            && training.sharpe_ratio.is_finite()
            && training.sharpe_ratio > 0.0
            && training_trades >= 3
            && validation_return_pct.is_finite()
            && validation_return_pct > 0.0
            && validation.sharpe_ratio.is_finite()
            && validation.sharpe_ratio > 0.0
            && validation_trades >= 3;
        let score = if eligible {
            score_optimize_row(
                goal,
                validation_return_pct,
                validation.sharpe_ratio,
                validation_max_drawdown_pct,
                validation_trades,
            )
        } else {
            f64::NEG_INFINITY
        };
        rows.push(Row {
            params: normalized,
            training_return_pct,
            training_sharpe_ratio: training.sharpe_ratio,
            training_max_drawdown_pct,
            training_total_trades: training_trades,
            total_return_pct: validation_return_pct,
            sharpe_ratio: validation.sharpe_ratio,
            max_drawdown_pct: validation_max_drawdown_pct,
            total_trades: validation_trades,
            win_rate_pct: validation.win_rate * 100.0,
            profit_factor: validation.profit_factor,
            score,
        });
    }

    rows.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let top_n = 15usize;
    let top: Vec<serde_json::Value> = rows
        .iter()
        .take(top_n)
        .enumerate()
        .map(|(i, r)| {
            serde_json::json!({
                "rank": i + 1,
                "params": r.params,
                "eligible": r.score.is_finite(),
                "training_return_pct": r.training_return_pct,
                "training_sharpe_ratio": r.training_sharpe_ratio,
                "training_max_drawdown_pct": r.training_max_drawdown_pct,
                "training_total_trades": r.training_total_trades,
                "total_return_pct": r.total_return_pct,
                "sharpe_ratio": r.sharpe_ratio,
                "max_drawdown_pct": r.max_drawdown_pct,
                "total_trades": r.total_trades,
                "win_rate_pct": r.win_rate_pct,
                "profit_factor": r.profit_factor,
                "score": r.score,
            })
        })
        .collect();

    let best_params = rows
        .iter()
        .find(|row| row.score.is_finite())
        .map(|row| row.params.clone());
    let best_detail = if let Some(ref p) = best_params {
        run_validated_backtest_result(strategy, p, data_file, capital, start, end)
            .await
            .ok()
    } else {
        None
    };

    let profitable = rows.iter().filter(|row| row.score.is_finite()).count();
    let with_trades = rows.iter().filter(|row| row.total_trades > 0).count();

    axum::Json(serde_json::json!({
        "status": "ok",
        "mode": mode,
        "goal": goal,
        "strategy": strategy,
        "data_file": data_file,
        "start": start,
        "end": end,
        "candles": candle_count,
        "tested": rows.len(),
        "with_trades": with_trades,
        "profitable": profitable,
        "optimized_params": best_params,
        "baseline": baseline_json,
        "optimized": best_detail,
        "leaderboard": top,
        "message": format!(
            "Scanned {n} combos on {candles} candles; {profit} profitable, {wt} with trades. Best by {goal}.",
            n = rows.len(),
            candles = candle_count,
            profit = profitable,
            wt = with_trades,
            goal = goal
        ),
    }))
}

fn build_opencode_prompt(base: &serde_json::Value, params: &str, goal: &str) -> String {
    let goal_text = match goal {
        "return" => "Maximize total return",
        "drawdown" => "Minimize max drawdown",
        "balanced" => "Balance return and risk",
        _ => "Maximize Sharpe ratio",
    };
    format!(
        "Optimize this crypto grid strategy.\n\
Current params: {params}\n\
Return={ret:.2}% Sharpe={sharpe:.2} MaxDD={dd:.2}% ProfitFactor={pf:.2} Trades={trades}\n\
Goal: {goal_text}\n\
Allowed: grid_count 4-20, investment 3-80, deviation 0.005-0.03\n\
Reply with EXACTLY ONE LINE ONLY:\n\
PARAMS: grid_count=X,investment=Y,deviation=Z",
        ret = base["total_return_pct"].as_f64().unwrap_or(0.0),
        sharpe = base["sharpe_ratio"].as_f64().unwrap_or(0.0),
        dd = base["max_drawdown_pct"].as_f64().unwrap_or(0.0),
        pf = base["profit_factor"].as_f64().unwrap_or(0.0),
        trades = base["total_trades"].as_u64().unwrap_or(0),
    )
}

fn parse_suggested_params(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("PARAMS:") {
            return Some(rest.trim().to_string());
        }
    }
    let compact = text.replace(' ', "");
    let gc = compact
        .split("grid_count=")
        .nth(1)?
        .split([',', '\n'])
        .next()?;
    let inv = compact
        .split("investment=")
        .nth(1)?
        .split([',', '\n'])
        .next()?;
    let dev = compact
        .split("deviation=")
        .nth(1)?
        .split([',', '\n'])
        .next()?;
    Some(format!("grid_count={gc},investment={inv},deviation={dev}"))
}

async fn opencode_optimize_handler(
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let strategy = body
        .get("strategy")
        .and_then(|s| s.as_str())
        .unwrap_or("grid");
    let params = body
        .get("params")
        .and_then(|s| s.as_str())
        .unwrap_or("grid_count=10,investment=8,deviation=0.012");
    let data_file = body.get("data_file").and_then(|s| s.as_str()).unwrap_or("");
    let capital = body
        .get("capital")
        .and_then(|c| c.as_f64())
        .unwrap_or(125.0);
    let start = body.get("start").and_then(|s| s.as_str()).unwrap_or("");
    let end = body.get("end").and_then(|s| s.as_str()).unwrap_or("");
    let goal = body
        .get("goal")
        .and_then(|s| s.as_str())
        .unwrap_or("sharpe");
    // No silent GLM5 default — caller must pass an explicit local model.
    let model = body
        .get("opencode_model")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(model) = model else {
        return axum::Json(serde_json::json!({
            "status": "error",
            "message": "opencode_model is required. Prefer the dashboard AI Lab provider/model fields for normal optimization; OpenCode is an optional local CLI path only."
        }));
    };

    let base = match run_validated_backtest_result(strategy, params, data_file, capital, start, end)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            return axum::Json(serde_json::json!({"status": "error", "message": e.to_string()}))
        }
    };

    let prompt = build_opencode_prompt(&base, params, goal);
    let output = match timeout(
        Duration::from_secs(240),
        Command::new("opencode")
            .arg("run")
            .arg("--pure")
            .arg("-m")
            .arg(model)
            .arg("--dir")
            .arg(".")
            .arg(prompt)
            .output(),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return axum::Json(serde_json::json!({
                "status": "error",
                "message": format!("OpenCode invocation failed: {}", e)
            }))
        }
        Err(_) => {
            return axum::Json(serde_json::json!({
                "status": "error",
                "message": "OpenCode request timed out after 240 seconds"
            }))
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = if stdout.trim().is_empty() {
        stderr.clone()
    } else {
        format!("{stdout}\n{stderr}")
    };
    let suggested = match parse_suggested_params(&combined) {
        Some(value) => value,
        None => {
            let preview = combined.chars().take(500).collect::<String>();
            return axum::Json(serde_json::json!({
                "status": "error",
                "message": format!("OpenCode did not return parsable parameters. Output: {}", preview)
            }));
        }
    };

    match run_validated_backtest_result(strategy, &suggested, data_file, capital, start, end).await
    {
        Ok(optimized) => axum::Json(serde_json::json!({
            "status": "ok",
            "model": model,
            "base": base,
            "optimized": optimized,
            "optimized_params": suggested,
            "suggestion": combined,
        })),
        Err(e) => axum::Json(serde_json::json!({
            "status": "error",
            "message": format!("Optimized backtest failed: {}", e),
            "base": base,
            "optimized_params": suggested,
            "suggestion": combined,
        })),
    }
}

fn normalize_omp_collab_url(raw: &str) -> Option<String> {
    const PREFIX: &str = "https://my.omp.sh/#";

    let url = raw.trim();
    let room = url.strip_prefix(PREFIX)?;
    let (room_id, room_key) = room.split_once('.')?;
    let valid_part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    if valid_part(room_id) && valid_part(room_key) {
        Some(url.to_owned())
    } else {
        None
    }
}

fn select_omp_collab_url(environment: Option<&str>, runtime_file: Option<&str>) -> Option<String> {
    environment
        .and_then(normalize_omp_collab_url)
        .or_else(|| runtime_file.and_then(normalize_omp_collab_url))
}

fn omp_collab_url() -> Option<String> {
    let environment = std::env::var("OMP_COLLAB_WEB_URL").ok();
    let runtime_file = std::fs::read_to_string(OMP_COLLAB_URL_FILE).ok();
    select_omp_collab_url(environment.as_deref(), runtime_file.as_deref())
}

async fn ai_page_handler() -> Response {
    match omp_collab_url() {
        Some(url) => (
            [
                (header::CACHE_CONTROL, "no-store"),
                (header::REFERRER_POLICY, "no-referrer"),
            ],
            Redirect::temporary(&url),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CACHE_CONTROL, "no-store")],
            Html(
                "<!doctype html><meta charset=\"utf-8\"><title>OMP Web unavailable</title>\
                 <h1>OMP Web session unavailable</h1>\
                 <p>Start the repository OMP host with <code>/collab</code>, then write its \
                 browser URL to <code>.omp/collab-url</code>.</p>",
            ),
        )
            .into_response(),
    }
}

// ── Trading Control Endpoints ──

async fn markets_get_handler(
    State(state): State<SharedDashboardState>,
) -> axum::Json<serde_json::Value> {
    let ds = state.read().await;
    axum::Json(serde_json::json!({
        "active_markets": ds.active_markets,
        "available_markets": ds.available_markets.iter().map(|(id, sym)| {
            serde_json::json!({ "id": id, "symbol": sym, "active": ds.active_markets.contains(id) })
        }).collect::<Vec<_>>(),
        "trading_paused": ds.trading_paused,
    }))
}

async fn markets_update_handler(
    State(state): State<SharedDashboardState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    let mut ds = state.write().await;
    if let Some(markets) = body.get("markets").and_then(|v| v.as_array()) {
        let new_markets: Vec<u32> = markets
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u32))
            .collect();
        info!("📊 Trading markets updated: {:?}", new_markets);
        ds.active_markets = new_markets.clone();
        axum::Json(serde_json::json!({
            "status": "ok",
            "message": format!("Active markets updated to {:?}", new_markets),
            "active_markets": new_markets,
        }))
    } else {
        axum::Json(serde_json::json!({
            "status": "error",
            "message": "Invalid request. Expected: {\"markets\": [0, 1]}"
        }))
    }
}

async fn trading_pause_handler(
    State(state): State<SharedDashboardState>,
) -> axum::Json<serde_json::Value> {
    let mut ds = state.write().await;
    ds.trading_paused = true;
    PersistentStrategyConfig::save(&ds);
    info!("⏸️ Trading PAUSED via dashboard");
    axum::Json(serde_json::json!({
        "status": "ok",
        "message": "Trading paused. New entries are blocked; explicit risk-reducing exits remain active.",
        "trading_paused": true,
    }))
}

async fn trading_resume_handler(
    State(state): State<SharedDashboardState>,
) -> axum::Json<serde_json::Value> {
    let mut ds = state.write().await;
    let policy = agent_policy(&ds);
    let params = ds
        .strategy_params
        .iter()
        .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
        .collect();
    let mut violations =
        super::quant_agent::validate_strategy_params(&ds.strategy_name, &params, &policy);
    if policy.emergency_triggered {
        violations.push("risk emergency is active".to_string());
    }
    if !policy.equity.is_finite() || policy.equity <= 0.0 {
        violations.push("account equity is unavailable".to_string());
    }
    if !violations.is_empty() {
        warn!(
            "Trading resume blocked by runtime policy: {}",
            violations.join("; ")
        );
        return axum::Json(serde_json::json!({
            "status": "error",
            "message": "Trading remains paused because the active strategy violates runtime policy.",
            "violations": violations,
            "trading_paused": true,
        }));
    }

    ds.trading_paused = false;
    PersistentStrategyConfig::save(&ds);
    info!("▶️ Trading RESUMED via dashboard after runtime policy validation");
    axum::Json(serde_json::json!({
        "status": "ok",
        "message": "Trading resumed. Orders will be placed normally.",
        "trading_paused": false,
    }))
}

async fn cancel_all_handler(
    State(state): State<SharedDashboardState>,
) -> axum::Json<serde_json::Value> {
    let mut ds = state.write().await;
    ds.cancel_all_requested = true;
    info!("🗑️ Cancel all orders requested via dashboard");
    axum::Json(serde_json::json!({
        "status": "ok",
        "message": "Cancel all orders requested. Will execute on next cycle.",
    }))
}

async fn risk_config_get_handler(
    State(state): State<SharedDashboardState>,
) -> axum::Json<serde_json::Value> {
    let ds = state.read().await;
    axum::Json(ds.risk_config.clone())
}

async fn risk_config_update_handler(
    State(state): State<SharedDashboardState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    let mut ds = state.write().await;
    // Update leverage_limit if provided
    if let Some(v) = body.get("leverage_limit").and_then(|v| v.as_f64()) {
        ds.leverage_limit = v;
    }
    // Store the update request for main loop to pick up
    ds.risk_update_requested = Some(body.clone());
    // Update cached config
    let fields = [
        "max_drawdown_pct",
        "daily_loss_limit_pct",
        "max_leverage",
        "position_stop_loss_pct",
        "position_take_profit_pct",
        "leverage_limit",
    ];
    for field in &fields {
        if let Some(v) = body.get(*field) {
            ds.risk_config[field] = v.clone();
        }
    }
    info!("🔧 Risk config updated from dashboard: {}", body);
    PersistentRiskConfig::save(&ds);
    axum::Json(serde_json::json!({
        "status": "ok",
        "message": "Risk parameters updated",
        "config": ds.risk_config.clone(),
    }))
}

#[cfg(test)]
mod auth_tests {
    use super::{credential_fields, masked_secret, request_is_authorized};
    use axum::http::{header, HeaderMap, HeaderValue};
    use multi_venue_quant_bot::exchange::LiveVenue;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn dashboard_mutation_auth_accepts_bearer_or_strict_cookie() {
        let mut bearer = HeaderMap::new();
        bearer.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer 0123456789abcdef0123456789abcdef"),
        );
        assert!(request_is_authorized(&bearer, TOKEN));

        let mut cookie = HeaderMap::new();
        cookie.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; quant_bot_auth=0123456789abcdef0123456789abcdef"),
        );
        assert!(request_is_authorized(&cookie, TOKEN));
    }

    #[test]
    fn dashboard_mutation_auth_rejects_missing_or_wrong_credentials() {
        assert!(!request_is_authorized(&HeaderMap::new(), TOKEN));
        let mut wrong = HeaderMap::new();
        wrong.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        assert!(!request_is_authorized(&wrong, TOKEN));
    }

    #[test]
    fn credential_fields_are_venue_specific() {
        let lighter = credential_fields(LiveVenue::LighterMainnet);
        assert!(lighter
            .iter()
            .any(|field| field.api_key == "LIGHTER_SECRET_KEY"));
        assert!(!lighter.iter().any(|field| field.api_key == "ARCUS_API_KEY"));

        let arcus = credential_fields(LiveVenue::ArcusMainnet);
        assert!(arcus.iter().any(|field| field.api_key == "ARCUS_API_KEY"));
        assert!(arcus
            .iter()
            .any(|field| field.api_key == "ARCUS_SIGNING_KEY" && field.secret));
        assert!(!arcus
            .iter()
            .any(|field| field.api_key == "EXCHANGE_SECRET_KEY"));

        let aster = credential_fields(LiveVenue::AsterMainnet);
        assert!(aster
            .iter()
            .any(|field| field.api_key == "ASTER_SIGNER_PRIVATE_KEY" && field.secret));
    }

    #[test]
    fn secret_mask_contains_metadata_but_never_plaintext() {
        let masked = masked_secret("highly-sensitive-private-key");
        assert_eq!(masked["configured"], true);
        assert!(masked.get("length").is_none());
        assert!(masked.get("tail").is_none());
        assert!(!masked.to_string().contains("highly-sensitive-private-key"));
    }
}

#[cfg(test)]
mod omp_web_tests {
    use super::select_omp_collab_url;

    #[test]
    fn collab_redirect_accepts_only_exact_https_omp_room_links() {
        assert_eq!(
            select_omp_collab_url(
                Some("  https://my.omp.sh/#room-id.room-key  "),
                Some("https://my.omp.sh/#fallback.key"),
            )
            .as_deref(),
            Some("https://my.omp.sh/#room-id.room-key"),
        );

        for unsafe_url in [
            "http://my.omp.sh/#room.key",
            "https://evil.example/#room.key",
            "https://my.omp.sh/path#room.key",
            "https://my.omp.sh/#",
            "javascript:alert(1)",
        ] {
            assert_eq!(select_omp_collab_url(Some(unsafe_url), None), None);
        }
    }

    #[test]
    fn invalid_environment_value_falls_back_to_runtime_file() {
        assert_eq!(
            select_omp_collab_url(
                Some("https://evil.example/#stolen.key"),
                Some("https://my.omp.sh/#runtime-room.runtime-key\n"),
            )
            .as_deref(),
            Some("https://my.omp.sh/#runtime-room.runtime-key"),
        );
    }
}

#[cfg(test)]
mod backtest_validation_tests {
    use super::{
        build_param_grid, group_candles_by_new_york_date, split_chronological_holdout,
        strategy_uses_maker_execution_model, summarize_cash_open_trades,
    };
    use crate::lighter::types::Candlestick;
    use chrono::{TimeZone, Utc};

    fn candle(timestamp: i64, symbol: &str) -> Candlestick {
        Candlestick {
            timestamp: Utc.timestamp_opt(timestamp, 0).unwrap(),
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.0,
            volume: 1.0,
            symbol: symbol.to_string(),
        }
    }

    #[test]
    fn hyperliquid_maker_backtest_uses_growth_mode_fees() {
        let settings = config::Config::builder()
            .add_source(config::File::with_name("config/settings.hyperliquid.yaml"))
            .build()
            .expect("yaml");
        let (maker, taker, adverse) =
            super::maker_backtest_fee_bps("config/settings.hyperliquid.yaml", &settings);
        assert!((maker - 0.29).abs() < 1e-9);
        assert!((taker - 0.86).abs() < 1e-9);
        assert!((adverse - 0.73).abs() < 1e-9);
        assert!(settings.get_float("profitability.entry_fee_bps").unwrap() > 1.0);
    }

    #[test]
    fn status_and_ws_push_include_user_fee_tier_fields() {
        let src = include_str!("server.rs");
        assert!(src.contains("user_add_rate_bps"));
        assert!(src.contains("user_cross_rate_bps"));
        assert!(src.contains("fee_tier_is_t4"));
        assert!(src.contains("last_cross_dex_net_bps"));
        assert!(src.contains("last_cross_dex_side"));
        assert!(src.contains("strategy_overlay"));
        assert!(
            src.matches("strategy_overlay").count() >= 3,
            "exists helper plus REST status plus WS status"
        );
        assert!(
            src.matches("ds.strategy_params.get(\"quote_mode\")").count() >= 2,
            "quote_mode on REST and WS status"
        );
        assert!(
            src.matches("ds.strategy_params.get(\"flatten_only\")").count() >= 2,
            "flatten_only on REST and WS status"
        );
        assert!(
            src.matches("user_add_rate_bps").count() >= 3,
            "field plus REST status plus WS status"
        );
        assert!(
            src.matches("last_cross_dex_net_bps").count() >= 3,
            "cross-dex field plus REST status plus WS status"
        );
    }

    #[test]
    fn maker_quote_uses_resting_fill_model_and_has_a_bounded_grid() {
        assert!(strategy_uses_maker_execution_model("maker_quote"));
        let grid = build_param_grid("maker_quote", "quick", 100.0).expect("maker grid");
        assert!(!grid.is_empty());
        assert!(grid.iter().all(|params| {
            params.contains("spread_bps=")
                && params.contains("per_quote_notional=")
                && params.contains("total_quote_budget=")
        }));
    }

    #[test]
    fn chronological_holdout_never_splits_one_market_timestamp() {
        let candles = vec![
            candle(1, "BTC"),
            candle(1, "ETH"),
            candle(2, "BTC"),
            candle(2, "ETH"),
            candle(3, "BTC"),
            candle(3, "ETH"),
            candle(4, "BTC"),
            candle(4, "ETH"),
        ];
        let (train, validation) =
            split_chronological_holdout(&candles, 0.70).expect("valid chronological split");
        assert!(!train.is_empty());
        assert!(!validation.is_empty());
        assert!(train.last().unwrap().timestamp < validation.first().unwrap().timestamp);
        assert_eq!(train.len() + validation.len(), candles.len());
    }

    #[test]
    fn rolling_days_follow_new_york_calendar_across_dst() {
        let candles = vec![
            candle(1_786_420_740, "BTC"), // 2026-08-11 03:59 UTC = Aug 10 NY
            candle(1_786_420_800, "BTC"), // 2026-08-11 04:00 UTC = Aug 11 NY
            candle(1_767_589_140, "BTC"), // 2026-01-05 04:59 UTC = Jan 4 NY
            candle(1_767_589_200, "BTC"), // 2026-01-05 05:00 UTC = Jan 5 NY
        ];

        let groups = group_candles_by_new_york_date(&candles);

        assert_eq!(groups.len(), 4);
        assert!(groups.iter().all(|day| day.len() == 1));
    }

    #[test]
    fn cash_open_summary_is_dst_aware_and_nets_commission() {
        use crate::backtest::results::BacktestTrade;
        use crate::lighter::types::Side;

        let trade = |timestamp, pnl, commission| BacktestTrade {
            timestamp: Utc.timestamp_opt(timestamp, 0).unwrap(),
            symbol: "TEST".into(),
            side: Side::Sell,
            price: 100.0,
            quantity: 1.0,
            pnl,
            commission,
        };
        let trades = vec![
            trade(1_786_455_600, -2.0, 0.1), // 2026-08-11 13:40 UTC = 09:40 EDT
            trade(1_768_228_200, 1.0, 0.1),  // 2026-01-12 14:30 UTC = 09:30 EST
            trade(1_768_224_600, 9.0, 0.0),  // 08:30 EST, excluded
        ];

        let summary = summarize_cash_open_trades(&trades, 1_000.0);

        assert_eq!(summary.trades, 2);
        assert!((summary.net_pnl + 1.2).abs() < 1e-12);
        assert!((summary.return_pct + 0.12).abs() < 1e-12);
    }
}
