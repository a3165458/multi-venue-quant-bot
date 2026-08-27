// src/main.rs
mod aster_hft_shadow;
mod aster_live;
mod aster_shadow;
mod backtest;
mod dashboard;
mod data;
mod env_profiles;
mod hft;
mod hyperliquid_live;
mod lighter;
mod risk;
mod strategy;
mod utils;

#[cfg(test)]
#[path = "hft_tests.rs"]
mod hft_tests;

#[cfg(test)]
#[path = "aster_live_tests.rs"]
mod aster_live_tests;

#[cfg(test)]
#[path = "aster_shadow_tests.rs"]
mod aster_shadow_tests;

#[cfg(test)]
#[path = "aster_hft_shadow_tests.rs"]
mod aster_hft_shadow_tests;

#[cfg(test)]
#[path = "hyperliquid_live_tests.rs"]
mod hyperliquid_live_tests;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use config::Config;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

#[derive(Parser)]
#[command(author, version, about = "Multi-Venue Quant Bot", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run live trading
    Live {
        /// Config file. When omitted, LIGHTER_NETWORK selects mainnet or Robinhood Chain.
        #[arg(short, long)]
        config: Option<String>,
    },

    /// Run backtest
    Backtest {
        #[arg(short, long)]
        strategy: String,
        #[arg(short, long)]
        data: String,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
        #[arg(long, default_value = "10000")]
        capital: f64,
        #[arg(short, long)]
        output: Option<String>,
        /// Strategy params: "grid_count=10,investment=8.0,deviation=0.008"
        #[arg(short, long)]
        params: Option<String>,
        /// Config file for the profitability gate (same section as live)
        #[arg(long)]
        config: Option<String>,
        /// Maker fill model: post_only signals only fill when the candle range
        /// crosses their quote price; no slippage. Wire commission/slippage from
        /// the config's profitability section (maker economics).
        #[arg(long)]
        maker: bool,
    },

    /// Run parameter optimization sweep
    Optimize {
        #[arg(short, long)]
        strategy: String,
        #[arg(short, long)]
        data: String,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
        #[arg(long, default_value = "10000")]
        capital: f64,
        #[arg(short, long)]
        output: Option<String>,
        /// Config file for the profitability gate (same section as live)
        #[arg(long)]
        config: Option<String>,
    },

    /// Start dashboard only
    Dashboard {
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        #[arg(short, long, default_value = "4028")]
        port: u16,
    },

    /// Download historical data
    Download {
        #[arg(short, long)]
        symbol: String,
        #[arg(short, long, default_value = "1h")]
        interval: String,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
        /// REST API base URL (mainnet 或 Robinhood Chain 实例)
        #[arg(long, default_value = "https://mainnet.zklighter.elliot.ai")]
        url: String,
        /// 输出文件名标签（默认取 URL 推断: mainnet / rh）
        #[arg(long)]
        tag: Option<String>,
    },

    /// Download Arcus historical candles (paged) into a backtest CSV.
    /// 支持多市场：--symbols "BTC-USD,NVDA-USD" 写入同一文件（按 symbol 列区分）。
    DownloadArcus {
        /// 逗号分隔的市场符号，如 "BTC-USD,NVDA-USD,TSLA-USD"
        #[arg(short, long)]
        symbols: String,
        #[arg(short, long, default_value = "1h")]
        interval: String,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
        /// mainnet | testnet
        #[arg(long, default_value = "mainnet")]
        env: String,
        /// 输出文件名标签（默认 arcus）
        #[arg(long)]
        tag: Option<String>,
    },

    /// Download Aster Pro Futures V3 candles into a backtest CSV.
    DownloadAster {
        /// 逗号分隔的永续合约符号，如 "BTCUSDT,ETHUSDT"
        #[arg(short, long)]
        symbols: String,
        #[arg(short, long, default_value = "1h")]
        interval: String,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
        /// Aster Futures V3 REST base URL
        #[arg(long, default_value = "https://fapi.asterdex.com")]
        url: String,
        /// 输出文件名标签（默认 aster）
        #[arg(long)]
        tag: Option<String>,
    },

    /// Generate test data
    GenerateData {
        #[arg(short, long, default_value = "BTCUSDT")]
        symbol: String,
        #[arg(short, long, default_value = "30")]
        days: u32,
    },

    /// Observe BBO updates across every discovered market (never places orders)
    Scan {
        #[arg(long, default_value = "https://mainnet.zklighter.elliot.ai")]
        url: String,
        #[arg(long, default_value = "wss://mainnet.zklighter.elliot.ai/stream")]
        ws_url: String,
        #[arg(long, default_value = "30")]
        duration: u64,
        #[arg(long, default_value = "10")]
        top: usize,
        #[arg(long, default_value = "all")]
        market_type: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    env_profiles::load_shared_env()?;
    utils::logger::init_logger();

    let cli = Cli::parse();

    match cli.command {
        Commands::Live { config } => {
            let config = config.unwrap_or_else(default_live_config_path);
            run_live_trading(&config).await
        }
        Commands::Backtest {
            strategy,
            data,
            start,
            end,
            capital,
            output,
            params,
            config,
            maker,
        } => {
            run_backtest(
                &strategy,
                &data,
                &start,
                &end,
                capital,
                BacktestCliOpts {
                    output,
                    params,
                    config,
                    maker,
                },
            )
            .await
        }
        Commands::Optimize {
            strategy,
            data,
            start,
            end,
            capital,
            output,
            config,
        } => {
            run_optimize(
                &strategy,
                &data,
                &start,
                &end,
                capital,
                output.as_deref(),
                config.as_deref(),
            )
            .await
        }
        Commands::Dashboard { host, port } => run_dashboard(&host, port).await,
        Commands::Download {
            symbol,
            interval,
            start,
            end,
            url,
            tag,
        } => download_data(&symbol, &interval, &start, &end, &url, tag.as_deref()).await,
        Commands::DownloadArcus {
            symbols,
            interval,
            start,
            end,
            env,
            tag,
        } => download_arcus_data(&symbols, &interval, &start, &end, &env, tag.as_deref()).await,
        Commands::DownloadAster {
            symbols,
            interval,
            start,
            end,
            url,
            tag,
        } => download_aster_data(&symbols, &interval, &start, &end, &url, tag.as_deref()).await,
        Commands::GenerateData { symbol, days } => generate_test_data(&symbol, days).await,
        Commands::Scan {
            url,
            ws_url,
            duration,
            top,
            market_type,
        } => hft::run_market_scan(&url, &ws_url, duration, top, &market_type).await,
    }
}

fn default_live_config_path() -> String {
    env_profiles::selected_venue().config_path().to_string()
}

async fn run_live_trading(config_path: &str) -> Result<()> {
    info!("🚀 Starting Multi-Venue Quant Bot");

    // Load config
    let settings = Config::builder()
        .add_source(config::File::with_name(config_path))
        .add_source(config::Environment::with_prefix("LIGHTER"))
        .build()
        .context("Failed to load config")?;

    let exchange_kind = settings
        .get_string("exchange.kind")
        .context("exchange.kind is required for live trading")?;
    match aster_live::LiveDispatch::parse(&exchange_kind)? {
        aster_live::LiveDispatch::Lighter => {}
        aster_live::LiveDispatch::Arcus => return run_arcus_live_trading(settings).await,
        aster_live::LiveDispatch::Aster => {
            return aster_live::run_aster_live_trading(settings).await;
        }
        aster_live::LiveDispatch::Hyperliquid => {
            return hyperliquid_live::run_hyperliquid_live_trading(settings).await;
        }
    }

    let chain_id = settings.get_int("lighter.chain_id").unwrap_or(304);
    let credential_profile = env_profiles::profile_for_chain_id(chain_id)?;
    let (credentials, credential_path) = env_profiles::load_credentials(credential_profile)?;
    info!(
        "🔐 Using {} credential profile from {}",
        credential_profile.network_name(),
        credential_path.display()
    );

    // Load credentials from env
    let secret_key = credentials.secret_key;
    let account_index: i64 = credentials
        .account_index
        .parse()
        .context("Invalid LIGHTER_ACCOUNT_INDEX")?;
    let api_key_index: i32 = credentials
        .api_key_index
        .parse()
        .context("Invalid LIGHTER_API_KEY_INDEX")?;

    let rest_url = settings
        .get_string("lighter.rest_url")
        .unwrap_or_else(|_| "https://mainnet.zklighter.elliot.ai".to_string());
    let ws_url = settings
        .get_string("lighter.ws_url")
        .unwrap_or_else(|_| "wss://mainnet.zklighter.elliot.ai/stream".to_string());
    let chain_id = chain_id as i32;

    let max_open_orders = settings.get_int("trading.max_open_orders").unwrap_or(8) as u32;
    info!("⚙️ Max open orders: {}", max_open_orders);

    // Initialize FFI signer (uses API secret key, not L1 private key)
    info!("🔑 Initializing signer...");
    lighter::ffi::init(
        &rest_url,
        &secret_key,
        chain_id,
        api_key_index,
        account_index,
    )
    .context("Failed to initialize FFI signer")?;
    info!(
        "✅ Signer initialized (account={}, api_key_index={})",
        account_index, api_key_index
    );

    // Create REST client (shared between main loop and refresh task)
    let lighter_client = Arc::new(lighter::client::LighterClient::new_with_account(
        &rest_url,
        account_index,
        api_key_index,
    ));

    // Fetch initial nonce
    let nonce = lighter_client
        .refresh_nonce()
        .await
        .context("Failed to fetch nonce")?;
    info!("📋 Initial nonce: {}", nonce);

    // Fetch account info
    info!("📡 Fetching account info...");
    let account = lighter_client
        .get_account_info()
        .await
        .context("Failed to fetch account info")?;
    let equity = account.total_equity;
    let free_balance = account.balances.first().map(|b| b.free).unwrap_or(0.0);
    info!(
        "✅ Account connected — Equity: ${:.2}, Free: ${:.2}, Positions: {}",
        equity,
        free_balance,
        account.positions.len()
    );

    // Cancel all existing orders for a clean start
    info!("🧹 Cancelling all existing orders...");
    match lighter_client.cancel_all_orders("all").await {
        Ok(()) => info!("✅ All existing orders cancelled"),
        Err(e) => warn!("⚠️ Cancel all orders: {} (may have no open orders)", e),
    }
    // Refresh nonce after cancel-all (it consumed one)
    let _ = lighter_client.refresh_nonce().await;

    // Get market configuration
    let markets: Vec<i64> = settings
        .get("trading.markets")
        .unwrap_or_else(|_| vec![0, 1]);
    let market_ids: Vec<u32> = markets.iter().map(|m| *m as u32).collect();

    // Fetch full market list and register symbol map (supports Robinhood Chain instance
    // with stock perps/spot markets — no hardcoded ETH/BTC assumptions)
    let all_markets: Vec<lighter::types::MarketInfo> = match lighter_client.get_all_markets().await
    {
        Ok(ms) => {
            lighter::symbols::register_all(ms.iter().map(|m| (m.market_id, m.symbol.clone())));
            info!(
                "📚 Market registry: {} markets ({} perp)",
                ms.len(),
                ms.iter().filter(|m| m.market_type == "perp").count()
            );
            ms
        }
        Err(e) => {
            warn!(
                "⚠️ Failed to fetch full market list: {} — falling back to ETH/BTC defaults",
                e
            );
            Vec::new()
        }
    };
    lighter_client.set_active_markets(market_ids.clone());
    // symbol -> MarketInfo for dynamic decimals / min-amount lookups
    let market_registry: std::collections::HashMap<String, lighter::types::MarketInfo> =
        all_markets
            .iter()
            .map(|m| (m.symbol.clone(), m.clone()))
            .collect();

    // Fetch market info
    let mut market_infos = std::collections::HashMap::new();
    for &mid in &market_ids {
        match lighter_client.get_market_info(mid).await {
            Ok(mi) => {
                info!(
                    "📊 Market {}: {} (price_dec={}, size_dec={}, last=${:.2})",
                    mid, mi.symbol, mi.price_decimals, mi.size_decimals, mi.last_trade_price
                );
                market_infos.insert(mid, mi);
            }
            Err(e) => {
                warn!("⚠️ Failed to fetch market {} info: {}", mid, e);
            }
        }
    }

    // Shared open orders counter
    let open_orders_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Setup shared dashboard state
    let network_name = if chain_id == 466324 {
        "lighter-robinhood"
    } else {
        "lighter-mainnet"
    }
    .to_string();
    let dash_state = Arc::new(RwLock::new(dashboard::server::DashboardState {
        network_name: network_name.clone(),
        rest_url: rest_url.clone(),
        ws_url: ws_url.clone(),
        chain_id,
        equity,
        available_balance: free_balance,
        unrealized_pnl: account.positions.iter().map(|p| p.unrealized_pnl).sum(),
        strategy_name: String::new(),
        total_trades: 0,
        open_orders: 0,
        open_orders_list: Vec::new(),
        positions: account
            .positions
            .iter()
            .map(|p| {
                let mark = if p.size.abs() > 1e-12 {
                    match p.side {
                        lighter::types::Side::Buy => p.entry_price + p.unrealized_pnl / p.size,
                        lighter::types::Side::Sell => p.entry_price - p.unrealized_pnl / p.size,
                    }
                } else {
                    p.entry_price
                };
                serde_json::json!({
                    "symbol": p.symbol,
                    "side": format!("{:?}", p.side),
                    "size": p.size,
                    "entry_price": p.entry_price,
                    "mark_price": mark,
                    "unrealized_pnl": p.unrealized_pnl,
                })
            })
            .collect(),
        trade_history: Vec::new(),
        event_history: Vec::new(),
        risk_status: None,
        daily_realized_pnl: 0.0,
        total_realized_pnl: 0.0,
        daily_funding_pnl: 0.0,
        total_funding_pnl: 0.0,
        initial_equity: equity,
        peak_equity: equity,
        equity_history: vec![(Utc::now().timestamp(), equity)],
        pnl_history: vec![(Utc::now().timestamp(), 0.0)],
        total_volume: 0.0,
        total_closed_trades: 0,
        strategy_params: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "grid_count".to_string(),
                settings
                    .get_int("trading.strategies.grid_trading.grid_count")
                    .unwrap_or(10)
                    .to_string(),
            );
            m.insert(
                "investment_per_grid".to_string(),
                settings
                    .get_float("trading.strategies.grid_trading.investment_per_grid")
                    .unwrap_or(8.0)
                    .to_string(),
            );
            m.insert(
                "price_deviation".to_string(),
                settings
                    .get_float("trading.strategies.grid_trading.price_deviation")
                    .unwrap_or(0.012)
                    .to_string(),
            );
            m
        },
        strategy_config_changed: false,
        daily_pnl_map: std::collections::HashMap::new(),
        active_markets: market_ids.clone(),
        trading_paused: false,
        cancel_all_requested: false,
        available_markets: {
            let perps: Vec<(u32, String)> = all_markets
                .iter()
                .filter(|m| m.market_type == "perp")
                .map(|m| (m.market_id, m.symbol.clone()))
                .collect();
            if perps.is_empty() {
                vec![(0, "ETH".to_string()), (1, "BTC".to_string())]
            } else {
                perps
            }
        },
        risk_config: serde_json::json!({
            "max_drawdown_pct": 10.0,
            "daily_loss_limit_pct": 5.0,
            "max_leverage": 5.0,
            "position_stop_loss_pct": 3.0,
            "position_take_profit_pct": 5.0,
            "leverage_limit": 3.0,
        }),
        dashboard_auth_token: String::new(),
        risk_update_requested: None,
        leverage_limit: 3.0,
        last_prices: std::collections::HashMap::new(),
        shadow_metrics: None,
        hft_shadow_metrics: None,
        quant_agent: dashboard::quant_agent::AgentLedger::load(&network_name),
        user_add_rate_bps: None,
        user_cross_rate_bps: None,
        last_cross_dex_net_bps: None,
        last_cross_dex_side: None,
    }));

    // Restore persistent PnL data from disk
    if let Some(persisted) = dashboard::server::PersistentPnlData::load(&network_name) {
        let mut ds = dash_state.write().await;
        ds.restore_pnl(&persisted);
    }

    // Restore persistent strategy config from disk
    if let Some(saved) = dashboard::server::PersistentStrategyConfig::load(&network_name) {
        info!(
            "📂 Loaded strategy config: {} params={:?}",
            saved.strategy_name, saved.strategy_params
        );
        {
            let mut ds = dash_state.write().await;
            ds.strategy_name = saved.strategy_name;
            ds.strategy_params = saved.strategy_params;
        }
    }

    // Restore persistent risk config from disk
    if let Some(saved) = dashboard::server::PersistentRiskConfig::load(&network_name) {
        let mut ds = dash_state.write().await;
        info!(
            "📂 Loaded risk config: leverage_limit={}",
            saved.leverage_limit
        );
        ds.risk_config = saved.risk_config;
        ds.leverage_limit = saved.leverage_limit;
    }

    // Start dashboard server
    let dash_port = settings.get_int("dashboard.port").unwrap_or(4028) as u16;
    let dash_host = settings
        .get_string("dashboard.host")
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let dash_state_clone = dash_state.clone();
    tokio::spawn(async move {
        if let Err(e) =
            dashboard::server::start_with_state(&dash_host, dash_port, dash_state_clone).await
        {
            error!("Dashboard error: {}", e);
        }
    });
    info!("🌐 Dashboard started on port {}", dash_port);

    // Initialize risk manager (shared for periodic equity updates)
    let risk_manager = Arc::new(tokio::sync::Mutex::new(
        risk::risk_manager::RiskManager::new(&settings)
            .context("Failed to initialize risk manager")?,
    ));
    {
        let mut rm = risk_manager.lock().await;
        rm.update_equity(equity);
    }
    // Sync initial risk config from RiskManager to DashboardState
    // If we loaded persistent config, apply it to risk manager
    {
        let ds = dash_state.read().await;
        if ds.leverage_limit != 3.0 {
            // Persistent config was loaded — apply it to risk manager
            let mut rm = risk_manager.lock().await;
            let cfg = &ds.risk_config;
            rm.update_params(
                cfg.get("max_drawdown_pct").and_then(|v| v.as_f64()),
                cfg.get("daily_loss_limit_pct").and_then(|v| v.as_f64()),
                cfg.get("max_leverage").and_then(|v| v.as_f64()),
                cfg.get("position_stop_loss_pct").and_then(|v| v.as_f64()),
                cfg.get("position_take_profit_pct").and_then(|v| v.as_f64()),
            );
        } else {
            // No persistent config — sync risk manager defaults to dashboard
            let rm = risk_manager.lock().await;
            drop(ds);
            let mut ds = dash_state.write().await;
            ds.risk_config = rm.get_config();
            ds.risk_config["leverage_limit"] = serde_json::json!(ds.leverage_limit);
        }
    }

    // Initialize strategy (wrapped in Arc<RwLock> for runtime switching)
    // Use persisted params if available, otherwise fall back to settings.yaml
    let strategy: Arc<tokio::sync::RwLock<Box<dyn strategy::Strategy>>> = {
        let ds = dash_state.read().await;
        let has_saved_params = !ds.strategy_params.is_empty();
        if has_saved_params {
            let params_str = ds
                .strategy_params
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(",");
            info!(
                "📂 Creating strategy from saved config: {} params={}",
                ds.strategy_name, params_str
            );
            let strat = strategy::create_strategy_with_params(&ds.strategy_name, Some(&params_str))
                .unwrap_or_else(|e| {
                    warn!(
                        "Failed to create strategy from saved params: {}, falling back to defaults",
                        e
                    );
                    strategy::create_strategy(&settings).expect("Failed to create default strategy")
                });
            Arc::new(tokio::sync::RwLock::new(strat))
        } else {
            Arc::new(tokio::sync::RwLock::new(
                strategy::create_strategy(&settings).context("Failed to initialize strategy")?,
            ))
        }
    };
    let strategy_name = strategy.read().await.name().to_string();
    info!("📈 Strategy: {}", strategy_name);

    // Update dashboard with strategy name
    {
        let mut ds = dash_state.write().await;
        ds.strategy_name = strategy_name.clone();
    }

    // Create data store
    let data_store = Arc::new(RwLock::new(data::storage::MarketDataStore::new()));

    // Fetch initial candle data for strategies that need history
    for &mid in &market_ids {
        let symbol = market_infos
            .get(&mid)
            .map(|m| m.symbol.as_str())
            .unwrap_or("UNKNOWN");
        match lighter_client.get_candlesticks(mid, "1h", 100).await {
            Ok(candles) => {
                info!("📊 Loaded {} candles for {}", candles.len(), symbol);
                let mut store = data_store.write().await;
                for candle in candles {
                    store.add_candle(candle);
                }
            }
            Err(e) => {
                warn!("⚠️ Failed to fetch candles for {}: {}", symbol, e);
            }
        }
    }

    // Flag to pause order placement during auto-reset (prevents nonce race)
    let grid_resetting = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Connect WebSocket
    info!("🔌 Connecting WebSocket...");
    let ws_client = lighter::websocket::LighterWebSocket::new(&ws_url);
    ws_client.connect().await?;

    // Subscribe to market data for actively traded markets only
    let mut subscribed = std::collections::HashSet::new();
    for &mid in &market_ids {
        let symbol = market_infos
            .get(&mid)
            .map(|m| m.symbol.as_str())
            .unwrap_or("?");
        ws_client.subscribe_market_data(&mid.to_string()).await?;
        subscribed.insert(mid);
        info!("📡 Subscribed to {} (market {})", symbol, mid);
    }

    // Start the main trading loop
    let mut ws_receiver = ws_client.get_receiver();
    let data_store_clone = data_store.clone();

    // Periodic account refresh task — updates dashboard, risk manager
    // Also checks for emergency close and per-position stop-loss/take-profit
    // Share the client with the refresh task (same nonce counter)
    let client_for_refresh = lighter_client.clone();
    let dash_state_refresh = dash_state.clone();
    let risk_manager_refresh = risk_manager.clone();
    let open_orders_refresh = open_orders_count.clone();
    let market_infos_refresh = market_infos.clone();
    let strategy_refresh = strategy.clone();
    let grid_resetting_refresh = grid_resetting.clone();
    let stale_price_pct = 0.012_f64; // Cancel orders >1.2% from mid price
    let max_order_age_secs = 300_u64; // Force cancel-all after 5 minutes if stale
    let configured_market_ids = market_ids.clone();
    let registry_refresh = market_registry.clone();
    tokio::spawn(async move {
        // 动态市场元数据查询（回退到主网 ETH/BTC 默认值）
        let market_id_of = |sym: &str| -> Option<u32> {
            registry_refresh
                .get(sym)
                .map(|m| m.market_id)
                .or_else(|| lighter::symbols::market_id_of(sym))
        };
        let size_decimals_of = |sym: &str| -> i32 {
            registry_refresh
                .get(sym)
                .map(|m| m.size_decimals as i32)
                .unwrap_or(if sym == "BTC" { 5 } else { 4 })
        };
        let min_change_of = |sym: &str| -> f64 {
            registry_refresh
                .get(sym)
                .map(|m| m.min_base_amount * 0.95)
                .unwrap_or(if sym == "BTC" { 0.00019 } else { 0.0049 })
        };
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        let mut at_max_since: Option<std::time::Instant> = None;
        // Equity-based realized PnL tracking
        let mut prev_equity: f64 = 0.0;
        let mut prev_unrealized: f64 = 0.0;
        // Position snapshot for logging close events (side, size, entry_price)
        let mut prev_positions: std::collections::HashMap<
            String,
            (lighter::types::Side, f64, f64),
        > = std::collections::HashMap::new();
        let mut position_opened_at: std::collections::HashMap<String, chrono::DateTime<Utc>> =
            std::collections::HashMap::new();
        // Track daily PnL reset
        let mut last_daily_reset_day: u32 = (Utc::now().timestamp() / 86400) as u32;
        let mut first_cycle = true;
        loop {
            interval.tick().await;

            // Always sync real open orders count first (fast, lightweight)
            match client_for_refresh.get_open_orders("all").await {
                Ok(orders) => {
                    let count = orders.len() as u32;
                    let prev =
                        open_orders_refresh.swap(count, std::sync::atomic::Ordering::Relaxed);
                    if prev != count {
                        info!("📋 Open orders synced: {} → {} (real)", prev, count);
                    }
                    let mut ds = dash_state_refresh.write().await;
                    ds.open_orders = count;
                    // Also store the actual orders for dashboard display
                    ds.open_orders_list = orders
                        .iter()
                        .map(|o| {
                            serde_json::json!({
                                "id": o.id,
                                "symbol": o.symbol,
                                "side": format!("{:?}", o.side),
                                "price": o.price,
                                "quantity": o.quantity,
                                "filled_quantity": o.filled_quantity,
                                "status": format!("{:?}", o.status),
                            })
                        })
                        .collect();
                    drop(ds);

                    // ===== Stale order management =====
                    // SKIP during emergency — don't cancel close orders
                    let is_emergency = {
                        let rm = risk_manager_refresh.lock().await;
                        rm.is_emergency_triggered()
                    };
                    if count > 0 && !is_emergency {
                        // Get fresh market prices
                        let mut mid_prices: std::collections::HashMap<u32, f64> =
                            std::collections::HashMap::new();
                        for &mid in &configured_market_ids {
                            if let Ok(mi) = client_for_refresh.get_market_info(mid).await {
                                if mi.last_trade_price > 0.0 {
                                    mid_prices.insert(mid, mi.last_trade_price);
                                }
                            }
                        }

                        // Signed net position per market (long = +, short = -) from last
                        // account refresh. Used to spare position-reducing orders from the
                        // stale-cancel sweeps below — otherwise the very orders that would
                        // flatten an accumulated bag get wiped before they can fill.
                        let net_pos_by_market: std::collections::HashMap<u32, f64> = {
                            let ds = dash_state_refresh.read().await;
                            let mut m = std::collections::HashMap::new();
                            for p in &ds.positions {
                                if let (Some(sym), Some(size), Some(side)) =
                                    (p["symbol"].as_str(), p["size"].as_f64(), p["side"].as_str())
                                {
                                    if let Some(mid) = market_id_of(sym) {
                                        let signed = if side == "Sell" {
                                            -size.abs()
                                        } else {
                                            size.abs()
                                        };
                                        *m.entry(mid).or_insert(0.0) += signed;
                                    }
                                }
                            }
                            m
                        };
                        // An order reduces the position if it trades against the net side.
                        let reduces_position =
                            |market_id: u32, side: &lighter::types::Side| -> bool {
                                match net_pos_by_market.get(&market_id).copied().unwrap_or(0.0) {
                                    n if n > 1e-9 => matches!(side, lighter::types::Side::Sell),
                                    n if n < -1e-9 => matches!(side, lighter::types::Side::Buy),
                                    _ => false,
                                }
                            };

                        // 诊断：打印每个挂单相对市价的距离（R = 减仓单）。
                        // 2026-07-24 的 15.5h 停摆就是靠这类信息才能判定挂单是"太远不成交"还是"够近但被占位"。
                        {
                            let summary: Vec<String> = orders
                                .iter()
                                .filter_map(|o| {
                                    let mid = *mid_prices.get(&market_id_of(&o.symbol)?)?;
                                    let tag = if reduces_position(market_id_of(&o.symbol)?, &o.side)
                                    {
                                        "R"
                                    } else {
                                        ""
                                    };
                                    Some(format!(
                                        "{:?}{}@{:.1}({:+.2}%)",
                                        o.side,
                                        tag,
                                        o.price,
                                        (o.price - mid) / mid * 100.0
                                    ))
                                })
                                .collect();
                            if !summary.is_empty() {
                                info!("📋 Orders vs mid: {}", summary.join(" "));
                            }
                        }

                        // 逃生阀（修复 2026-07-24 死锁）：减仓单可以豁免撤单，但**绝不允许占满所有槽位**。
                        // 否则「持仓封顶 → 只出单一方向信号 → 全部挂单都是减仓单 → 全部豁免 → 槽位占死
                        // → 新信号被 max_open_orders 拒绝」构成无出口的闭环，机器人完全停摆。
                        // 只保护最接近市价（最可能成交）的 N 个减仓单，至少留 2 个槽位给新信号。
                        let reducing_keep_limit = max_open_orders.saturating_sub(2) as usize;
                        let protected_reducing: std::collections::HashSet<String> = {
                            let mut cand: Vec<(&str, f64)> = orders
                                .iter()
                                .filter_map(|o| {
                                    let mid_id = market_id_of(&o.symbol)?;
                                    if !reduces_position(mid_id, &o.side) {
                                        return None;
                                    }
                                    let mid = *mid_prices.get(&mid_id)?;
                                    Some((o.id.as_str(), (o.price - mid).abs() / mid))
                                })
                                .collect();
                            cand.sort_by(|a, b| {
                                a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            cand.truncate(reducing_keep_limit);
                            cand.into_iter().map(|(id, _)| id.to_string()).collect()
                        };

                        // Strategy 1: Cancel individual orders that are too far from mid price
                        let mut cancelled = 0u32;
                        for order in &orders {
                            let market_id = match market_id_of(&order.symbol) {
                                Some(id) => id,
                                None => continue,
                            };
                            // 减仓单原则上保留（让它成交去平仓），但仅限受逃生阀保护的那几个。
                            // 未受保护的（离市价最远的）即使是减仓单也照常按 >1.2% 撤掉：
                            // 它短期内不会成交，占着槽位纯属死重；撤掉后策略会在更贴近市价的
                            // 网格层重新挂减仓单，反而**加快**平仓。
                            if reduces_position(market_id, &order.side)
                                && protected_reducing.contains(&order.id)
                            {
                                continue;
                            }
                            if let Some(&mid) = mid_prices.get(&market_id) {
                                let diff = (order.price - mid).abs() / mid;
                                if diff > stale_price_pct {
                                    if let Ok(idx) = order.id.parse::<i64>() {
                                        info!("🗑️ Cancelling stale order: {} {:?} @ {:.2} (mid={:.2}, diff={:.1}%)",
                                            order.symbol, order.side, order.price, mid, diff * 100.0);
                                        match client_for_refresh
                                            .cancel_order_by_index(market_id, idx)
                                            .await
                                        {
                                            Ok(()) => cancelled += 1,
                                            Err(e) => warn!(
                                                "Failed to cancel stale order {}: {}",
                                                order.id, e
                                            ),
                                        }
                                    }
                                }
                            }
                        }

                        if cancelled > 0 {
                            info!(
                                "🗑️ Cancelled {} stale orders, resetting grid state",
                                cancelled
                            );
                            strategy_refresh.read().await.clear_filled_state();
                            let _ = client_for_refresh.refresh_nonce().await;
                            let new_count = count.saturating_sub(cancelled);
                            open_orders_refresh
                                .store(new_count, std::sync::atomic::Ordering::Relaxed);
                            at_max_since = None; // Reset timer
                        }

                        // Strategy 2: Time-based cancel-all if orders sit unfilled too long
                        // This prevents the bot from being stuck when orders are within
                        // the grid range but none are filling
                        if count >= 3 {
                            // Any meaningful number of open orders
                            if at_max_since.is_none() {
                                at_max_since = Some(std::time::Instant::now());
                            }
                            if let Some(since) = at_max_since {
                                let elapsed = since.elapsed().as_secs();
                                if elapsed >= max_order_age_secs {
                                    // Cancel only the position-INCREASING orders; keep any
                                    // reducing orders alive so the accumulated position can
                                    // still be closed when price returns to those levels.
                                    grid_resetting_refresh
                                        .store(true, std::sync::atomic::Ordering::Relaxed);
                                    let mut auto_cancelled = 0u32;
                                    let mut kept_reducing = 0u32;
                                    for order in &orders {
                                        let market_id = match market_id_of(&order.symbol) {
                                            Some(id) => id,
                                            None => continue,
                                        };
                                        // 同逃生阀：只保护最接近市价的若干减仓单，其余照撤，
                                        // 保证永远有空槽位接纳新信号。
                                        if reduces_position(market_id, &order.side)
                                            && protected_reducing.contains(&order.id)
                                        {
                                            kept_reducing += 1;
                                            continue;
                                        }
                                        if let Ok(idx) = order.id.parse::<i64>() {
                                            match client_for_refresh
                                                .cancel_order_by_index(market_id, idx)
                                                .await
                                            {
                                                Ok(()) => auto_cancelled += 1,
                                                Err(e) => warn!(
                                                    "❌ Auto-reset cancel failed for {}: {}",
                                                    order.id, e
                                                ),
                                            }
                                        }
                                    }
                                    info!("🔄 Auto-reset: cancelled {} stale orders, kept {} reducing (stale for {}s)",
                                        auto_cancelled, kept_reducing, elapsed);
                                    if auto_cancelled > 0 {
                                        strategy_refresh.read().await.clear_filled_state();
                                        let _ = client_for_refresh.refresh_nonce().await;
                                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                    }
                                    let remaining = count.saturating_sub(auto_cancelled);
                                    open_orders_refresh
                                        .store(remaining, std::sync::atomic::Ordering::Relaxed);
                                    at_max_since = None;
                                    grid_resetting_refresh
                                        .store(false, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                        } else {
                            at_max_since = None; // Below threshold, reset timer
                        }
                    } else {
                        at_max_since = None;
                    }
                }
                Err(e) => {
                    warn!("⚠️ Open orders sync error: {}", e);
                }
            }

            // Refresh account info (every cycle)
            match client_for_refresh.get_account_info().await {
                Ok(acct) => {
                    let curr_equity = acct.total_equity;
                    let curr_unrealized: f64 =
                        acct.positions.iter().map(|p| p.unrealized_pnl).sum();

                    // ===== Auto-close positions on non-configured markets =====
                    for pos in &acct.positions {
                        if pos.size.abs() < 1e-10 {
                            continue;
                        }
                        let pos_market_id = match market_id_of(&pos.symbol) {
                            Some(id) => id,
                            None => {
                                warn!("⚠️ Unknown market for position symbol {}, skipping auto-close check", pos.symbol);
                                continue;
                            }
                        };
                        if !configured_market_ids.contains(&pos_market_id) {
                            warn!("⚠️ Found position on non-configured market {}: {} {:?} {:.6} — closing",
                                pos.symbol, pos.symbol, pos.side, pos.size);
                            let close_side = match pos.side {
                                lighter::types::Side::Buy => lighter::types::Side::Sell,
                                lighter::types::Side::Sell => lighter::types::Side::Buy,
                            };
                            let mi = market_infos_refresh.values().find(|m| {
                                pos.symbol.contains(&m.symbol) || m.symbol.contains(&pos.symbol)
                            });
                            // Get fresh price for this market
                            let fresh_price =
                                match client_for_refresh.get_market_info(pos_market_id).await {
                                    Ok(fmi) if fmi.last_trade_price > 0.0 => fmi.last_trade_price,
                                    _ => pos.entry_price,
                                };
                            let slippage = 0.005;
                            let close_price = match close_side {
                                lighter::types::Side::Buy => fresh_price * (1.0 + slippage),
                                lighter::types::Side::Sell => fresh_price * (1.0 - slippage),
                            };
                            warn!(
                                "🔄 Auto-closing {} {:?} {:.6} @ {:.2} (non-configured market)",
                                pos.symbol, close_side, pos.size, close_price
                            );
                            match client_for_refresh
                                .place_order_with_market(
                                    pos_market_id,
                                    close_side,
                                    close_price,
                                    pos.size.abs(),
                                    mi,
                                    true,
                                )
                                .await
                            {
                                Ok(resp) => info!(
                                    "✅ Auto-close order placed: {} id={}",
                                    pos.symbol, resp.order_id
                                ),
                                Err(e) => error!("❌ Auto-close failed for {}: {}", pos.symbol, e),
                            }
                        }
                    }

                    // ===== Realized PnL detection =====
                    // Step 1: Detect actual position changes (size or side changed)
                    // Step 2: Only then compute PnL via equity method: realized = Δequity - Δunrealized
                    let mut realized_pnl_this_cycle = 0.0_f64;
                    let mut close_events: Vec<serde_json::Value> = Vec::new();
                    let close_timestamp = Utc::now();
                    let mut curr_pos_map: std::collections::HashMap<
                        String,
                        (lighter::types::Side, f64, f64),
                    > = std::collections::HashMap::new();
                    for p in &acct.positions {
                        if p.size.abs() > 1e-10 {
                            let factor = 10_f64.powi(size_decimals_of(&p.symbol));
                            let rounded_size = (p.size * factor).round() / factor;
                            curr_pos_map
                                .insert(p.symbol.clone(), (p.side, rounded_size, p.entry_price));
                        }
                    }

                    if !first_cycle && prev_equity > 0.0 {
                        // Check for meaningful position changes (size decreased or position closed)
                        let mut position_reductions: Vec<(
                            String,
                            lighter::types::Side,
                            f64,
                            f64,
                            &str,
                        )> = Vec::new();
                        for (symbol, (prev_side, prev_size, prev_entry)) in &prev_positions {
                            // Min change threshold per market
                            let min_change = min_change_of(symbol);
                            match curr_pos_map.get(symbol) {
                                Some((curr_side, curr_size, _)) => {
                                    if *curr_side != *prev_side {
                                        // Side flipped — full close of old position
                                        position_reductions.push((
                                            symbol.clone(),
                                            *prev_side,
                                            *prev_size,
                                            *prev_entry,
                                            "Full Close",
                                        ));
                                    } else if *prev_size - *curr_size >= min_change {
                                        // Position reduced
                                        let closed = prev_size - curr_size;
                                        let close_type = if *curr_size < min_change {
                                            "Full Close"
                                        } else {
                                            "Partial Close"
                                        };
                                        position_reductions.push((
                                            symbol.clone(),
                                            *prev_side,
                                            closed,
                                            *prev_entry,
                                            close_type,
                                        ));
                                    }
                                }
                                None => {
                                    // Position gone entirely
                                    if *prev_size >= 1e-10 {
                                        position_reductions.push((
                                            symbol.clone(),
                                            *prev_side,
                                            *prev_size,
                                            *prev_entry,
                                            "Full Close",
                                        ));
                                    }
                                }
                            }
                        }

                        // Only compute PnL if an actual position change occurred
                        if !position_reductions.is_empty() {
                            let equity_change = curr_equity - prev_equity;
                            let unrealized_change = curr_unrealized - prev_unrealized;
                            realized_pnl_this_cycle = equity_change - unrealized_change;

                            // Distribute PnL across changed positions proportionally
                            let total_notional: f64 = position_reductions
                                .iter()
                                .map(|(_, _, size, entry, _)| size * entry)
                                .sum();

                            for (symbol, prev_side, closed_size, prev_entry, close_type) in
                                &position_reductions
                            {
                                let pnl_share =
                                    if total_notional > 0.0 && position_reductions.len() > 1 {
                                        realized_pnl_this_cycle * (closed_size * prev_entry)
                                            / total_notional
                                    } else {
                                        realized_pnl_this_cycle
                                    };
                                let market_id = market_id_of(symbol).unwrap_or(0);

                                info!(
                                    "💰 {} {}: {:?} {:.6} @ entry={:.2} | PnL: {}{:.4}",
                                    close_type,
                                    symbol,
                                    prev_side,
                                    closed_size,
                                    prev_entry,
                                    if pnl_share >= 0.0 { "+" } else { "" },
                                    pnl_share
                                );

                                let duration_secs = position_opened_at
                                    .get(symbol)
                                    .map(|opened| {
                                        close_timestamp
                                            .signed_duration_since(*opened)
                                            .num_seconds()
                                            .max(0)
                                    })
                                    .unwrap_or(0);

                                close_events.push(serde_json::json!({
                                    "timestamp": close_timestamp.to_rfc3339(),
                                    "symbol": symbol,
                                    "market_id": market_id,
                                    "side": format!("{:?}", match prev_side {
                                        lighter::types::Side::Buy => lighter::types::Side::Sell,
                                        lighter::types::Side::Sell => lighter::types::Side::Buy,
                                    }),
                                    "price": prev_entry,
                                    "quantity": closed_size,
                                    "pnl": (pnl_share * 10000.0).round() / 10000.0,
                                    "action": close_type, // "Full Close" or "Partial Close"
                                    "duration_secs": duration_secs,
                                }));
                            }
                        }
                    }
                    first_cycle = false;
                    prev_equity = curr_equity;
                    prev_unrealized = curr_unrealized;

                    let mut next_opened_at = position_opened_at.clone();
                    next_opened_at.retain(|symbol, _| curr_pos_map.contains_key(symbol));
                    for (symbol, (curr_side, curr_size, _)) in &curr_pos_map {
                        let min_change = min_change_of(symbol);
                        let should_reset = match prev_positions.get(symbol) {
                            Some((prev_side, prev_size, _)) => {
                                *prev_side != *curr_side || *prev_size < min_change
                            }
                            None => *curr_size >= min_change,
                        };
                        if should_reset || !next_opened_at.contains_key(symbol) {
                            next_opened_at.insert(symbol.clone(), close_timestamp);
                        }
                    }
                    position_opened_at = next_opened_at;

                    // Update position snapshot (rounded) for next cycle
                    prev_positions.clear();
                    for p in &acct.positions {
                        if p.size.abs() > 1e-10 {
                            let factor = 10_f64.powi(size_decimals_of(&p.symbol));
                            let rounded_size = (p.size * factor).round() / factor;
                            prev_positions
                                .insert(p.symbol.clone(), (p.side, rounded_size, p.entry_price));
                        }
                    }

                    // Update dashboard
                    {
                        let mut ds = dash_state_refresh.write().await;
                        ds.equity = curr_equity;
                        ds.available_balance = acct.balances.first().map(|b| b.free).unwrap_or(0.0);
                        ds.unrealized_pnl = curr_unrealized;
                        ds.positions = acct.positions.iter().map(|p| {
                            // Calculate mark price from unrealized PnL
                            let mark = if p.size.abs() > 1e-12 {
                                match p.side {
                                    lighter::types::Side::Buy => p.entry_price + p.unrealized_pnl / p.size,
                                    lighter::types::Side::Sell => p.entry_price - p.unrealized_pnl / p.size,
                                }
                            } else { p.entry_price };
                            serde_json::json!({
                                "symbol": p.symbol,
                                "side": format!("{:?}", p.side),
                                "size": p.size,
                                "entry_price": p.entry_price,
                                "mark_price": mark,
                                "unrealized_pnl": p.unrealized_pnl,
                                "opened_at": position_opened_at.get(&p.symbol).map(|ts| ts.to_rfc3339()),
                            })
                        }).collect();

                        // ===== Update realized PnL =====
                        if realized_pnl_this_cycle.abs() > 0.0001 {
                            // Daily reset check
                            let today = (Utc::now().timestamp() / 86400) as u32;
                            if today > last_daily_reset_day {
                                // Save yesterday's daily PnL to map before reset
                                let yesterday = chrono::Utc::now()
                                    .checked_sub_signed(chrono::Duration::days(1))
                                    .map(|d| d.format("%Y-%m-%d").to_string())
                                    .unwrap_or_default();
                                if !yesterday.is_empty() && ds.daily_realized_pnl.abs() > 0.0001 {
                                    let yesterday_pnl = ds.daily_realized_pnl;
                                    ds.daily_pnl_map.insert(yesterday, yesterday_pnl);
                                }
                                info!(
                                    "📅 New day — resetting daily realized PnL ({:.4} → 0.0)",
                                    ds.daily_realized_pnl
                                );
                                ds.daily_realized_pnl = 0.0;
                                last_daily_reset_day = today;
                            }
                            ds.daily_realized_pnl += realized_pnl_this_cycle;
                            ds.total_realized_pnl += realized_pnl_this_cycle;
                            info!(
                                "📊 Realized PnL update: cycle={:+.4}, daily={:+.4}, total={:+.4}",
                                realized_pnl_this_cycle,
                                ds.daily_realized_pnl,
                                ds.total_realized_pnl
                            );
                        }

                        // Record close events in trade history (lifetime volume/close
                        // counters + shared ring-buffer limit live in push_trade).
                        let has_close_events = !close_events.is_empty();
                        for evt in close_events {
                            ds.push_trade(evt);
                        }

                        if realized_pnl_this_cycle.abs() > 0.0001 || has_close_events {
                            ds.save_pnl();
                        }

                        // Track initial equity on first update
                        if ds.initial_equity == 0.0 {
                            ds.initial_equity = curr_equity;
                        }
                        // Track peak equity
                        if curr_equity > ds.peak_equity {
                            ds.peak_equity = curr_equity;
                        }
                        // Record equity history (max 1440 points = 24h at 1/min)
                        let now_ts = Utc::now().timestamp();
                        let should_record = ds
                            .equity_history
                            .last()
                            .map(|(ts, _)| now_ts - ts >= 60)
                            .unwrap_or(true);
                        if should_record {
                            ds.equity_history.push((now_ts, curr_equity));
                            let cum_pnl = curr_equity - ds.initial_equity;
                            ds.pnl_history.push((now_ts, cum_pnl));
                            // Keep up to 10080 points (~7 days at 1/min)
                            if ds.equity_history.len() > 10080 {
                                ds.equity_history.remove(0);
                                ds.pnl_history.remove(0);
                            }
                            // Periodic save: every 5 minutes
                            let should_periodic_save = ds.equity_history.len() % 5 == 0;
                            if should_periodic_save {
                                ds.save_pnl();
                            }
                        }
                    }

                    // Update risk manager equity
                    let daily_pnl: f64 = acct.positions.iter().map(|p| p.unrealized_pnl).sum();
                    let is_emergency = {
                        let mut rm = risk_manager_refresh.lock().await;
                        rm.update_equity(acct.total_equity);
                        rm.update_daily_pnl(daily_pnl);

                        // Check if emergency close should trigger
                        if rm.should_emergency_close() && !rm.is_emergency_triggered() {
                            warn!("🚨 紧急平仓触发! 取消所有订单并平仓...");
                            rm.set_emergency_triggered();
                        }
                        rm.is_emergency_triggered()
                    }; // rm lock released here

                    // ===== Emergency close: keep retrying until flat =====
                    if is_emergency {
                        let has_positions = acct.positions.iter().any(|p| p.size.abs() > 1e-10);
                        if has_positions {
                            // Cancel all orders to free margin
                            let _ = client_for_refresh.cancel_all_orders("all").await;
                            let _ = client_for_refresh.refresh_nonce().await;

                            // Get fresh market prices for aggressive close
                            let mut fresh_prices: std::collections::HashMap<u32, f64> =
                                std::collections::HashMap::new();
                            // Fetch prices for configured markets plus any market with an open position
                            let mut close_mids: std::collections::HashSet<u32> =
                                configured_market_ids.iter().copied().collect();
                            for pos in &acct.positions {
                                if pos.size.abs() < 1e-10 {
                                    continue;
                                }
                                if let Some(id) = market_id_of(&pos.symbol) {
                                    close_mids.insert(id);
                                }
                            }
                            for &mid in &close_mids {
                                if let Ok(fmi) = client_for_refresh.get_market_info(mid).await {
                                    if fmi.last_trade_price > 0.0 {
                                        fresh_prices.insert(mid, fmi.last_trade_price);
                                    }
                                }
                            }

                            // Close all positions at CURRENT market price with slippage
                            for pos in &acct.positions {
                                if pos.size.abs() < 1e-10 {
                                    continue;
                                }
                                let close_side = match pos.side {
                                    lighter::types::Side::Buy => lighter::types::Side::Sell,
                                    lighter::types::Side::Sell => lighter::types::Side::Buy,
                                };
                                let mi = market_infos_refresh.values().find(|m| {
                                    pos.symbol.contains(&m.symbol)
                                        || m.symbol.contains(&pos.symbol.replace("market_", ""))
                                });
                                let market_id = mi
                                    .map(|m| m.market_id)
                                    .or_else(|| market_id_of(&pos.symbol))
                                    .unwrap_or(0);

                                // Use CURRENT market price + slippage (not entry price!)
                                let current_mid = fresh_prices
                                    .get(&market_id)
                                    .copied()
                                    .unwrap_or(pos.entry_price);
                                let slippage = 0.005; // 0.5% slippage for aggressive fill
                                let close_price = match close_side {
                                    lighter::types::Side::Buy => current_mid * (1.0 + slippage),
                                    lighter::types::Side::Sell => current_mid * (1.0 - slippage),
                                };

                                warn!(
                                    "🚨 紧急平仓: {} {:?} {:.6} @ {:.2} (mid={:.2}, entry={:.2})",
                                    pos.symbol,
                                    close_side,
                                    pos.size,
                                    close_price,
                                    current_mid,
                                    pos.entry_price
                                );
                                match client_for_refresh
                                    .place_order_with_market(
                                        market_id,
                                        close_side,
                                        close_price,
                                        pos.size.abs(),
                                        mi,
                                        true,
                                    )
                                    .await
                                {
                                    Ok(resp) => info!("✅ 紧急平仓订单: id={}", resp.order_id),
                                    Err(e) => error!("❌ 紧急平仓订单失败: {}", e),
                                }
                            }
                            continue; // skip normal processing this cycle
                        } else {
                            info!("✅ 紧急平仓完成 — 所有持仓已关闭");
                        }
                    }

                    // ===== Per-position stop-loss / take-profit check =====
                    if !is_emergency {
                        // Build current prices from last known market data
                        let mut current_prices = std::collections::HashMap::new();
                        for mi in market_infos_refresh.values() {
                            if mi.last_trade_price > 0.0 {
                                current_prices.insert(mi.symbol.clone(), mi.last_trade_price);
                                current_prices.insert(
                                    format!("market_{}", mi.market_id),
                                    mi.last_trade_price,
                                );
                            }
                        }

                        for (&mid, mi) in &market_infos_refresh {
                            if let Ok(fresh_mi) = client_for_refresh.get_market_info(mid).await {
                                current_prices
                                    .insert(fresh_mi.symbol.clone(), fresh_mi.last_trade_price);
                                current_prices
                                    .insert(format!("market_{}", mid), fresh_mi.last_trade_price);
                            } else {
                                current_prices.insert(mi.symbol.clone(), mi.last_trade_price);
                                current_prices
                                    .insert(format!("market_{}", mid), mi.last_trade_price);
                            }
                        }

                        let close_signals = {
                            let rm = risk_manager_refresh.lock().await;
                            rm.check_position_stop_loss_take_profit(
                                &acct.positions,
                                &current_prices,
                            )
                        }; // rm released here

                        for sig in close_signals {
                            let mi = market_infos_refresh.values().find(|m| {
                                sig.symbol.contains(&m.symbol)
                                    || m.symbol.contains(&sig.symbol.replace("market_", ""))
                            });
                            let market_id = mi
                                .map(|m| m.market_id)
                                .or_else(|| market_id_of(&sig.symbol))
                                .unwrap_or(0);

                            info!(
                                "📌 {} — {} {:?} {:.6} @ {:.2} (entry={:.2})",
                                sig.reason,
                                sig.symbol,
                                sig.side_to_close,
                                sig.size,
                                sig.current_price,
                                sig.entry_price
                            );

                            // Cancel all orders first to free up margin
                            let _ = client_for_refresh.cancel_all_orders("all").await;
                            let _ = client_for_refresh.refresh_nonce().await;

                            match client_for_refresh
                                .place_order_with_market(
                                    market_id,
                                    sig.side_to_close,
                                    sig.current_price,
                                    sig.size,
                                    mi,
                                    true,
                                )
                                .await
                            {
                                Ok(resp) => {
                                    info!("✅ 止损止盈订单: id={} — {}", resp.order_id, sig.reason)
                                }
                                Err(e) => error!("❌ 止损止盈订单失败: {} — {}", e, sig.reason),
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Account refresh error: {}", e);
                }
            }
        }
    });

    info!("🎯 Trading system ready. Waiting for market data...");

    // Main event loop
    let mut trade_count: u64 = 0;
    let mut last_risk_update = std::time::Instant::now();
    while let Ok(msg) = ws_receiver.recv().await {
        // Update data store
        {
            let mut store = data_store_clone.write().await;
            match &msg {
                lighter::types::WsMessage::OrderBookUpdate(ob) => {
                    store.update_order_book(ob.clone());
                }
                lighter::types::WsMessage::TradeUpdate(trade) => {
                    store.add_trade(trade.clone());
                }
                _ => {}
            }
        }

        // Run strategy
        let mut snapshot = data_store_clone.read().await.get_snapshot();

        // 把盘口中间价写回 DashboardState。这是面板上唯一一个「空仓时也还在动」的数：
        // positions 里的 mark_price 只有持仓才有，而这个 bot 大部分时间是空仓的。
        //
        // 两点注意：
        // 1) 用 `ob.symbol` 而不是 map 的 key。二者当前相同（storage.rs 就是按
        //    order_book.symbol 插入的），但显式取字段可以免疫 key 格式漂移 ——
        //    键对不上时面板只会一直显示「暂无行情」，不会报错，很难查。
        // 2) 这里在 WS 驱动的主循环热路径上，而 DashboardState 是唯一的跨任务控制面。
        //    所以先只读比较、值真的变了才拿写锁；面板 3s 才消费一次，不会漏。
        {
            let mut prices = std::collections::HashMap::new();
            for ob in snapshot.order_books.values() {
                // 增量盘口更新的某一瞬间，买盘顶部可能还没补上，best_bid 会掉到很深的一档，
                // 中间价随之被拉歪（实测出现过 (63193+60040)/2 = 61616，偏离 2.5%）。
                // 价差超过 0.5% 就认为这一帧的盘口不完整，跳过不更新，保留上一个值。
                let (Some(bid), Some(ask), Some(mid)) =
                    (ob.best_bid(), ob.best_ask(), ob.mid_price())
                else {
                    continue;
                };
                if mid > 0.0 && bid > 0.0 && ask > bid && (ask - bid) / mid < 0.005 {
                    prices.insert(ob.symbol.clone(), mid);
                }
            }
            if !prices.is_empty() {
                let changed = {
                    let ds = dash_state.read().await;
                    ds.last_prices != prices
                };
                if changed {
                    // 整体赋值而非 extend：不再挂盘的市场会自动被剔除，不会无限累积
                    dash_state.write().await.last_prices = prices;
                }
            }
        }

        // Inject real exchange positions (signed: long = +, short = -) so the strategy
        // can cap one-sided accumulation against the actual position, not just its own
        // reset-prone internal state.
        {
            let ds = dash_state.read().await;
            snapshot.positions_authoritative = true;
            for p in &ds.positions {
                let (Some(symbol), Some(size), Some(side)) =
                    (p["symbol"].as_str(), p["size"].as_f64(), p["side"].as_str())
                else {
                    continue;
                };
                let signed = if side == "Sell" {
                    -size.abs()
                } else {
                    size.abs()
                };
                snapshot.positions.insert(symbol.to_string(), signed);
                if let Some(entry_price) = p["entry_price"].as_f64().filter(|price| *price > 0.0) {
                    snapshot
                        .position_entry_prices
                        .insert(symbol.to_string(), entry_price);
                }
            }
        }

        // Block all trading when emergency is active
        {
            let rm = risk_manager.lock().await;
            let is_emergency = rm.is_emergency_triggered();
            drop(rm);
            if is_emergency {
                if last_risk_update.elapsed().as_secs() >= 5 {
                    warn!("🚨 紧急模式 — 停止所有交易信号处理");
                    last_risk_update = std::time::Instant::now();
                }
                continue;
            }
        }

        // Check dashboard trading controls
        let (is_paused, active_markets, should_cancel_all) = {
            let ds = dash_state.read().await;
            (
                ds.trading_paused,
                ds.active_markets.clone(),
                ds.cancel_all_requested,
            )
        };

        // Handle cancel-all request from dashboard
        if should_cancel_all {
            info!("🗑️ Executing cancel-all from dashboard...");
            match lighter_client.cancel_all_orders("all").await {
                Ok(_) => info!("✅ All orders cancelled via dashboard"),
                Err(e) => warn!("⚠️ Cancel-all failed: {}", e),
            }
            let mut ds = dash_state.write().await;
            ds.cancel_all_requested = false;
        }

        // Check for risk config updates from dashboard
        {
            let mut ds = dash_state.write().await;
            if let Some(update) = ds.risk_update_requested.take() {
                let mut rm = risk_manager.lock().await;
                rm.update_params(
                    update.get("max_drawdown_pct").and_then(|v| v.as_f64()),
                    update.get("daily_loss_limit_pct").and_then(|v| v.as_f64()),
                    update.get("max_leverage").and_then(|v| v.as_f64()),
                    update
                        .get("position_stop_loss_pct")
                        .and_then(|v| v.as_f64()),
                    update
                        .get("position_take_profit_pct")
                        .and_then(|v| v.as_f64()),
                );
            }
        }

        // Block all trading when paused from dashboard
        if is_paused {
            continue;
        }

        // Check total position exposure — block same-direction signals if overleveraged
        let position_exposure: f64 = {
            let ds = dash_state.read().await;
            ds.positions
                .iter()
                .map(|p| {
                    let size = p["size"].as_f64().unwrap_or(0.0).abs();
                    let entry = p["entry_price"].as_f64().unwrap_or(0.0);
                    size * entry
                })
                .sum()
        };
        let equity = {
            let ds = dash_state.read().await;
            ds.equity
        };
        let current_leverage = if equity > 0.0 {
            position_exposure / equity
        } else {
            0.0
        };

        match strategy.read().await.evaluate(&snapshot).await {
            Ok(Some(signals)) => {
                for mut signal in signals {
                    // Check if market is active (dashboard trading controls)
                    if !active_markets.contains(&signal.market_id) {
                        continue;
                    }

                    // Check max open orders limit
                    let current_open = open_orders_count.load(std::sync::atomic::Ordering::Relaxed);
                    if current_open >= max_open_orders {
                        info!(
                            "⏸️ Max open orders ({}/{}) reached, skipping signal: {} {:?}",
                            current_open, max_open_orders, signal.symbol, signal.side
                        );
                        continue;
                    }

                    // Wait if grid is being reset (prevents nonce race)
                    if grid_resetting.load(std::sync::atomic::Ordering::Relaxed) {
                        debug!(
                            "⏳ Grid resetting, skipping signal: {} {:?}",
                            signal.symbol, signal.side
                        );
                        continue;
                    }

                    // Leverage limit: block new position-increasing signals if leverage > limit
                    let leverage_limit = {
                        let ds = dash_state.read().await;
                        ds.leverage_limit
                    };
                    if current_leverage > leverage_limit {
                        let existing_side: Option<String> = {
                            let ds = dash_state.read().await;
                            ds.positions
                                .iter()
                                .find(|p| {
                                    p["symbol"]
                                        .as_str()
                                        .map(|s| signal.symbol.contains(s))
                                        .unwrap_or(false)
                                })
                                .and_then(|p| p["side"].as_str().map(|s| s.to_string()))
                        };
                        let would_increase = matches!(
                            (existing_side.as_deref(), signal.side),
                            (Some("Buy"), lighter::types::Side::Buy)
                                | (Some("Sell"), lighter::types::Side::Sell)
                        );
                        if would_increase {
                            info!("⚠️ Leverage {:.1}x > {:.1}x limit, blocking same-direction signal: {} {:?}",
                                current_leverage, leverage_limit, signal.symbol, signal.side);
                            continue;
                        }
                    }

                    // Dedup: skip if an order already exists at a similar price (within 0.3%)
                    {
                        let ds = dash_state.read().await;
                        let has_dup = ds.open_orders_list.iter().any(|o| {
                            let same_symbol = o["symbol"].as_str() == Some(&signal.symbol);
                            let same_side = o["side"].as_str()
                                == Some(match signal.side {
                                    lighter::types::Side::Buy => "Buy",
                                    lighter::types::Side::Sell => "Sell",
                                });
                            let order_price = o["price"].as_f64().unwrap_or(0.0);
                            let price_diff = (order_price - signal.price).abs() / signal.price;
                            same_symbol && same_side && price_diff < 0.0008
                        });
                        if has_dup {
                            debug!(
                                "🔄 Skipping duplicate signal: {} {:?} @ {:.2}",
                                signal.symbol, signal.side, signal.price
                            );
                            continue;
                        }
                    }

                    // Risk check
                    {
                        let rm = risk_manager.lock().await;
                        if !rm.check_signal(&signal).await.unwrap_or(false) {
                            continue;
                        }
                    }

                    // Risk-reducing signals (exits/stop-loss): clamp quantity to the
                    // actual position held. Defense in depth on top of reduce_only
                    // — 即使交易所侧 ReduceOnly 生效，也要保证发出的数量语义正确，
                    // 防止策略状态滞后导致超大离场量（2026-08-08 事故的 qty 抬升隐患）。
                    if signal.risk_reducing {
                        let held_size = {
                            let ds = dash_state.read().await;
                            ds.positions
                                .iter()
                                .find(|p| {
                                    p["symbol"]
                                        .as_str()
                                        .map(|s| signal.symbol.contains(s))
                                        .unwrap_or(false)
                                })
                                .map(|p| p["size"].as_f64().unwrap_or(0.0).abs())
                                .unwrap_or(0.0)
                        };
                        if held_size <= 0.0 {
                            info!(
                                "⏭️ Risk-reducing signal for {} but no position held, skipping",
                                signal.symbol
                            );
                            continue;
                        }
                        if signal.quantity > held_size {
                            info!(
                                "🛡️ Clamping risk-reducing {} qty {:.6} -> held {:.6}",
                                signal.symbol, signal.quantity, held_size
                            );
                            signal.quantity = held_size;
                        }
                    }

                    let market_info = market_infos.get(&signal.market_id);
                    info!(
                        "📊 Signal: {} {:?} {} @ ${:.2} qty={:.6} — {}",
                        signal.symbol,
                        signal.side,
                        signal.market_id,
                        signal.price,
                        signal.quantity,
                        signal.reason
                    );

                    match lighter_client
                        .place_order_with_market(
                            signal.market_id,
                            signal.side,
                            signal.price,
                            signal.quantity,
                            market_info,
                            signal.risk_reducing,
                        )
                        .await
                    {
                        Ok(resp) => {
                            trade_count += 1;
                            // Optimistically increment open orders counter
                            open_orders_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            info!(
                                "✅ Order placed: id={}, status={}",
                                resp.order_id, resp.status
                            );

                            // Update dashboard
                            let mut ds = dash_state.write().await;
                            ds.total_trades = trade_count;
                            ds.open_orders =
                                open_orders_count.load(std::sync::atomic::Ordering::Relaxed);
                            // Determine action: Open (new position) or Add (increase existing)
                            let action = {
                                let has_position = ds.positions.iter().any(|p| {
                                    p.get("symbol").and_then(|s| s.as_str()) == Some(&signal.symbol)
                                });
                                if has_position {
                                    "Add"
                                } else {
                                    "Open"
                                }
                            };
                            // Shared path with close events: updates lifetime volume
                            // and trims to TRADE_HISTORY_LIMIT (was inconsistently 100 here).
                            // Disk flush happens on close events / periodic equity save.
                            ds.push_trade(serde_json::json!({
                                "timestamp": signal.timestamp.to_rfc3339(),
                                "symbol": signal.symbol,
                                "market_id": signal.market_id,
                                "side": format!("{:?}", signal.side),
                                "price": signal.price,
                                "quantity": signal.quantity,
                                "pnl": 0.0,
                                "action": action,
                                "reason": signal.reason,
                            }));
                        }
                        Err(e) => {
                            error!("❌ Order failed: {}", e);
                        }
                    }
                }
            }
            Ok(None) => {} // No signals
            Err(e) => {
                warn!("Strategy error: {}", e);
            }
        }

        // Update risk status in dashboard every 5 seconds (not every tick)
        if last_risk_update.elapsed() >= std::time::Duration::from_secs(5) {
            let rm = risk_manager.lock().await;
            let risk_status = rm.status();
            let is_emergency = rm.is_emergency_triggered();
            drop(rm);

            // If emergency triggered, stop processing new signals
            if is_emergency {
                let mut ds = dash_state.write().await;
                ds.risk_status = Some(serde_json::json!({
                    "drawdown_pct": risk_status.drawdown_pct,
                    "daily_loss_pct": risk_status.daily_loss_pct,
                    "max_drawdown_limit": risk_status.max_drawdown_limit,
                    "daily_loss_limit": risk_status.daily_loss_limit,
                    "position_stop_loss_pct": risk_status.position_stop_loss_pct,
                    "position_take_profit_pct": risk_status.position_take_profit_pct,
                    "is_healthy": risk_status.is_healthy,
                    "emergency_triggered": risk_status.emergency_triggered,
                }));
                drop(ds);
                warn!("🚨 紧急模式 — 停止所有交易信号处理");
                last_risk_update = std::time::Instant::now();
                continue;
            }

            let mut ds = dash_state.write().await;
            ds.risk_status = Some(serde_json::json!({
                "drawdown_pct": risk_status.drawdown_pct,
                "daily_loss_pct": risk_status.daily_loss_pct,
                "max_drawdown_limit": risk_status.max_drawdown_limit,
                "daily_loss_limit": risk_status.daily_loss_limit,
                "position_stop_loss_pct": risk_status.position_stop_loss_pct,
                "position_take_profit_pct": risk_status.position_take_profit_pct,
                "is_healthy": risk_status.is_healthy,
                "emergency_triggered": risk_status.emergency_triggered,
            }));
            drop(ds);
            last_risk_update = std::time::Instant::now();
        }

        apply_pending_strategy_update(&dash_state, &strategy).await;
    }

    Ok(())
}

async fn apply_pending_strategy_update(
    dash_state: &Arc<RwLock<dashboard::server::DashboardState>>,
    execution_strategy: &Arc<tokio::sync::RwLock<Box<dyn strategy::Strategy>>>,
) -> bool {
    let (requested_name, params) = {
        let mut dashboard = dash_state.write().await;
        if !dashboard.strategy_config_changed {
            return false;
        }
        dashboard.strategy_config_changed = false;
        (
            dashboard.strategy_name.clone(),
            dashboard.strategy_params.clone(),
        )
    };

    let current_name = execution_strategy.read().await.name().to_string();
    let target_name = if requested_name.is_empty() {
        current_name.clone()
    } else {
        requested_name
    };
    let mut params = params.into_iter().collect::<Vec<_>>();
    params.sort_by(|left, right| left.0.cmp(&right.0));
    let params_string = params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");

    if target_name == current_name && params_string.is_empty() {
        return false;
    }
    if target_name != current_name {
        info!("🔄 Strategy switch: {current_name} → {target_name}");
    } else {
        info!("🔧 Strategy params update: {:?}", params);
    }

    match strategy::create_strategy_with_params(
        &target_name,
        (!params_string.is_empty()).then_some(params_string.as_str()),
    ) {
        Ok(new_strategy) => {
            *execution_strategy.write().await = new_strategy;
            dash_state.write().await.strategy_name = target_name.clone();
            info!("✅ Execution strategy active: {target_name}");
            true
        }
        Err(error) => {
            warn!("❌ Strategy update failed: {error} — keeping {current_name}");
            dash_state.write().await.strategy_name = current_name;
            false
        }
    }
}

async fn load_arcus_fills_from(
    client: &multi_venue_quant_bot::arcus::ArcusClient,
    address: &str,
    account_index: u8,
    from_micros: Option<i64>,
) -> Result<Vec<multi_venue_quant_bot::arcus::ArcusFill>> {
    const PAGE_SIZE: u16 = 1000;
    const MAX_PAGES: usize = 1000;

    let mut fills = Vec::new();
    let mut seen = HashSet::new();
    let mut to = None;
    for _ in 0..MAX_PAGES {
        let page = client
            .fills(address, account_index, from_micros, to, PAGE_SIZE)
            .await
            .context("failed to fetch Arcus fills")?;
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        let oldest = page.iter().map(|fill| fill.created_at).min();
        for fill in page {
            if seen.insert(fill.trade_id.clone()) {
                fills.push(fill);
            }
        }
        if page_len < PAGE_SIZE as usize {
            break;
        }
        let Some(oldest) = oldest else { break };
        if from_micros.is_some_and(|from| oldest <= from) {
            break;
        }
        let next_to = oldest.saturating_sub(1);
        if to.is_some_and(|current| next_to >= current) {
            anyhow::bail!("Arcus fill pagination cursor did not advance");
        }
        to = Some(next_to);
    }
    fills.sort_by_key(|fill| fill.created_at);
    Ok(fills)
}

async fn load_all_arcus_fills(
    client: &multi_venue_quant_bot::arcus::ArcusClient,
    address: &str,
    account_index: u8,
) -> Result<Vec<multi_venue_quant_bot::arcus::ArcusFill>> {
    load_arcus_fills_from(client, address, account_index, None).await
}

fn arcus_funding_key(payment: &multi_venue_quant_bot::arcus::ArcusFundingPayment) -> String {
    format!(
        "{}:{}:{}:{}",
        payment.market_id, payment.time, payment.size, payment.payment
    )
}

async fn load_arcus_funding_from(
    client: &multi_venue_quant_bot::arcus::ArcusClient,
    address: &str,
    account_index: u8,
    from_micros: Option<i64>,
) -> Result<Vec<multi_venue_quant_bot::arcus::ArcusFundingPayment>> {
    const PAGE_SIZE: u16 = 1000;
    const MAX_PAGES: usize = 1000;

    let from_ms = from_micros.map(|value| value / 1_000);
    let mut payments = Vec::new();
    let mut seen = HashSet::new();
    let mut to_ms = None;
    for _ in 0..MAX_PAGES {
        let page = client
            .funding_payments(address, account_index, from_ms, to_ms, PAGE_SIZE)
            .await
            .context("failed to fetch Arcus funding payments")?;
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        let oldest = page.iter().map(|payment| payment.time).min();
        for payment in page {
            if seen.insert(arcus_funding_key(&payment)) {
                payments.push(payment);
            }
        }
        if page_len < PAGE_SIZE as usize {
            break;
        }
        let Some(oldest) = oldest else { break };
        if from_micros.is_some_and(|from| oldest <= from) {
            break;
        }
        let next_to_ms = oldest.saturating_sub(1) / 1_000;
        if to_ms.is_some_and(|current| next_to_ms >= current) {
            anyhow::bail!("Arcus funding pagination cursor did not advance");
        }
        to_ms = Some(next_to_ms);
    }
    payments.sort_by_key(|payment| payment.time);
    Ok(payments)
}

async fn load_all_arcus_funding(
    client: &multi_venue_quant_bot::arcus::ArcusClient,
    address: &str,
    account_index: u8,
) -> Result<Vec<multi_venue_quant_bot::arcus::ArcusFundingPayment>> {
    load_arcus_funding_from(client, address, account_index, None).await
}

fn arcus_fill_action(fill: &multi_venue_quant_bot::arcus::ArcusFill) -> &'static str {
    match fill.position_effect.as_deref().unwrap_or_default() {
        effect if effect.starts_with("CLOSE_") => "Close",
        effect if effect.starts_with("FLIP_") => "Flip Close",
        effect if effect.starts_with("ADD_") => "Add",
        _ => "Open",
    }
}

fn arcus_fill_trade(fill: &multi_venue_quant_bot::arcus::ArcusFill) -> Result<serde_json::Value> {
    let timestamp = chrono::DateTime::<Utc>::from_timestamp_micros(fill.created_at)
        .context("Arcus fill timestamp is out of range")?;
    let price = fill
        .price
        .parse::<f64>()
        .context("invalid Arcus fill price")?;
    let quantity = fill
        .size
        .parse::<f64>()
        .context("invalid Arcus fill size")?;
    Ok(serde_json::json!({
        "timestamp": timestamp.to_rfc3339(),
        "symbol": fill.market_display_name,
        "market_id": fill.market_id,
        "side": format!("{:?}", fill.side),
        "price": price,
        "quantity": quantity,
        "pnl": fill.net_realized_pnl()?,
        "closed_pnl": fill.closed_pnl.parse::<f64>().unwrap_or(0.0),
        "fee": fill.fee.parse::<f64>().unwrap_or(0.0),
        "action": arcus_fill_action(fill),
        "position_effect": fill.position_effect,
        "role": fill.role,
        "trade_id": fill.trade_id,
        "order_id": fill.order_id,
    }))
}

fn apply_arcus_fill(
    dashboard: &mut dashboard::server::DashboardState,
    fill: &multi_venue_quant_bot::arcus::ArcusFill,
) -> Result<()> {
    let pnl = fill.net_realized_pnl()?;
    let day = chrono::DateTime::<Utc>::from_timestamp_micros(fill.created_at)
        .context("Arcus fill timestamp is out of range")?
        .format("%Y-%m-%d")
        .to_string();
    *dashboard.daily_pnl_map.entry(day.clone()).or_insert(0.0) += pnl;
    if day == Utc::now().format("%Y-%m-%d").to_string() {
        dashboard.daily_realized_pnl += pnl;
    }
    dashboard.total_realized_pnl += pnl;
    dashboard.push_trade(arcus_fill_trade(fill)?);
    Ok(())
}

fn apply_arcus_funding(
    dashboard: &mut dashboard::server::DashboardState,
    payment: &multi_venue_quant_bot::arcus::ArcusFundingPayment,
) -> Result<()> {
    let pnl = payment.payment_value()?;
    let day = chrono::DateTime::<Utc>::from_timestamp_micros(payment.time)
        .context("Arcus funding timestamp is out of range")?
        .format("%Y-%m-%d")
        .to_string();
    *dashboard.daily_pnl_map.entry(day.clone()).or_insert(0.0) += pnl;
    dashboard.total_funding_pnl += pnl;
    dashboard.total_realized_pnl += pnl;
    if day == Utc::now().format("%Y-%m-%d").to_string() {
        dashboard.daily_funding_pnl += pnl;
        dashboard.daily_realized_pnl += pnl;
    }
    Ok(())
}

fn rebuild_arcus_account_history(
    dashboard: &mut dashboard::server::DashboardState,
    fills: &[multi_venue_quant_bot::arcus::ArcusFill],
    funding: &[multi_venue_quant_bot::arcus::ArcusFundingPayment],
) -> Result<()> {
    dashboard.trade_history.clear();
    dashboard.total_volume = 0.0;
    dashboard.total_closed_trades = 0;
    dashboard.total_realized_pnl = 0.0;
    dashboard.daily_realized_pnl = 0.0;
    dashboard.total_funding_pnl = 0.0;
    dashboard.daily_funding_pnl = 0.0;
    dashboard.daily_pnl_map.clear();
    for payment in funding {
        apply_arcus_funding(dashboard, payment)?;
    }
    for fill in fills {
        apply_arcus_fill(dashboard, fill)?;
    }
    dashboard.total_trades = fills.len() as u64;
    Ok(())
}

fn arcus_exchange_client_id(strategy_client_id: &str, timestamp: u64) -> String {
    const SUFFIX_LEN: usize = 17; // '_' plus 16 hexadecimal digits.
    const MAX_CLIENT_ID_LEN: usize = 36;
    let prefix_len = MAX_CLIENT_ID_LEN - SUFFIX_LEN;
    let prefix: String = strategy_client_id.chars().take(prefix_len).collect();
    format!("{prefix}_{timestamp:016x}")
}

fn arcus_maker_quote_key(symbol: &str, side: multi_venue_quant_bot::arcus::OrderSide) -> String {
    let side = match side {
        multi_venue_quant_bot::arcus::OrderSide::Buy => "buy",
        multi_venue_quant_bot::arcus::OrderSide::Sell => "sell",
    };
    format!("mq_{symbol}_{side}")
}

fn arcus_open_order_limit_reached(
    reconciled_open_orders: u32,
    local_resting_quotes: usize,
    max_open_orders: u32,
) -> bool {
    reconciled_open_orders >= max_open_orders || local_resting_quotes >= max_open_orders as usize
}

fn arcus_quote_attempt_ready(
    last_failed_attempt: Option<std::time::Instant>,
    cooldown: std::time::Duration,
    now: std::time::Instant,
) -> bool {
    last_failed_attempt
        .map(|last| now.saturating_duration_since(last) >= cooldown)
        .unwrap_or(true)
}

fn arcus_resting_quote_is_live(
    quote_client_id: &str,
    quote_order_id: Option<&str>,
    live_client_ids: &HashSet<&str>,
    live_order_ids: &HashSet<&str>,
) -> bool {
    live_client_ids.contains(quote_client_id)
        || quote_order_id.is_some_and(|order_id| live_order_ids.contains(order_id))
}

fn arcus_signal_allowed_while_paused(
    trading_paused: bool,
    risk_reducing: bool,
    post_only: bool,
) -> bool {
    !trading_paused || (risk_reducing && !post_only)
}

async fn run_arcus_live_trading(settings: Config) -> Result<()> {
    use multi_venue_quant_bot::arcus::{
        ArcusClient, ArcusEnvironment, ArcusKeypair, ArcusMarket, ArcusWebSocket, ArcusWsEvent,
        CancelOrder, CancelOrderRequest, CancelTarget, DecimalGrid, OrderSide as ArcusSide,
        PlaceOrder, PlaceOrderRequest, TimeInForce,
    };
    use multi_venue_quant_bot::exchange::LiveVenue;

    /// 本 bot 挂出订单的镜像，供 maker 报价去重/替换与对账。
    #[derive(Clone, Debug)]
    struct RestingQuote {
        client_id: String,
        order_id: Option<String>,
        symbol: String,
        side: ArcusSide,
        price: f64,
        quantity: f64,
        /// 最近一次下单时间，用于报价替换冷却（防 BBO 每 tick 微变触发撤单风暴）
        last_action: std::time::Instant,
    }

    let environment = match settings
        .get_string("exchange.environment")
        .unwrap_or_else(|_| "mainnet".into())
        .as_str()
    {
        "mainnet" => ArcusEnvironment::Mainnet,
        "testnet" => ArcusEnvironment::Testnet,
        other => anyhow::bail!("unsupported Arcus environment {other}"),
    };
    let venue = match environment {
        ArcusEnvironment::Mainnet => LiveVenue::ArcusMainnet,
        ArcusEnvironment::Testnet => LiveVenue::ArcusTestnet,
    };
    let selected = env_profiles::selected_venue();
    if selected.exchange() == multi_venue_quant_bot::exchange::ExchangeKind::Arcus
        && selected != venue
    {
        anyhow::bail!(
            "selected venue {selected} does not match config environment {}",
            venue.as_str()
        );
    }

    let (credentials, credential_path) = env_profiles::load_arcus_credentials(venue)?;
    let keypair = ArcusKeypair::from_secret_hex(&credentials.signing_key)
        .context("invalid Arcus Ed25519 API signing key")?;
    let api_key = credentials.api_key;
    let client = Arc::new(
        ArcusClient::authenticated_with_keypair(environment, &api_key, keypair)
            .context("failed to initialize Arcus client")?,
    );
    info!(
        "🔐 Using {} account {} subaccount {} from {} (API key …{})",
        venue,
        credentials.address,
        credentials.account_index,
        credential_path.display(),
        &api_key[api_key.len().saturating_sub(8)..]
    );

    let account = client
        .account(&credentials.address, credentials.account_index)
        .await
        .context("failed to fetch Arcus account")?;
    let equity = account
        .equity
        .parse::<f64>()
        .context("invalid Arcus equity")?;
    let free_collateral = account
        .free_collateral
        .parse::<f64>()
        .context("invalid Arcus free collateral")?;
    let positions = account
        .market_positions()
        .context("invalid Arcus position response")?;
    client
        .cancel_all_orders(&credentials.address, credentials.account_index)
        .await
        .context("Arcus startup cancel-all failed; refusing to start trading")?;
    info!("✅ Cleared existing Arcus orders before strategy startup");

    let raw_markets = client
        .markets()
        .await
        .context("failed to fetch Arcus markets")?;
    let configured_ids: Vec<i64> = settings
        .get("trading.markets")
        .context("trading.markets is required for Arcus live mode")?;
    let market_ids: Vec<u32> = configured_ids
        .into_iter()
        .map(|id| u32::try_from(id).context("Arcus market id must be positive"))
        .collect::<Result<_>>()?;
    let mut markets = std::collections::HashMap::<u32, ArcusMarket>::new();
    for market in &raw_markets {
        if market.status == "ONLINE" && market_ids.contains(&(market.market_id as u32)) {
            markets.insert(
                market.market_id as u32,
                ArcusMarket {
                    market_id: market.market_id,
                    symbol: market.market_display_name.clone(),
                    tick_size: DecimalGrid::new(&market.tick_size)?,
                    step_size: DecimalGrid::new(&market.step_size)?,
                },
            );
        }
    }
    if markets.len() != market_ids.len() {
        anyhow::bail!(
            "one or more configured Arcus markets are missing or offline: {:?}",
            market_ids
        );
    }

    let strategy: Arc<tokio::sync::RwLock<Box<dyn strategy::Strategy>>> = Arc::new(
        tokio::sync::RwLock::new(strategy::create_strategy(&settings)?),
    );
    let strategy_name = strategy.read().await.name().to_string();
    let risk_manager = Arc::new(tokio::sync::Mutex::new(
        risk::risk_manager::RiskManager::new(&settings)?,
    ));
    risk_manager.lock().await.update_equity(equity);

    let dash_state = Arc::new(RwLock::new(dashboard::server::DashboardState {
        network_name: venue.as_str().to_string(),
        rest_url: environment.rest_url().to_string(),
        ws_url: environment.websocket_url().to_string(),
        chain_id: 0,
        equity,
        available_balance: free_collateral,
        unrealized_pnl: positions
            .iter()
            .filter_map(|p| p.unrealized_pnl.parse::<f64>().ok())
            .sum(),
        strategy_name,
        initial_equity: equity,
        peak_equity: equity,
        equity_history: vec![(Utc::now().timestamp(), equity)],
        active_markets: market_ids.clone(),
        trading_paused: settings.get_bool("trading.start_paused").unwrap_or(false),
        available_markets: raw_markets
            .iter()
            .filter(|market| market.status == "ONLINE")
            .map(|market| (market.market_id as u32, market.market_display_name.clone()))
            .collect(),
        positions: arcus_dashboard_positions(&positions),
        leverage_limit: 3.0,
        quant_agent: dashboard::quant_agent::AgentLedger::load(venue.as_str()),
        ..dashboard::server::DashboardState::default()
    }));

    // Arcus order responses are acknowledgements, not executions. Restore chart
    // baselines first, then rebuild all trading statistics from the exchange's
    // authoritative fill ledger so restarts also repair legacy over-counting.
    if let Some(saved) = dashboard::server::PersistentPnlData::load(venue.as_str()) {
        dash_state.write().await.restore_pnl(&saved);
    }
    let mut known_fill_ids = HashSet::new();
    let mut known_funding_ids = HashSet::new();
    let mut last_fill_micros = None;
    let mut last_funding_micros = None;
    let mut account_history_reconciled = false;
    let (fills, funding) = tokio::join!(
        load_all_arcus_fills(&client, &credentials.address, credentials.account_index),
        load_all_arcus_funding(&client, &credentials.address, credentials.account_index),
    );
    match (fills, funding) {
        (Ok(fills), Ok(funding)) => {
            account_history_reconciled = true;
            known_fill_ids.extend(fills.iter().map(|fill| fill.trade_id.clone()));
            known_funding_ids.extend(funding.iter().map(arcus_funding_key));
            last_fill_micros = fills.iter().map(|fill| fill.created_at).max();
            last_funding_micros = funding.iter().map(|payment| payment.time).max();
            let mut dashboard = dash_state.write().await;
            rebuild_arcus_account_history(&mut dashboard, &fills, &funding)?;
            dashboard.save_pnl();
            info!(
                "📂 Reconciled {} Arcus fills and {} funding payments: volume={:.2}, net realized PnL={:+.4}",
                fills.len(),
                funding.len(),
                dashboard.total_volume,
                dashboard.total_realized_pnl
            );
        }
        (fills, funding) => {
            warn!(
                "⚠️ Initial Arcus account reconciliation failed; fills={}, funding={}; will retry",
                fills
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "ok".into()),
                funding
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "ok".into())
            );
        }
    }

    if let Some(saved) = dashboard::server::PersistentStrategyConfig::load(venue.as_str()) {
        info!(
            "📂 Loaded strategy config: {} params={:?}",
            saved.strategy_name, saved.strategy_params
        );
        {
            let mut dashboard = dash_state.write().await;
            dashboard.strategy_name = saved.strategy_name;
            dashboard.strategy_params = saved.strategy_params;
            dashboard.strategy_config_changed = true;
        }
        apply_pending_strategy_update(&dash_state, &strategy).await;
    }
    let dashboard_port = settings.get_int("dashboard.port").unwrap_or(4028) as u16;
    let dashboard_host = settings
        .get_string("dashboard.host")
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    let dashboard_state = dash_state.clone();
    tokio::spawn(async move {
        if let Err(error) =
            dashboard::server::start_with_state(&dashboard_host, dashboard_port, dashboard_state)
                .await
        {
            error!("Arcus dashboard failed: {error}");
        }
    });

    let data_store = Arc::new(RwLock::new(data::storage::MarketDataStore::new()));
    for market in markets.values() {
        match client.candles(&market.symbol, "1h", 100).await {
            Ok(candles) => {
                let mut store = data_store.write().await;
                for candle in candles {
                    let Some(timestamp) =
                        chrono::DateTime::<Utc>::from_timestamp_micros(candle.open_time)
                    else {
                        continue;
                    };
                    let parsed = || -> Option<lighter::types::Candlestick> {
                        Some(lighter::types::Candlestick {
                            timestamp,
                            open: candle.open.parse().ok()?,
                            high: candle.high.parse().ok()?,
                            low: candle.low.parse().ok()?,
                            close: candle.close.parse().ok()?,
                            volume: candle.volume.parse().ok()?,
                            symbol: market.symbol.clone(),
                        })
                    };
                    if let Some(candle) = parsed() {
                        store.add_candle(candle);
                    }
                }
            }
            Err(error) => warn!(
                "failed to load Arcus candles for {}: {error}",
                market.symbol
            ),
        }
    }

    let websocket = ArcusWebSocket::new(environment.websocket_url());
    websocket
        .connect()
        .await
        .context("failed to connect Arcus WebSocket")?;
    for market in markets.values() {
        websocket.subscribe_bbo(&market.symbol).await?;
    }
    websocket.subscribe_orders(&credentials.address).await?;
    websocket.subscribe_user_fills(&credentials.address).await?;
    websocket.subscribe_funding(&credentials.address).await?;
    let mut events = websocket.receiver();

    let refresh_client = client.clone();
    let refresh_address = credentials.address.clone();
    let refresh_account_index = credentials.account_index;
    let refresh_dashboard = dash_state.clone();
    let refresh_risk = risk_manager.clone();
    // 本 bot 挂出的订单镜像（client_id → 报价），供 maker 报价去重/替换与对账。
    // 与 10s 刷新任务共享：刷新侧用交易所 open_orders 对账移除已成交/被撤条目。
    let resting_quotes: Arc<Mutex<HashMap<String, RestingQuote>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let refresh_resting = resting_quotes.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;

            // Rebuild only when both authoritative ledgers are available.
            if !account_history_reconciled {
                let (fills, funding) = tokio::join!(
                    load_all_arcus_fills(&refresh_client, &refresh_address, refresh_account_index,),
                    load_all_arcus_funding(
                        &refresh_client,
                        &refresh_address,
                        refresh_account_index,
                    ),
                );
                match (fills, funding) {
                    (Ok(fills), Ok(funding)) => {
                        let mut dashboard = refresh_dashboard.write().await;
                        if let Err(error) =
                            rebuild_arcus_account_history(&mut dashboard, &fills, &funding)
                        {
                            warn!("⚠️ Arcus account ledger contains invalid data: {error}");
                        } else {
                            known_fill_ids =
                                fills.iter().map(|fill| fill.trade_id.clone()).collect();
                            known_funding_ids = funding.iter().map(arcus_funding_key).collect();
                            last_fill_micros = fills.iter().map(|fill| fill.created_at).max();
                            last_funding_micros = funding.iter().map(|payment| payment.time).max();
                            dashboard.save_pnl();
                            account_history_reconciled = true;
                        }
                    }
                    (fills, funding) => warn!(
                        "⚠️ Arcus account reconciliation retry failed: fills={}, funding={}",
                        fills
                            .err()
                            .map(|error| error.to_string())
                            .unwrap_or_else(|| "ok".into()),
                        funding
                            .err()
                            .map(|error| error.to_string())
                            .unwrap_or_else(|| "ok".into())
                    ),
                }
            }

            let cancel_requested = refresh_dashboard.read().await.cancel_all_requested;
            if cancel_requested {
                match refresh_client
                    .cancel_all_orders(&refresh_address, refresh_account_index)
                    .await
                {
                    Ok(_) => {
                        info!("✅ Arcus cancel-all accepted");
                        refresh_dashboard.write().await.cancel_all_requested = false;
                        // Keep the mirror until REST/WS confirms terminal order state.
                    }
                    Err(error) => {
                        error!("❌ Arcus cancel-all failed: {error}");
                        // Keep the request armed so the next refresh retries.
                    }
                }
            }
            let (account, orders, fills, funding) = tokio::join!(
                refresh_client.account(&refresh_address, refresh_account_index),
                refresh_client.open_orders(&refresh_address, refresh_account_index),
                load_arcus_fills_from(
                    &refresh_client,
                    &refresh_address,
                    refresh_account_index,
                    last_fill_micros,
                ),
                load_arcus_funding_from(
                    &refresh_client,
                    &refresh_address,
                    refresh_account_index,
                    last_funding_micros,
                ),
            );
            if let Ok(account) = account {
                if let (Ok(equity), Ok(free), Ok(positions)) = (
                    account.equity.parse::<f64>(),
                    account.free_collateral.parse::<f64>(),
                    account.market_positions(),
                ) {
                    let unrealized = positions
                        .iter()
                        .filter_map(|position| position.unrealized_pnl.parse::<f64>().ok())
                        .sum();
                    refresh_risk.lock().await.update_equity(equity);
                    let mut dashboard = refresh_dashboard.write().await;
                    dashboard.equity = equity;
                    dashboard.available_balance = free;
                    dashboard.unrealized_pnl = unrealized;
                    dashboard.positions = arcus_dashboard_positions(&positions);
                    if equity > dashboard.peak_equity {
                        dashboard.peak_equity = equity;
                    }
                    let now = Utc::now().timestamp();
                    let should_record = dashboard
                        .equity_history
                        .last()
                        .map(|(timestamp, _)| now - timestamp >= 60)
                        .unwrap_or(true);
                    if should_record {
                        dashboard.equity_history.push((now, equity));
                        let cumulative_pnl = equity - dashboard.initial_equity;
                        dashboard.pnl_history.push((now, cumulative_pnl));
                        if dashboard.equity_history.len() > 10_080 {
                            dashboard.equity_history.remove(0);
                            dashboard.pnl_history.remove(0);
                        }
                        if dashboard.equity_history.len() % 5 == 0 {
                            dashboard.save_pnl();
                        }
                    }
                }
            }
            match fills {
                Ok(fills) => {
                    let mut new_fills = 0_u64;
                    let mut dashboard = refresh_dashboard.write().await;
                    for fill in fills {
                        last_fill_micros = Some(
                            last_fill_micros
                                .map_or(fill.created_at, |last| last.max(fill.created_at)),
                        );
                        if known_fill_ids.contains(&fill.trade_id) {
                            continue;
                        }
                        match apply_arcus_fill(&mut dashboard, &fill) {
                            Ok(()) => {
                                known_fill_ids.insert(fill.trade_id);
                                new_fills += 1;
                            }
                            Err(error) => warn!("⚠️ Invalid Arcus fill ignored: {error}"),
                        }
                    }
                    dashboard.total_trades = known_fill_ids.len() as u64;
                    if new_fills > 0 {
                        info!(
                            "💰 Reconciled {new_fills} new Arcus fills: volume={:.2}, net realized PnL={:+.4}",
                            dashboard.total_volume, dashboard.total_realized_pnl
                        );
                        dashboard.save_pnl();
                    }
                }
                Err(error) => warn!("⚠️ Arcus fills refresh failed: {error}"),
            }
            match funding {
                Ok(payments) => {
                    let mut new_payments = 0_u64;
                    let mut dashboard = refresh_dashboard.write().await;
                    for payment in payments {
                        last_funding_micros = Some(
                            last_funding_micros.map_or(payment.time, |last| last.max(payment.time)),
                        );
                        let key = arcus_funding_key(&payment);
                        if known_funding_ids.contains(&key) {
                            continue;
                        }
                        match apply_arcus_funding(&mut dashboard, &payment) {
                            Ok(()) => {
                                known_funding_ids.insert(key);
                                new_payments += 1;
                            }
                            Err(error) => warn!("⚠️ Invalid Arcus funding ignored: {error}"),
                        }
                    }
                    if new_payments > 0 {
                        info!(
                            "💰 Reconciled {new_payments} Arcus funding payments: funding PnL={:+.4}",
                            dashboard.total_funding_pnl
                        );
                        dashboard.save_pnl();
                    }
                }
                Err(error) => warn!("⚠️ Arcus funding refresh failed: {error}"),
            }
            let daily_realized_pnl = refresh_dashboard.read().await.daily_realized_pnl;
            let emergency = {
                let mut risk = refresh_risk.lock().await;
                risk.update_daily_pnl(daily_realized_pnl);
                if risk.should_emergency_close() && !risk.is_emergency_triggered() {
                    risk.set_emergency_triggered();
                    true
                } else {
                    false
                }
            };
            if emergency {
                {
                    let mut dashboard = refresh_dashboard.write().await;
                    dashboard.trading_paused = true;
                    dashboard.cancel_all_requested = true;
                }
                match refresh_client
                    .cancel_all_orders(&refresh_address, refresh_account_index)
                    .await
                {
                    Ok(_) => {
                        refresh_dashboard.write().await.cancel_all_requested = false;
                        error!("🚨 Arcus emergency gate paused trading and accepted cancel-all");
                    }
                    Err(error) => {
                        error!("🚨 Arcus emergency cancel-all failed and remains armed: {error}");
                    }
                }
            }
            if let Ok(orders) = orders {
                // 对账：交易所侧已消失的挂单（成交/被撤）从本地镜像移除，
                // 使 maker 策略经 snapshot.open_orders 感知成交并重新报价。
                {
                    let live_client_ids: HashSet<&str> = orders
                        .iter()
                        .filter_map(|order| order.client_id.as_deref())
                        .collect();
                    let live_order_ids: HashSet<&str> =
                        orders.iter().map(|order| order.order_id.as_str()).collect();
                    let mut resting = refresh_resting.lock();
                    let removed: Vec<String> = resting
                        .iter()
                        .filter(|(client_id, quote)| {
                            !arcus_resting_quote_is_live(
                                client_id,
                                quote.order_id.as_deref(),
                                &live_client_ids,
                                &live_order_ids,
                            )
                        })
                        .map(|(client_id, _)| client_id.clone())
                        .collect();
                    for cid in removed {
                        if let Some(q) = resting.remove(&cid) {
                            info!(
                                "maker quote {} {} gone from exchange (filled/cancelled)",
                                q.symbol, cid
                            );
                        }
                    }
                }
                let mut dashboard = refresh_dashboard.write().await;
                dashboard.open_orders = orders.len() as u32;
                dashboard.open_orders_list = orders
                    .into_iter()
                    .map(|order| {
                        serde_json::json!({
                            "id": order.order_id,
                            "symbol": order.market_display_name,
                            "side": format!("{:?}", order.side),
                            "price": order.price.parse::<f64>().unwrap_or(0.0),
                            "quantity": order.original_size.parse::<f64>().unwrap_or(0.0),
                            "filled_quantity": order.filled_size.parse::<f64>().unwrap_or(0.0),
                            "status": order.status,
                        })
                    })
                    .collect();
            }
        }
    });

    if dash_state.read().await.trading_paused {
        warn!("⏸️ Arcus connected on {venue}; entry strategy is paused");
    } else {
        info!("🚀 Arcus live trading active on {venue}");
    }
    let max_open_orders = settings.get_int("trading.max_open_orders").unwrap_or(8) as u32;
    // 报价替换冷却：同 client_id 的撤旧挂新最少间隔（秒），防止 BBO 每 tick 微变触发撤单风暴。
    // 冷却期内保持旧单在场（价格略旧可接受，30bps 宽价差下数秒漂移 < 1bp）。
    let requote_cooldown = std::time::Duration::from_secs(
        settings
            .get_int("trading.strategies.maker_quote.requote_cooldown_secs")
            .unwrap_or(5)
            .max(1) as u64,
    );
    // 签名 timestamp 单调递增（Arcus 防重放：同 tick 多信号/撤单+挂新共用 now() 会被 401 拒）
    let last_signed_timestamp = std::sync::atomic::AtomicU64::new(0);
    // 交易所异步拒单不会进入 resting_quotes；单独保留失败时间，避免每个 BBO tick 重试。
    let mut failed_quote_attempts = HashMap::<String, std::time::Instant>::new();
    loop {
        let event = events
            .recv()
            .await
            .context("Arcus WebSocket event stream closed")?;
        let (symbol, bid, ask, bid_size, ask_size) = match event {
            ArcusWsEvent::Disconnected => {
                client
                    .cancel_all_orders(&credentials.address, credentials.account_index)
                    .await
                    .context("Arcus WebSocket disconnected and cancel-all failed")?;
                anyhow::bail!(
                    "Arcus WebSocket disconnected; cancel-all accepted and live loop stopped"
                );
            }
            ArcusWsEvent::Orders(orders) => {
                for order in orders {
                    let Some(exchange_client_id) = order.client_id else {
                        continue;
                    };
                    let strategy_client_id = {
                        let resting = resting_quotes.lock();
                        resting.iter().find_map(|(strategy_client_id, quote)| {
                            (quote.client_id == exchange_client_id)
                                .then(|| strategy_client_id.clone())
                        })
                    }
                    .or_else(|| {
                        exchange_client_id
                            .starts_with("mq_")
                            .then(|| arcus_maker_quote_key(&order.market_display_name, order.side))
                    });
                    let status = order.status.to_ascii_uppercase();
                    if status == "REJECTED" {
                        if let Some(strategy_client_id) = strategy_client_id.as_ref() {
                            failed_quote_attempts
                                .insert(strategy_client_id.clone(), std::time::Instant::now());
                        }
                        error!(
                            "❌ Arcus order rejected: client_id={exchange_client_id}, order_id={}, reason={}",
                            order.order_id,
                            order.rejection_reason.as_deref().unwrap_or("unknown")
                        );
                    }
                    if matches!(
                        status.as_str(),
                        "FILLED"
                            | "CANCELED"
                            | "MARGIN_CANCELED"
                            | "REJECTED"
                            | "TPSL_CANCELED"
                            | "TPSL_TRIGGERED"
                    ) {
                        if let Some(strategy_client_id) = strategy_client_id {
                            if let Some(quote) = resting_quotes.lock().remove(&strategy_client_id) {
                                info!(
                                    "maker quote {} {} reached terminal status {}",
                                    quote.symbol, strategy_client_id, status
                                );
                            }
                        }
                    } else if status == "OPEN" && exchange_client_id.starts_with("mq_") {
                        let strategy_client_id = strategy_client_id.expect("maker key");
                        failed_quote_attempts.remove(&strategy_client_id);
                        let price = order.price.parse::<f64>().unwrap_or(0.0);
                        let quantity = order.remaining_size.parse::<f64>().unwrap_or(0.0);
                        if price > 0.0 && quantity > 0.0 {
                            resting_quotes
                                .lock()
                                .entry(strategy_client_id)
                                .and_modify(|quote| {
                                    quote.client_id = exchange_client_id.clone();
                                    quote.order_id = Some(order.order_id.clone());
                                    quote.price = price;
                                    quote.quantity = quantity;
                                })
                                .or_insert_with(|| RestingQuote {
                                    client_id: exchange_client_id,
                                    order_id: Some(order.order_id),
                                    symbol: order.market_display_name,
                                    side: order.side,
                                    price,
                                    quantity,
                                    last_action: std::time::Instant::now(),
                                });
                        }
                    }
                }
                continue;
            }
            ArcusWsEvent::UserFills(fills) => {
                if !fills.is_empty() {
                    debug!("Arcus userFills received: {} fills", fills.len());
                }
                continue;
            }
            ArcusWsEvent::Funding(payments) => {
                if !payments.is_empty() {
                    debug!("Arcus funding received: {} payments", payments.len());
                }
                continue;
            }
            ArcusWsEvent::Bbo {
                symbol,
                bid,
                ask,
                bid_size,
                ask_size,
                ..
            } => (symbol, bid, ask, bid_size, ask_size),
            ArcusWsEvent::Ignored => continue,
        };
        let market = markets
            .values()
            .find(|market| market.symbol == symbol)
            .context("Arcus BBO references an unknown market")?;
        data_store
            .write()
            .await
            .update_order_book(lighter::types::OrderBook {
                symbol: symbol.clone(),
                market_id: market.market_id as u32,
                bids: vec![lighter::types::PriceLevel {
                    price: bid,
                    quantity: bid_size,
                }],
                asks: vec![lighter::types::PriceLevel {
                    price: ask,
                    quantity: ask_size,
                }],
                timestamp: Utc::now(),
            });
        let mut snapshot = data_store.read().await.get_snapshot();
        {
            let dashboard = dash_state.read().await;
            snapshot.positions = dashboard
                .positions
                .iter()
                .filter_map(|position| {
                    let symbol = position.get("symbol")?.as_str()?.to_string();
                    let size = position.get("size")?.as_f64()?;
                    let signed = if position.get("side")?.as_str()? == "Sell" {
                        -size
                    } else {
                        size
                    };
                    Some((symbol, signed))
                })
                .collect();
            snapshot.positions_authoritative = true;
        }
        {
            // 注入本 bot 的挂单视图，供 maker 策略对账（成交/被撤后重新报价）
            let resting = resting_quotes.lock();
            for q in resting.values() {
                snapshot.open_orders.push(lighter::types::OpenOrderRef {
                    symbol: q.symbol.clone(),
                    client_id: Some(q.client_id.clone()),
                    side: match q.side {
                        ArcusSide::Buy => lighter::types::Side::Buy,
                        ArcusSide::Sell => lighter::types::Side::Sell,
                    },
                    price: q.price,
                    quantity: q.quantity,
                    status: "OPEN".into(),
                });
            }
            snapshot.open_orders_authoritative = true;
        }
        dash_state
            .write()
            .await
            .last_prices
            .insert(symbol.clone(), (bid + ask) / 2.0);
        apply_pending_strategy_update(&dash_state, &strategy).await;
        let trading_paused = dash_state.read().await.trading_paused;
        let mut signals = Vec::new();
        let current_position = {
            let dashboard = dash_state.read().await;
            dashboard
                .positions
                .iter()
                .find(|position| {
                    position.get("symbol").and_then(|value| value.as_str()) == Some(symbol.as_str())
                })
                .cloned()
        };
        if let Some(position) = current_position {
            let size = position
                .get("size")
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0);
            let entry_price = position
                .get("entry_price")
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0);
            let position_side =
                if position.get("side").and_then(|value| value.as_str()) == Some("Sell") {
                    lighter::types::Side::Sell
                } else {
                    lighter::types::Side::Buy
                };
            if size > 0.0 && entry_price > 0.0 {
                let mark = (bid + ask) / 2.0;
                let risk_position = lighter::types::Position {
                    symbol: symbol.clone(),
                    side: position_side,
                    size,
                    entry_price,
                    unrealized_pnl: position
                        .get("unrealized_pnl")
                        .and_then(|value| value.as_f64())
                        .unwrap_or(0.0),
                    leverage: position
                        .get("leverage")
                        .and_then(|value| value.as_f64())
                        .unwrap_or(1.0),
                };
                let close = {
                    let risk = risk_manager.lock().await;
                    if risk.is_emergency_triggered() {
                        Some(risk::risk_manager::PositionCloseSignal {
                            symbol: symbol.clone(),
                            side_to_close: match position_side {
                                lighter::types::Side::Buy => lighter::types::Side::Sell,
                                lighter::types::Side::Sell => lighter::types::Side::Buy,
                            },
                            size,
                            entry_price,
                            current_price: mark,
                            pnl_pct: 0.0,
                            reason: "Emergency account close".into(),
                        })
                    } else {
                        risk.check_position_stop_loss_take_profit(
                            &[risk_position],
                            &HashMap::from([(symbol.clone(), mark)]),
                        )
                        .into_iter()
                        .next()
                    }
                };
                if let Some(close) = close {
                    let aggressive_price = match close.side_to_close {
                        lighter::types::Side::Buy => ask * 1.005,
                        lighter::types::Side::Sell => bid * 0.995,
                    };
                    signals.push(lighter::types::TradeSignal {
                        action: lighter::types::SignalAction::Place,
                        symbol: symbol.clone(),
                        market_id: market.market_id as u32,
                        side: close.side_to_close,
                        price: aggressive_price,
                        quantity: close.size,
                        order_type: lighter::types::OrderType::Limit,
                        reason: close.reason,
                        timestamp: Utc::now(),
                        expected_edge_bps: None,
                        risk_reducing: true,
                        post_only: false,
                        client_id: Some(format!(
                            "risk_{}_{}",
                            market.market_id,
                            match close.side_to_close {
                                lighter::types::Side::Buy => "buy",
                                lighter::types::Side::Sell => "sell",
                            }
                        )),
                    });
                }
            }
        }
        if signals.is_empty() {
            if let Some(mut strategy_signals) = strategy.read().await.evaluate(&snapshot).await? {
                strategy_signals.retain(|signal| {
                    arcus_signal_allowed_while_paused(
                        trading_paused,
                        signal.risk_reducing,
                        signal.post_only,
                    )
                });
                signals = strategy_signals;
            }
        }
        if signals.is_empty() {
            continue;
        }
        for mut signal in signals {
            let market = markets
                .get(&signal.market_id)
                .context("strategy emitted unknown Arcus market")?;
            let side = match signal.side {
                lighter::types::Side::Buy => ArcusSide::Buy,
                lighter::types::Side::Sell => ArcusSide::Sell,
            };
            let timestamp = {
                let now_ns = Utc::now()
                    .timestamp_nanos_opt()
                    .context("system clock out of range")? as u64;
                last_signed_timestamp
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .max(now_ns)
                    + 1
            };
            last_signed_timestamp.store(timestamp, std::sync::atomic::Ordering::Relaxed);
            let strategy_client_id = signal
                .client_id
                .clone()
                .unwrap_or_else(|| format!("qb{}_{}", market.market_id, timestamp));

            if signal.action == lighter::types::SignalAction::Cancel {
                let Some(quote) = resting_quotes.lock().get(&strategy_client_id).cloned() else {
                    continue;
                };
                let (target, order_id, cancel_client_id) =
                    if let Some(order_id) = quote.order_id.clone() {
                        (
                            CancelTarget::OrderId(order_id.clone()),
                            Some(order_id),
                            None,
                        )
                    } else {
                        (
                            CancelTarget::ClientId(quote.client_id.clone()),
                            None,
                            Some(quote.client_id.clone()),
                        )
                    };
                let cancel = CancelOrder {
                    address: credentials.address.clone(),
                    account_index: credentials.account_index,
                    market_id: market.market_id,
                    target,
                };
                let request = CancelOrderRequest {
                    address: credentials.address.clone(),
                    market_id: market.market_id,
                    account_index: credentials.account_index,
                    order_id: order_id.clone(),
                    client_id: cancel_client_id,
                    timestamp,
                };
                match client.cancel_order(&cancel, &request).await {
                    Ok(_) => {
                        if let Some(quote) = resting_quotes.lock().get_mut(&strategy_client_id) {
                            quote.last_action = std::time::Instant::now();
                        }
                        info!(
                            "Arcus cancel accepted for {strategy_client_id}: {}",
                            signal.reason
                        );
                    }
                    Err(cancel_error) => {
                        let orders = client
                            .open_orders(&credentials.address, credentials.account_index)
                            .await
                            .context("cancel failed and Arcus open-order reconciliation failed")?;
                        let still_live = orders.iter().any(|order| {
                            order.client_id.as_deref() == Some(quote.client_id.as_str())
                                || order_id.as_deref().is_some_and(|id| order.order_id == id)
                        });
                        if still_live {
                            anyhow::bail!(
                                "cancel {strategy_client_id} failed while order remains live: {cancel_error}"
                            );
                        }
                        resting_quotes.lock().remove(&strategy_client_id);
                        debug!(
                            "cancel {strategy_client_id} failed because order is already terminal"
                        );
                    }
                }
                continue;
            }

            {
                let dashboard = dash_state.read().await;
                if signal.risk_reducing {
                    let Some(position) = dashboard.positions.iter().find(|position| {
                        position.get("symbol").and_then(|v| v.as_str())
                            == Some(signal.symbol.as_str())
                    }) else {
                        continue;
                    };
                    let held = position
                        .get("size")
                        .and_then(|value| value.as_f64())
                        .unwrap_or(0.0);
                    let held_side = position
                        .get("side")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    let closes_position = matches!(
                        (held_side, signal.side),
                        ("Buy", lighter::types::Side::Sell) | ("Sell", lighter::types::Side::Buy)
                    );
                    if held <= 0.0 || !closes_position {
                        error!(
                            "❌ Invalid risk-reducing signal rejected: {} {:?} vs held side {}",
                            signal.symbol, signal.side, held_side
                        );
                        continue;
                    }
                    signal.quantity = signal.quantity.min(held);
                }
            }
            let values = market.quantize_order(signal.price, signal.quantity, side)?;
            let quote_price: f64 = values.price.parse().unwrap_or(signal.price);
            let quote_qty: f64 = values.quantity.parse().unwrap_or(signal.quantity);
            let good_til_time_us =
                (Utc::now() + chrono::Duration::days(32)).timestamp_micros() as u64;

            let replace = {
                let resting = resting_quotes.lock();
                match resting.get(&strategy_client_id) {
                    Some(quote) if quote.price == quote_price => {
                        debug!(
                            "quote {strategy_client_id} already resting at {}; skip",
                            values.price
                        );
                        continue;
                    }
                    Some(quote) if quote.last_action.elapsed() < requote_cooldown => {
                        debug!(
                            "quote {strategy_client_id} within requote cooldown; keep old price {}",
                            quote.price
                        );
                        continue;
                    }
                    Some(quote) => Some(quote.clone()),
                    None => None,
                }
            };
            if let Some(quote) = replace {
                let (target, order_id, cancel_client_id) =
                    if let Some(order_id) = quote.order_id.clone() {
                        (
                            CancelTarget::OrderId(order_id.clone()),
                            Some(order_id),
                            None,
                        )
                    } else {
                        (
                            CancelTarget::ClientId(quote.client_id.clone()),
                            None,
                            Some(quote.client_id.clone()),
                        )
                    };
                let cancel = CancelOrder {
                    address: credentials.address.clone(),
                    account_index: credentials.account_index,
                    market_id: market.market_id,
                    target,
                };
                let cancel_request = CancelOrderRequest {
                    address: credentials.address.clone(),
                    market_id: market.market_id,
                    account_index: credentials.account_index,
                    order_id: order_id.clone(),
                    client_id: cancel_client_id,
                    timestamp,
                };
                match client.cancel_order(&cancel, &cancel_request).await {
                    Ok(_) => {
                        if let Some(quote) = resting_quotes.lock().get_mut(&strategy_client_id) {
                            quote.last_action = std::time::Instant::now();
                        }
                        debug!(
                            "cancel accepted for stale quote {strategy_client_id}; awaiting terminal state"
                        );
                    }
                    Err(cancel_error) => {
                        let orders = client
                            .open_orders(&credentials.address, credentials.account_index)
                            .await
                            .context(
                                "replace cancel failed and open-order reconciliation failed",
                            )?;
                        let still_live = orders.iter().any(|order| {
                            order.client_id.as_deref() == Some(quote.client_id.as_str())
                                || order_id.as_deref().is_some_and(|id| order.order_id == id)
                        });
                        if still_live {
                            anyhow::bail!(
                                "replace cancel {strategy_client_id} failed while order remains live: {cancel_error}"
                            );
                        }
                        resting_quotes.lock().remove(&strategy_client_id);
                    }
                }
                // Await terminal confirmation before placing the replacement.
                continue;
            }

            if !arcus_quote_attempt_ready(
                failed_quote_attempts.get(&strategy_client_id).copied(),
                requote_cooldown,
                std::time::Instant::now(),
            ) {
                debug!("quote {strategy_client_id} rejected recently; waiting for retry cooldown");
                continue;
            }

            if !signal.risk_reducing {
                let reconciled_open_orders = dash_state.read().await.open_orders;
                let local_resting_quotes = resting_quotes.lock().len();
                if arcus_open_order_limit_reached(
                    reconciled_open_orders,
                    local_resting_quotes,
                    max_open_orders,
                ) {
                    warn!(
                        "Arcus max_open_orders reached (REST={}, local={}, max={}); skipping new entry",
                        reconciled_open_orders, local_resting_quotes, max_open_orders
                    );
                    continue;
                }
            }

            let exposure = {
                let dashboard = dash_state.read().await;
                // symbol -> (signed position, resting buys, resting sells), all in notional USD.
                let mut by_symbol: HashMap<String, (f64, f64, f64)> = HashMap::new();
                for position in &dashboard.positions {
                    let Some(symbol) = position.get("symbol").and_then(|value| value.as_str())
                    else {
                        continue;
                    };
                    let size = position
                        .get("size")
                        .and_then(|value| value.as_f64())
                        .unwrap_or(0.0);
                    let mark = position
                        .get("mark_price")
                        .and_then(|value| value.as_f64())
                        .unwrap_or(0.0);
                    let signed_notional =
                        if position.get("side").and_then(|value| value.as_str()) == Some("Sell") {
                            -(size * mark).abs()
                        } else {
                            (size * mark).abs()
                        };
                    by_symbol.entry(symbol.to_string()).or_default().0 = signed_notional;
                }
                for order in &dashboard.open_orders_list {
                    let Some(symbol) = order.get("symbol").and_then(|value| value.as_str()) else {
                        continue;
                    };
                    let notional = (order
                        .get("price")
                        .and_then(|value| value.as_f64())
                        .unwrap_or(0.0)
                        * order
                            .get("quantity")
                            .and_then(|value| value.as_f64())
                            .unwrap_or(0.0))
                    .abs();
                    let entry = by_symbol.entry(symbol.to_string()).or_default();
                    if order.get("side").and_then(|value| value.as_str()) == Some("Sell") {
                        entry.2 += notional;
                    } else {
                        entry.1 += notional;
                    }
                }
                let total_worst_case_notional = by_symbol
                    .values()
                    .map(|(position, buys, sells)| {
                        risk::risk_manager::worst_case_symbol_notional(*position, *buys, *sells)
                    })
                    .sum();
                let (symbol_position_notional, symbol_buy_open_notional, symbol_sell_open_notional) =
                    by_symbol.get(&signal.symbol).copied().unwrap_or_default();
                risk::risk_manager::RiskExposure {
                    symbol_position_notional,
                    symbol_buy_open_notional,
                    symbol_sell_open_notional,
                    total_worst_case_notional,
                }
            };
            if !risk_manager
                .lock()
                .await
                .check_signal_with_exposure(&signal, exposure)
                .await?
            {
                continue;
            }

            // Maker quotes rest as ALO. Explicit risk reductions are
            // marketable IOC limits so a stale exit cannot become new exposure.
            let tif = if signal.post_only {
                TimeInForce::Alo
            } else if signal.risk_reducing {
                TimeInForce::Ioc
            } else {
                TimeInForce::Gtt
            };
            // Arcus rejects any client ID reused after an earlier order reaches terminal state.
            // Keep the strategy key stable locally, but sign every exchange attempt uniquely.
            let exchange_client_id = arcus_exchange_client_id(&strategy_client_id, timestamp);
            let signed = PlaceOrder {
                address: credentials.address.clone(),
                account_index: credentials.account_index,
                market_id: market.market_id,
                side,
                price_ticks: values.price_ticks,
                quantity_quantums: values.quantity_quantums,
                good_til_time_ns: good_til_time_us * 1_000,
                time_in_force: tif,
                reduce_only: signal.risk_reducing,
                client_id: Some(exchange_client_id.clone()),
            };
            let request = PlaceOrderRequest {
                address: credentials.address.clone(),
                market_id: market.market_id,
                account_index: credentials.account_index,
                order_side: side,
                order_type: "LIMIT".into(),
                quantity: values.quantity,
                price: values.price,
                time_in_force: tif,
                good_til_time: good_til_time_us.to_string(),
                timestamp,
                client_id: Some(exchange_client_id.clone()),
                reduce_only: signal.risk_reducing,
            };
            match client.place_order(&signed, &request).await {
                Ok(ack) => {
                    let status = ack.status.to_ascii_uppercase();
                    if matches!(
                        status.as_str(),
                        "REJECTED" | "FILLED" | "CANCELED" | "MARGIN_CANCELED" | "TPSL_CANCELED"
                    ) {
                        resting_quotes.lock().remove(&strategy_client_id);
                        if status == "REJECTED" {
                            failed_quote_attempts
                                .insert(strategy_client_id.clone(), std::time::Instant::now());
                            error!(
                                "❌ Arcus order rejected: client_id={exchange_client_id}, order_id={}, reason={}",
                                ack.order_id,
                                ack.rejection_reason.as_deref().unwrap_or("unknown")
                            );
                        } else {
                            debug!(
                                "Arcus order reached terminal status {status}: {}",
                                ack.order_id
                            );
                        }
                    } else {
                        failed_quote_attempts.remove(&strategy_client_id);
                        resting_quotes.lock().insert(
                            strategy_client_id.clone(),
                            RestingQuote {
                                client_id: exchange_client_id.clone(),
                                order_id: Some(ack.order_id.clone()),
                                symbol: market.symbol.clone(),
                                side,
                                price: quote_price,
                                quantity: quote_qty,
                                last_action: std::time::Instant::now(),
                            },
                        );
                        debug!(
                            "Arcus order acknowledged: order_id={}, status={status}",
                            ack.order_id
                        );
                    }
                    // A 200/202 response is only an acknowledgement. The
                    // orders/userFills channels provide definitive lifecycle.
                }
                Err(error) => {
                    failed_quote_attempts
                        .insert(strategy_client_id.clone(), std::time::Instant::now());
                    error!(
                        "❌ Arcus order request failed: client_id={exchange_client_id}, post_only={}, error={error}",
                        signal.post_only
                    );
                }
            }
        }
    }
}

fn arcus_dashboard_positions(
    positions: &[multi_venue_quant_bot::arcus::MarketPosition],
) -> Vec<serde_json::Value> {
    positions
        .iter()
        .filter_map(|position| {
            let signed_size = position.signed_size().ok()?;
            let size = signed_size.abs();
            let entry_price = position.entry_price.parse::<f64>().ok()?;
            let unrealized_pnl = position.unrealized_pnl.parse::<f64>().ok()?;
            let mark_price = if size > 1e-12 {
                if signed_size >= 0.0 {
                    entry_price + unrealized_pnl / size
                } else {
                    entry_price - unrealized_pnl / size
                }
            } else {
                entry_price
            };
            Some(serde_json::json!({
                "symbol": position.market_display_name,
                "side": if signed_size >= 0.0 { "Buy" } else { "Sell" },
                "size": size,
                "entry_price": entry_price,
                "mark_price": mark_price,
                "unrealized_pnl": unrealized_pnl,
            }))
        })
        .collect()
}

struct BacktestCliOpts {
    output: Option<String>,
    params: Option<String>,
    config: Option<String>,
    maker: bool,
}

async fn run_backtest(
    strategy_name: &str,
    data_path: &str,
    start_date: &str,
    end_date: &str,
    initial_capital: f64,
    opts: BacktestCliOpts,
) -> Result<()> {
    info!("📊 Starting backtest: {}", strategy_name);
    info!("   Data: {}", data_path);
    info!("   Period: {} to {}", start_date, end_date);
    info!("   Capital: ${:.2}", initial_capital);
    if let Some(p) = &opts.params {
        info!("   Params: {}", p);
    }

    let historical_data = data::loader::load_csv_data_in_range(data_path, start_date, end_date)
        .context("Failed to load historical data")?;

    let mut backtest_engine =
        backtest::engine::BacktestEngine::new(initial_capital, historical_data);

    // 收益门槛 parity：--config 指定与实盘相同的 yaml 时，回测也拒绝净收益不足的入场
    if let Some(cfg) = &opts.config {
        let settings = Config::builder()
            .add_source(config::File::with_name(cfg))
            .build()
            .context("Failed to load config")?;
        let guard = risk::profitability::ProfitabilityGuard::from_config(&settings)?;
        let cost_bps = guard.total_cost_bps();
        backtest_engine = backtest_engine.with_profitability(guard);
        info!(
            "🧮 Profitability gate enabled (total cost {:.2} bps)",
            cost_bps
        );
    }

    // maker 填充模型：post_only 信号只在K线区间穿越报价价时成交、无滑点；
    // 费率从 --config 的 profitability 段接线（maker 经济学，如 entry/exit 0 bps）。
    if opts.maker {
        backtest_engine = backtest_engine.with_maker_fills(true);
        if let Some(cfg) = &opts.config {
            let settings = Config::builder()
                .add_source(config::File::with_name(cfg))
                .build()
                .context("Failed to load config")?;
            let maker_fee_bps = settings
                .get_float("profitability.entry_fee_bps")
                .unwrap_or(0.0);
            let taker_fee_bps = settings
                .get_float("profitability.exit_fee_bps")
                .unwrap_or(2.25);
            let taker_slippage_bps = settings
                .get_float("profitability.exit_slippage_bps")
                .unwrap_or(0.0);
            let fill_ratio = settings
                .get_float("profitability.maker_fill_ratio")
                .unwrap_or(0.5);
            let min_penetration_bps = settings
                .get_float("profitability.maker_min_penetration_bps")
                .unwrap_or(2.0);
            let adverse_selection_bps = settings
                .get_float("profitability.adverse_selection_bps")
                .unwrap_or(1.0);
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
            backtest_engine = backtest_engine
                .with_execution_costs(
                    maker_fee_bps / 10_000.0,
                    taker_fee_bps / 10_000.0,
                    taker_slippage_bps / 10_000.0,
                )?
                .with_conservative_maker_model(
                    fill_ratio,
                    min_penetration_bps,
                    adverse_selection_bps,
                )?
                .with_position_risk(stop_loss, take_profit)?
                .with_max_position_notional(max_position_notional)?
                .with_max_total_notional_pct(max_total_notional_pct)?;
            info!(
                "🛠 Maker model: maker/taker fee {:.2}/{:.2} bps, fill {:.0}%, penetration {:.2} bps, adverse {:.2} bps, stop/take {:.1}/{:.1}%, symbol/account cap ${:.2}/{:.1}%",
                maker_fee_bps,
                taker_fee_bps,
                fill_ratio * 100.0,
                min_penetration_bps,
                adverse_selection_bps,
                stop_loss * 100.0,
                take_profit * 100.0,
                max_position_notional,
                max_total_notional_pct * 100.0,
            );
        } else {
            info!(
                "🛠 Maker fill model: fills at quote price when candle crosses; default commission {:.4}%",
                backtest_engine.commission_rate() * 100.0
            );
        }
    }

    let bt_strategy = strategy::create_strategy_with_params(strategy_name, opts.params.as_deref())?;
    let results = backtest_engine.run(bt_strategy).await?;

    let output_path = opts.output.as_deref().unwrap_or("backtests/results");
    backtest::metrics::generate_report(&results, output_path).await?;

    info!("📈 Backtest complete!");
    info!("   Return: {:.2}%", results.total_return * 100.0);
    info!("   Sharpe: {:.3}", results.sharpe_ratio);
    info!("   Max DD: {:.2}%", results.max_drawdown * 100.0);
    info!("   Trades: {}", results.trades.len());
    info!("   Win Rate: {:.1}%", results.win_rate * 100.0);
    info!(
        "   Blocked by profitability gate: {}",
        results.blocked_by_profitability
    );

    Ok(())
}

/// Run parameter optimization sweep across grid strategy parameters
async fn run_optimize(
    strategy_name: &str,
    data_path: &str,
    start_date: &str,
    end_date: &str,
    initial_capital: f64,
    output_dir: Option<&str>,
    config_path: Option<&str>,
) -> Result<()> {
    info!("🔬 Starting parameter optimization for: {}", strategy_name);

    let historical_data = data::loader::load_csv_data_in_range(data_path, start_date, end_date)
        .context("Failed to load historical data")?;
    info!("   Loaded {} candles", historical_data.len());

    // 收益门槛 parity：--config 指定与实盘相同的 yaml 时，参数扫描也拒绝净收益不足的入场
    let profitability = if let Some(cfg) = config_path {
        let settings = Config::builder()
            .add_source(config::File::with_name(cfg))
            .build()
            .context("Failed to load config")?;
        let guard = risk::profitability::ProfitabilityGuard::from_config(&settings)?;
        let cost_bps = guard.total_cost_bps();
        info!(
            "🧮 Profitability gate enabled (total cost {:.2} bps)",
            cost_bps
        );
        Some(guard)
    } else {
        None
    };

    // Define parameter grid based on strategy type
    let param_sets: Vec<String> = match strategy_name {
        "grid_trading" | "grid" => {
            let grid_counts = [6, 8, 10, 14, 20];
            let investments = [5.0, 8.0, 12.0, 16.0];
            let deviations = [0.003, 0.005, 0.008, 0.012, 0.02];
            let mut sets = Vec::new();
            for &gc in &grid_counts {
                for &inv in &investments {
                    for &dev in &deviations {
                        sets.push(format!(
                            "grid_count={},investment={},deviation={}",
                            gc, inv, dev
                        ));
                    }
                }
            }
            sets
        }
        "trend_following" | "trend" => {
            let fast_periods = [5, 7, 10, 14];
            let slow_periods = [14, 21, 30, 50];
            let stop_losses = [0.02, 0.03, 0.05];
            let take_profits = [0.04, 0.06, 0.10];
            let trailing_stops = [0.0, 0.015, 0.025];
            let mut sets = Vec::new();
            for &f in &fast_periods {
                for &s in &slow_periods {
                    if f >= s {
                        continue;
                    }
                    for &sl in &stop_losses {
                        for &tp in &take_profits {
                            if tp <= sl {
                                continue;
                            }
                            for &tr in &trailing_stops {
                                sets.push(format!(
                                    "fast_ma={},slow_ma={},stop_loss={},take_profit={},trailing_stop={}",
                                    f, s, sl, tp, tr
                                ));
                            }
                        }
                    }
                }
            }
            sets
        }
        _ => anyhow::bail!("未知策略: {}", strategy_name),
    };

    info!("   Testing {} parameter combinations...", param_sets.len());

    struct OptResult {
        params: String,
        total_return: f64,
        sharpe: f64,
        max_dd: f64,
        trades: usize,
        win_rate: f64,
        profit_factor: f64,
    }

    let mut results_vec: Vec<OptResult> = Vec::new();

    for (i, params) in param_sets.iter().enumerate() {
        let bt_strategy = strategy::create_strategy_with_params(strategy_name, Some(params))?;
        let mut engine =
            backtest::engine::BacktestEngine::new(initial_capital, historical_data.clone());
        if let Some(guard) = &profitability {
            engine = engine.with_profitability(guard.clone());
        }
        let result = engine.run(bt_strategy).await?;

        results_vec.push(OptResult {
            params: params.clone(),
            total_return: result.total_return,
            sharpe: result.sharpe_ratio,
            max_dd: result.max_drawdown,
            trades: result.total_trades,
            win_rate: result.win_rate,
            profit_factor: result.profit_factor,
        });

        if (i + 1) % 20 == 0 {
            info!("   Progress: {}/{}", i + 1, param_sets.len());
        }
    }

    // Sort by Sharpe ratio descending (highest = best risk-adjusted performance)
    results_vec.sort_by(|a, b| {
        let score_a = if a.trades > 0 {
            a.sharpe
        } else {
            f64::NEG_INFINITY
        };
        let score_b = if b.trades > 0 {
            b.sharpe
        } else {
            f64::NEG_INFINITY
        };
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Print top 10 results
    info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    info!("🏆 TOP 10 PARAMETER COMBINATIONS");
    info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    for (rank, r) in results_vec.iter().take(10).enumerate() {
        info!(
            "#{:2} Return: {:+6.2}% | Sharpe: {:6.3} | MaxDD: {:5.2}% | Trades: {:4} | WinRate: {:5.1}% | PF: {:5.2} | {}",
            rank + 1, r.total_return * 100.0, r.sharpe, r.max_dd * 100.0,
            r.trades, r.win_rate * 100.0, r.profit_factor, r.params
        );
    }
    info!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // Save full results to CSV
    let output_path = output_dir.unwrap_or("backtests/results");
    let opt_dir = format!("{}/optimization", output_path);
    std::fs::create_dir_all(&opt_dir)?;
    let csv_path = format!("{}/sweep_results.csv", opt_dir);
    let mut csv = String::from(
        "rank,params,return_pct,sharpe,max_dd_pct,trades,win_rate_pct,profit_factor\n",
    );
    for (i, r) in results_vec.iter().enumerate() {
        csv.push_str(&format!(
            "{},\"{}\",{:.4},{:.4},{:.4},{},{:.2},{:.4}\n",
            i + 1,
            r.params,
            r.total_return * 100.0,
            r.sharpe,
            r.max_dd * 100.0,
            r.trades,
            r.win_rate * 100.0,
            r.profit_factor
        ));
    }
    std::fs::write(&csv_path, csv)?;
    info!("📄 Full results saved to: {}", csv_path);

    // Run the best params and generate detailed report
    if let Some(best) = results_vec.first() {
        info!(
            "\n🥇 Running detailed backtest with best params: {}",
            best.params
        );
        let bt_strategy = strategy::create_strategy_with_params(strategy_name, Some(&best.params))?;
        let mut engine = backtest::engine::BacktestEngine::new(initial_capital, historical_data);
        if let Some(guard) = &profitability {
            engine = engine.with_profitability(guard.clone());
        }
        let result = engine.run(bt_strategy).await?;
        let best_dir = format!("{}/best", opt_dir);
        backtest::metrics::generate_report(&result, &best_dir).await?;
        info!("📊 Best-params detailed report saved to: {}", best_dir);
    }

    Ok(())
}

async fn run_dashboard(host: &str, port: u16) -> Result<()> {
    info!("🌐 Starting dashboard at {}:{}", host, port);
    dashboard::server::start(host, port)
        .await
        .context("Dashboard failed")
}

async fn download_data(
    symbol: &str,
    interval: &str,
    start_date: &str,
    end_date: &str,
    base_url: &str,
    tag: Option<&str>,
) -> Result<()> {
    info!(
        "📥 Download data: {} {} {} {} ({})",
        symbol, interval, start_date, end_date, base_url
    );

    let start = data::loader::parse_range_start(start_date).context("Invalid start date")?;
    let end = match data::loader::parse_range_end(end_date).context("Invalid end date")? {
        data::loader::RangeEnd::Inclusive(dt) => dt,
        data::loader::RangeEnd::Exclusive(dt) => dt - chrono::Duration::seconds(1),
    };

    let secs_per_candle = match interval {
        "1m" => 60,
        "5m" => 300,
        "15m" => 900,
        "1h" => 3600,
        "4h" => 14400,
        "1d" => 86400,
        other => anyhow::bail!("Unsupported interval: {}", other),
    };

    let client = lighter::client::LighterClient::new("", "", base_url, "");

    // 从交易所动态解析 symbol -> market_id（优先 perp 市场）
    let all_markets = client
        .get_all_markets()
        .await
        .context("Failed to fetch market list")?;
    lighter::symbols::register_all(all_markets.iter().map(|m| (m.market_id, m.symbol.clone())));
    let upper = symbol.to_ascii_uppercase();
    let market_id = all_markets
        .iter()
        .filter(|m| m.symbol.to_ascii_uppercase() == upper)
        .min_by_key(|m| if m.market_type == "perp" { 0 } else { 1 })
        .map(|m| m.market_id)
        .ok_or_else(|| {
            let mut available: Vec<&str> = all_markets.iter().map(|m| m.symbol.as_str()).collect();
            available.sort();
            anyhow::anyhow!(
                "Symbol {} not found. Available: {}",
                symbol,
                available.join(", ")
            )
        })?;
    info!("   Resolved {} -> market_id {}", symbol, market_id);

    // API 单次最多返回 500 根，按窗口分页拉取
    const MAX_CANDLES_PER_REQ: i64 = 500;
    let chunk_secs = secs_per_candle as i64 * MAX_CANDLES_PER_REQ;
    let mut candles: Vec<lighter::types::Candlestick> = Vec::new();
    let mut chunk_start = start.timestamp();
    let end_ts = end.timestamp();
    while chunk_start <= end_ts {
        let chunk_end = (chunk_start + chunk_secs - secs_per_candle as i64).min(end_ts);
        let batch = client
            .get_candlesticks_in_range(
                market_id,
                interval,
                chunk_start,
                chunk_end,
                MAX_CANDLES_PER_REQ as u32,
            )
            .await
            .context("Failed to download candlestick data")?;
        info!(
            "   Chunk {} -> {}: {} candles",
            chrono::DateTime::from_timestamp(chunk_start, 0).unwrap(),
            chrono::DateTime::from_timestamp(chunk_end, 0).unwrap(),
            batch.len()
        );
        candles.extend(batch);
        chunk_start = chunk_end + secs_per_candle as i64;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    candles.retain(|c| c.timestamp >= start && c.timestamp <= end);
    candles.sort_by_key(|c| c.timestamp);
    candles.dedup_by_key(|c| c.timestamp);
    if candles.is_empty() {
        anyhow::bail!(
            "No candles returned for {} {} {} {}",
            symbol,
            interval,
            start_date,
            end_date
        );
    }

    let network_tag = tag.map(|t| t.to_string()).unwrap_or_else(|| {
        if base_url.contains("rh.lighter") {
            "rh".to_string()
        } else {
            "mainnet".to_string()
        }
    });
    let output_path = format!(
        "backtests/data/{}-{}-{}-{}-{}.csv",
        symbol.to_ascii_uppercase(),
        network_tag,
        interval,
        start.format("%Y%m%d"),
        end.with_timezone(&Utc).format("%Y%m%d")
    );
    data::loader::write_csv_data(&output_path, &candles)
        .context("Failed to write downloaded data")?;

    info!(
        "✅ Data download complete: {} ({} candles)",
        output_path,
        candles.len()
    );
    Ok(())
}

/// 下载 Arcus 历史蜡烛（多市场可合并到一个 CSV，按 symbol 列区分）。
/// 走公开 /v1/candles，用 `to` 游标向前分页（单次最多 1500 根）。
async fn download_arcus_data(
    symbols: &str,
    interval: &str,
    start_date: &str,
    end_date: &str,
    env: &str,
    tag: Option<&str>,
) -> Result<()> {
    let environment = match env.to_ascii_lowercase().as_str() {
        "mainnet" => multi_venue_quant_bot::arcus::ArcusEnvironment::Mainnet,
        "testnet" => multi_venue_quant_bot::arcus::ArcusEnvironment::Testnet,
        other => anyhow::bail!("未知 env: {other}（mainnet | testnet）"),
    };
    let client = multi_venue_quant_bot::arcus::ArcusClient::public(environment);

    let start = data::loader::parse_range_start(start_date).context("Invalid start date")?;
    let end = match data::loader::parse_range_end(end_date).context("Invalid end date")? {
        data::loader::RangeEnd::Inclusive(dt) => dt,
        data::loader::RangeEnd::Exclusive(dt) => dt - chrono::Duration::seconds(1),
    };
    if end <= start {
        anyhow::bail!("end 必须晚于 start");
    }

    let secs_per_candle = match interval {
        "1m" => 60,
        "3m" => 180,
        "5m" => 300,
        "15m" => 900,
        "30m" => 1800,
        "1h" => 3600,
        "2h" => 7200,
        "4h" => 14400,
        "1d" => 86400,
        other => anyhow::bail!("Unsupported interval: {}", other),
    };

    const PAGE: u16 = 1500;
    let start_ts = start.timestamp();
    let end_ts = end.timestamp();
    let mut all: Vec<lighter::types::Candlestick> = Vec::new();

    for raw in symbols.split(',') {
        let market = raw.trim();
        if market.is_empty() {
            continue;
        }
        info!(
            "📥 Arcus download: {} {} {} -> {}",
            market, interval, start_date, end_date
        );
        let mut to_micros = (end_ts + secs_per_candle as i64) * 1_000_000; // 先多取一根再过滤
        let mut pages = 0;
        loop {
            let page = client
                .candles_before(market, interval, PAGE, to_micros)
                .await
                .map_err(|e| anyhow::anyhow!("{} candles 拉取失败: {}", market, e))?;
            pages += 1;
            info!("   页 {pages}: {} 根（to={}）", page.len(), to_micros);
            if page.is_empty() {
                break;
            }
            let oldest = page.iter().map(|c| c.open_time).min().unwrap_or(to_micros);
            for candle in page {
                let Some(timestamp) =
                    chrono::DateTime::<Utc>::from_timestamp_micros(candle.open_time)
                else {
                    continue;
                };
                let ts = timestamp.timestamp();
                if ts < start_ts || ts > end_ts {
                    continue;
                }
                let parsed = || -> Option<lighter::types::Candlestick> {
                    Some(lighter::types::Candlestick {
                        timestamp,
                        open: candle.open.parse().ok()?,
                        high: candle.high.parse().ok()?,
                        low: candle.low.parse().ok()?,
                        close: candle.close.parse().ok()?,
                        volume: candle.volume.parse().ok()?,
                        symbol: candle.market_display_name.clone(),
                    })
                };
                if let Some(c) = parsed() {
                    all.push(c);
                }
            }
            // 下一页从最老一根之前开始
            to_micros = oldest - 1;
            if to_micros < start_ts * 1_000_000 || pages >= 200 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
    }

    all.sort_by_key(|c| c.timestamp);
    all.dedup_by_key(|c| (c.symbol.clone(), c.timestamp));
    if all.is_empty() {
        anyhow::bail!(
            "No Arcus candles returned for {} {} -> {}",
            symbols,
            start_date,
            end_date
        );
    }

    let net_tag = tag
        .map(|t| t.to_string())
        .unwrap_or_else(|| "arcus".to_string());
    let label = if symbols.contains(',') {
        "multi".to_string()
    } else {
        symbols.trim().to_ascii_uppercase()
    };
    let output_path = format!(
        "backtests/data/{}-{}-{}-{}-{}.csv",
        label,
        net_tag,
        interval,
        start.format("%Y%m%d"),
        end.with_timezone(&Utc).format("%Y%m%d")
    );
    data::loader::write_csv_data(&output_path, &all).context("Failed to write Arcus data")?;
    info!(
        "✅ Arcus data download complete: {} ({} candles, {} markets)",
        output_path,
        all.len(),
        symbols.split(',').filter(|s| !s.trim().is_empty()).count()
    );
    Ok(())
}

fn aster_interval_millis(interval: &str) -> Result<u64> {
    let seconds = match interval {
        "1m" => 60,
        "3m" => 180,
        "5m" => 300,
        "15m" => 900,
        "30m" => 1_800,
        "1h" => 3_600,
        "2h" => 7_200,
        "4h" => 14_400,
        "6h" => 21_600,
        "8h" => 28_800,
        "12h" => 43_200,
        "1d" => 86_400,
        other => anyhow::bail!("Unsupported Aster interval: {other}"),
    };
    Ok(seconds * 1_000)
}

/// 下载 Aster Pro Futures V3 历史蜡烛。公开接口不需要 API Wallet 凭据。
async fn download_aster_data(
    symbols: &str,
    interval: &str,
    start_date: &str,
    end_date: &str,
    url: &str,
    tag: Option<&str>,
) -> Result<()> {
    let interval_ms = aster_interval_millis(interval)?;
    let start = data::loader::parse_range_start(start_date).context("Invalid start date")?;
    let end_exclusive = match data::loader::parse_range_end(end_date).context("Invalid end date")? {
        data::loader::RangeEnd::Inclusive(dt) => dt + chrono::Duration::milliseconds(1),
        data::loader::RangeEnd::Exclusive(dt) => dt,
    };
    if end_exclusive <= start {
        anyhow::bail!("end 必须晚于 start");
    }
    let start_ms = u64::try_from(start.timestamp_millis()).context("start 日期超出范围")?;
    let end_exclusive_ms =
        u64::try_from(end_exclusive.timestamp_millis()).context("end 日期超出范围")?;
    let client = multi_venue_quant_bot::aster::AsterClient::with_base_url(url);
    let mut requested_symbols: Vec<String> = symbols
        .split(',')
        .map(|symbol| symbol.trim().to_ascii_uppercase())
        .filter(|symbol| !symbol.is_empty())
        .collect();
    requested_symbols.sort();
    requested_symbols.dedup();
    if requested_symbols.is_empty() {
        anyhow::bail!("至少需要一个 Aster symbol");
    }
    if requested_symbols
        .iter()
        .any(|symbol| symbol.len() > 32 || !symbol.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    {
        anyhow::bail!("Aster symbol 只能包含 ASCII 字母和数字，且最长 32 字符");
    }

    const PAGE: u16 = 1_000;
    let mut all = Vec::<lighter::types::Candlestick>::new();
    for symbol in &requested_symbols {
        info!(
            "📥 Aster download: {} {} {} -> {}",
            symbol, interval, start_date, end_date
        );
        let mut cursor = start_ms;
        let mut pages = 0_u32;
        while cursor < end_exclusive_ms {
            let page = client
                .klines(
                    symbol,
                    interval,
                    Some(cursor),
                    Some(end_exclusive_ms.saturating_sub(1)),
                    Some(PAGE),
                )
                .await
                .map_err(|error| anyhow::anyhow!("Aster {symbol} candles 拉取失败: {error}"))?;
            pages += 1;
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            let newest = page
                .iter()
                .map(|candle| candle.open_time)
                .max()
                .context("Aster kline page unexpectedly empty")?;
            for candle in page {
                if candle.open_time < start_ms || candle.open_time >= end_exclusive_ms {
                    continue;
                }
                let timestamp =
                    chrono::DateTime::<Utc>::from_timestamp_millis(candle.open_time as i64)
                        .context("Aster kline timestamp is out of range")?;
                all.push(lighter::types::Candlestick {
                    timestamp,
                    open: candle.open.parse().context("invalid Aster open")?,
                    high: candle.high.parse().context("invalid Aster high")?,
                    low: candle.low.parse().context("invalid Aster low")?,
                    close: candle.close.parse().context("invalid Aster close")?,
                    volume: candle.volume.parse().context("invalid Aster volume")?,
                    symbol: symbol.clone(),
                });
            }
            let next = newest.saturating_add(1);
            if next <= cursor {
                anyhow::bail!("Aster kline pagination cursor did not advance");
            }
            cursor = next;
            if page_len < usize::from(PAGE) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        info!("   Aster {symbol}: {pages} pages");
    }
    all.sort_by_key(|candle| (candle.symbol.clone(), candle.timestamp));
    all.dedup_by_key(|candle| (candle.symbol.clone(), candle.timestamp));
    if all.is_empty() {
        anyhow::bail!(
            "No Aster candles returned for {} {} -> {}",
            symbols,
            start_date,
            end_date
        );
    }
    let label = if requested_symbols.len() == 1 {
        requested_symbols[0].clone()
    } else {
        "multi".to_string()
    };
    let output_path = format!(
        "backtests/data/{}-{}-{}-{}-{}.csv",
        label,
        tag.unwrap_or("aster"),
        interval,
        start.format("%Y%m%d"),
        (end_exclusive - chrono::Duration::milliseconds(1)).format("%Y%m%d")
    );
    data::loader::write_csv_data(&output_path, &all).context("Failed to write Aster data")?;
    info!(
        "✅ Aster data download complete: {} ({} candles, {} markets, interval={}ms)",
        output_path,
        all.len(),
        requested_symbols.len(),
        interval_ms
    );
    Ok(())
}

async fn generate_test_data(symbol: &str, days: u32) -> Result<()> {
    info!("🎲 Generating test data: {} {}d", symbol, days);
    data::loader::generate_synthetic_data(symbol, days).context("Failed to generate test data")?;
    info!("✅ Test data generated");
    Ok(())
}

#[cfg(test)]
mod arcus_open_order_limit_tests {
    use super::{
        arcus_exchange_client_id, arcus_maker_quote_key, arcus_open_order_limit_reached,
        arcus_quote_attempt_ready, arcus_resting_quote_is_live, arcus_signal_allowed_while_paused,
    };
    use multi_venue_quant_bot::arcus::OrderSide as ArcusSide;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    #[test]
    fn blocks_when_local_acknowledgements_reach_the_limit_before_rest_refresh() {
        assert!(arcus_open_order_limit_reached(12, 16, 16));
    }

    #[test]
    fn blocks_when_reconciled_exchange_count_reaches_the_limit() {
        assert!(arcus_open_order_limit_reached(16, 0, 16));
    }

    #[test]
    fn permits_an_order_while_both_counts_are_below_the_limit() {
        assert!(!arcus_open_order_limit_reached(12, 15, 16));
    }

    #[test]
    fn throttles_a_recent_order_attempt_after_async_rejection() {
        let now = Instant::now();
        assert!(!arcus_quote_attempt_ready(
            Some(now - Duration::from_secs(1)),
            Duration::from_secs(5),
            now,
        ));
    }

    #[test]
    fn retries_after_the_requote_cooldown_expires() {
        let now = Instant::now();
        assert!(arcus_quote_attempt_ready(
            Some(now - Duration::from_secs(5)),
            Duration::from_secs(5),
            now,
        ));
    }

    #[test]
    fn permits_a_quote_without_a_prior_attempt() {
        assert!(arcus_quote_attempt_ready(
            None,
            Duration::from_secs(5),
            Instant::now(),
        ));
    }

    #[test]
    fn exchange_client_ids_are_unique_and_within_arcus_limit() {
        let first = arcus_exchange_client_id("mq_SUPER-LONG-MARKET-NAME_buy", 1);
        let second = arcus_exchange_client_id("mq_SUPER-LONG-MARKET-NAME_buy", 2);

        assert_ne!(first, second);
        assert!(first.starts_with("mq_"));
        assert!(first.len() <= 36);
        assert!(first
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')));
    }

    #[test]
    fn maker_quote_key_is_stable_across_exchange_attempts() {
        assert_eq!(
            arcus_maker_quote_key("USO-USD", ArcusSide::Sell),
            "mq_USO-USD_sell"
        );
    }

    #[test]
    fn keeps_a_resting_quote_when_rest_omits_client_id_but_order_id_matches() {
        let live_client_ids = HashSet::new();
        let live_order_ids = HashSet::from(["order-1"]);
        assert!(arcus_resting_quote_is_live(
            "mq_BTC-USD_buy",
            Some("order-1"),
            &live_client_ids,
            &live_order_ids,
        ));
    }

    #[test]
    fn keeps_a_resting_quote_when_client_id_matches() {
        let live_client_ids = HashSet::from(["mq_BTC-USD_buy"]);
        let live_order_ids = HashSet::new();
        assert!(arcus_resting_quote_is_live(
            "mq_BTC-USD_buy",
            None,
            &live_client_ids,
            &live_order_ids,
        ));
    }

    #[test]
    fn removes_a_resting_quote_only_when_neither_identifier_matches() {
        let live_client_ids = HashSet::new();
        let live_order_ids = HashSet::new();
        assert!(!arcus_resting_quote_is_live(
            "mq_BTC-USD_buy",
            Some("order-1"),
            &live_client_ids,
            &live_order_ids,
        ));
    }

    #[test]
    fn pause_allows_only_immediate_reduce_only_exits() {
        assert!(!arcus_signal_allowed_while_paused(true, false, false));
        assert!(!arcus_signal_allowed_while_paused(true, true, true));
        assert!(arcus_signal_allowed_while_paused(true, true, false));
        assert!(arcus_signal_allowed_while_paused(false, false, true));
    }
}
