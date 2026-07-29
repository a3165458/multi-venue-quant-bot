//! Shared fixtures for backtest engine accounting/commission/report tests.

use super::*;
use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) fn signal_price(snapshot: &MarketSnapshot) -> (String, f64, DateTime<Utc>) {
    if let Some((symbol, candles)) = snapshot.candles.iter().next() {
        if let Some(last) = candles.last() {
            return (symbol.clone(), last.close, last.timestamp);
        }
    }
    if let Some(ob) = snapshot.order_books.values().next() {
        let close = ob.bids.first().map(|l| l.price / 0.999).unwrap_or(100.0);
        return (ob.symbol.clone(), close, ob.timestamp);
    }
    (
        "BTC".to_string(),
        100.0,
        Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
    )
}

/// Emits a single open signal on the first evaluate call, then holds.
pub(crate) struct HoldAfterOpen {
    pub side: Side,
    pub qty: f64,
    pub fired: AtomicUsize,
}

#[async_trait]
impl Strategy for HoldAfterOpen {
    fn name(&self) -> &str {
        "hold_after_open"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        if self.fired.fetch_add(1, Ordering::SeqCst) > 0 {
            return Ok(None);
        }
        let (symbol, price, ts) = signal_price(snapshot);
        Ok(Some(vec![TradeSignal {
            symbol,
            market_id: 0,
            side: self.side,
            price,
            quantity: self.qty,
            order_type: OrderType::Market,
            reason: "test open".into(),
            timestamp: ts,
            expected_edge_bps: None,
            risk_reducing: false,
        }]))
    }

    fn reset(&mut self) {}
}

/// Open long, then reverse to short on the second signal candle.
pub(crate) struct OpenThenReverse {
    pub qty: f64,
    pub step: AtomicUsize,
}

#[async_trait]
impl Strategy for OpenThenReverse {
    fn name(&self) -> &str {
        "open_then_reverse"
    }

    async fn evaluate(&self, snapshot: &MarketSnapshot) -> Result<Option<Vec<TradeSignal>>> {
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        let (symbol, price, ts) = signal_price(snapshot);

        match step {
            0 => Ok(Some(vec![TradeSignal {
                symbol,
                market_id: 0,
                side: Side::Buy,
                price,
                quantity: self.qty,
                order_type: OrderType::Market,
                reason: "open long".into(),
                timestamp: ts,
                expected_edge_bps: None,
                risk_reducing: false,
            }])),
            1 => Ok(Some(vec![TradeSignal {
                symbol,
                market_id: 0,
                side: Side::Sell,
                price,
                quantity: self.qty * 2.0, // close long + open short same size
                order_type: OrderType::Market,
                reason: "reverse short".into(),
                timestamp: ts,
                expected_edge_bps: None,
                risk_reducing: false,
            }])),
            _ => Ok(None),
        }
    }

    fn reset(&mut self) {}
}

pub(crate) fn candle(ts: i64, close: f64) -> Candlestick {
    Candlestick {
        timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
        open: close,
        high: close,
        low: close,
        close,
        volume: 1.0,
        symbol: "BTC".to_string(),
    }
}

pub(crate) fn known_candles() -> Vec<Candlestick> {
    // first 100, mid 110, last 120 — deterministic prices for BH math
    vec![
        candle(1_700_000_000, 100.0),
        candle(1_700_003_600, 110.0),
        candle(1_700_007_200, 120.0),
    ]
}

pub(crate) fn expected_bh_return(first: f64, last: f64, slip: f64, comm: f64, initial: f64) -> f64 {
    let buy_px = first * (1.0 + slip);
    let sell_px = last * (1.0 - slip);
    let qty = initial / buy_px;
    let open_comm = buy_px * comm * qty;
    let pnl = (sell_px - buy_px) * qty;
    let close_comm = sell_px * comm * qty;
    let final_cap = initial - open_comm + pnl - close_comm;
    (final_cap - initial) / initial
}
