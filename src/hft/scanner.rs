use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::{plan_subscription_shards, ScanStats};
use crate::lighter::{
    client::LighterClient,
    types::WsMessage,
    websocket::LighterWebSocket,
};

const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 100;
const WS_CLIENT_MESSAGE_INTERVAL: Duration = Duration::from_millis(310);
const EVENT_QUEUE_CAPACITY: usize = 16_384;

pub async fn run_market_scan(
    rest_url: &str,
    ws_url: &str,
    duration_secs: u64,
    top: usize,
    market_type: &str,
) -> Result<()> {
    if duration_secs == 0 {
        bail!("scan duration must be greater than zero");
    }
    if top == 0 {
        bail!("top must be greater than zero");
    }

    let market_filter = market_type.trim().to_ascii_lowercase();
    if !matches!(market_filter.as_str(), "all" | "perp" | "spot") {
        bail!("market-type must be one of: all, perp, spot");
    }

    let client = LighterClient::new_with_account(rest_url, 0, 0);
    let markets = client
        .get_all_markets()
        .await
        .context("failed to discover Lighter markets")?;
    crate::lighter::symbols::register_all(
        markets
            .iter()
            .map(|market| (market.market_id, market.symbol.clone())),
    );

    let mut market_ids = markets
        .iter()
        .filter(|market| market_filter == "all" || market.market_type == market_filter)
        .map(|market| market.market_id)
        .collect::<Vec<_>>();
    market_ids.sort_unstable();
    market_ids.dedup();
    if market_ids.is_empty() {
        bail!("no markets matched market-type={market_filter}");
    }

    let shards = plan_subscription_shards(&market_ids, MAX_SUBSCRIPTIONS_PER_CONNECTION)?;
    info!(
        markets = market_ids.len(),
        connections = shards.len(),
        market_type = market_filter,
        "Starting read-only all-market BBO scan"
    );

    let (event_tx, mut event_rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let setup_tx = event_tx.clone();
    let setup = async {
        let mut connections = Vec::with_capacity(shards.len());
        let mut forwarders = Vec::with_capacity(shards.len());

        for (shard_index, shard) in shards.iter().enumerate() {
            let connection = LighterWebSocket::new(ws_url);
            connection
                .connect()
                .await
                .with_context(|| format!("failed to connect websocket shard {shard_index}"))?;

            let mut receiver = connection.get_receiver();
            let shard_tx = setup_tx.clone();
            forwarders.push(tokio::spawn(async move {
                loop {
                    match receiver.recv().await {
                        Ok(WsMessage::BboUpdate(update)) => {
                            if shard_tx.send(update).await.is_err() {
                                break;
                            }
                        }
                        Ok(WsMessage::Error(message)) => {
                            warn!(shard = shard_index, error = %message, "Lighter websocket error");
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                            warn!(
                                shard = shard_index,
                                dropped = count,
                                "Scanner receiver lagged"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }));

            for market_id in shard {
                connection
                    .subscribe_ticker(*market_id)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to subscribe websocket shard {shard_index} to market {market_id}"
                        )
                    })?;
                tokio::time::sleep(WS_CLIENT_MESSAGE_INTERVAL).await;
            }
            connections.push(connection);
        }

        Ok::<_, anyhow::Error>((connections, forwarders))
    };
    tokio::pin!(setup);

    let mut stats = ScanStats::new();
    let (connections, forwarders) = loop {
        tokio::select! {
            setup_result = &mut setup => break setup_result?,
            maybe_update = event_rx.recv() => {
                if let Some(update) = maybe_update {
                    stats.record(update);
                }
            }
        }
    };
    drop(event_tx);

    while let Ok(update) = event_rx.try_recv() {
        stats.record(update);
    }
    info!(
        warmup_events = stats.summary(Duration::from_secs(1), 1).events,
        "All subscriptions active; resetting warmup statistics"
    );
    stats.reset();

    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(duration_secs);
    let mut report_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(5),
        Duration::from_secs(5),
    );
    report_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_update = event_rx.recv() => {
                match maybe_update {
                    Some(update) => stats.record(update),
                    None => break,
                }
            }
            _ = report_interval.tick() => {
                let summary = stats.summary(started.elapsed(), top);
                info!(
                    events = summary.events,
                    live_markets = summary.live_markets,
                    events_per_second = format_args!("{:.1}", summary.events_per_second),
                    "BBO scan progress"
                );
            }
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }

    for connection in &connections {
        connection.shutdown();
    }
    for forwarder in forwarders {
        forwarder.abort();
    }

    let summary = stats.summary(started.elapsed(), top);
    println!(
        "Scan complete: {} events, {} live markets, {:.1} events/s",
        summary.events, summary.live_markets, summary.events_per_second
    );
    println!("Top current quoted spreads:");
    for quote in summary.top_spreads {
        println!(
            "  {:>5} {:<14} bid={:<14.8} ask={:<14.8} spread={:.2} bps",
            quote.market_id,
            quote.symbol,
            quote.bid_price,
            quote.ask_price,
            quote.spread_bps
        );
    }

    Ok(())
}
