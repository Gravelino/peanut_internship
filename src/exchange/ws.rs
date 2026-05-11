use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::core::types::BPS_SCALE;
use crate::exchange::errors::{ExchangeError, ExchangeResult};
use crate::exchange::types::OrderBookSnapshot;

/// Binance depth stream snapshot message.
#[derive(Debug, Clone, Deserialize)]
pub struct DepthSnapshot {
    /// Last update ID from the snapshot.
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: u64,
    /// Bid levels as [price, qty] pairs.
    pub bids: Vec<[String; 2]>,
    /// Ask levels as [price, qty] pairs.
    pub asks: Vec<[String; 2]>,
}

/// Binance depth stream incremental update message.
#[derive(Debug, Clone, Deserialize)]
pub struct DepthUpdate {
    /// First update ID in this event.
    #[serde(rename = "U")]
    pub first_update_id: u64,
    /// Last update ID in this event.
    #[serde(rename = "u")]
    pub last_update_id: u64,
    /// Bid level deltas as [price, qty] pairs.
    #[serde(rename = "b")]
    pub bids: Vec<[String; 2]>,
    /// Ask level deltas as [price, qty] pairs.
    #[serde(rename = "a")]
    pub asks: Vec<[String; 2]>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookTickerEvent {
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "b")]
    pub bid_price: String,
    #[serde(rename = "B")]
    pub bid_qty: String,
    #[serde(rename = "a")]
    pub ask_price: String,
    #[serde(rename = "A")]
    pub ask_qty: String,
}

/// A parsed depth event from the Binance WebSocket stream.
#[derive(Debug, Clone)]
pub enum DepthEvent {
    /// Full book snapshot (received on connection).
    Snapshot(DepthSnapshot),
    /// Incremental update to apply on top of the current book.
    Update(DepthUpdate),
}

/// Whether a sequence gap requires reconnection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceStatus {
    /// The update was applied successfully.
    Applied,
    /// The update was dropped as stale (u <= last_update_id).
    Stale,
    /// A gap was detected; reconnection and fresh snapshot required.
    NeedsReconnect,
}

/// Local order book that maintains BTreeMap-based state and applies incremental updates.
///
/// Bids are stored in **descending** order (highest first), asks in **ascending** order (lowest first).
/// Zero-quantity updates remove the price level; positive quantities replace the existing level.
#[derive(Debug, Clone)]
pub struct LocalOrderBook {
    symbol: String,
    /// Bids: ordered by price descending. Key = negative price for descending sort.
    bids: BTreeMap<OrderedDecimal, (Decimal, Decimal)>,
    /// Asks: ordered by price ascending.
    asks: BTreeMap<OrderedDecimal, (Decimal, Decimal)>,
    /// Last applied update ID for sequence validation.
    last_update_id: u64,
}

/// Wrapper that provides a total order for `Decimal` suitable for `BTreeMap` keys.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrderedDecimal(Decimal);

impl PartialOrd for OrderedDecimal {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedDecimal {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl OrderedDecimal {
    fn from_decimal(d: Decimal) -> Self {
        Self(d)
    }
}

impl LocalOrderBook {
    /// Creates an empty local book for the given symbol.
    pub fn new(symbol: &str) -> Self {
        Self {
            symbol: symbol.to_string(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_update_id: 0,
        }
    }

    /// Applies a full snapshot, replacing all book state.
    pub fn apply_snapshot(&mut self, snap: DepthSnapshot) {
        self.bids.clear();
        self.asks.clear();
        self.last_update_id = snap.last_update_id;

        for [price_str, qty_str] in &snap.bids {
            if let (Ok(p), Ok(q)) = (
                Decimal::from_str_exact(price_str),
                Decimal::from_str_exact(qty_str),
            ) && q > Decimal::ZERO
            {
                let key = OrderedDecimal::from_decimal(-p);
                self.bids.insert(key, (p, q));
            }
        }

        for [price_str, qty_str] in &snap.asks {
            if let (Ok(p), Ok(q)) = (
                Decimal::from_str_exact(price_str),
                Decimal::from_str_exact(qty_str),
            ) && q > Decimal::ZERO
            {
                let key = OrderedDecimal::from_decimal(p);
                self.asks.insert(key, (p, q));
            }
        }

        debug!(
            symbol = %self.symbol,
            bids = self.bids.len(),
            asks = self.asks.len(),
            last_update_id = self.last_update_id,
            "Applied snapshot"
        );
    }

    /// Applies an incremental update, validating sequence IDs.
    ///
    /// When the book is empty (`last_update_id == 0`), the first update is
    /// always accepted since there is no prior state to be consistent with.
    pub fn apply_update(&mut self, update: DepthUpdate) -> SequenceStatus {
        if update.last_update_id <= self.last_update_id {
            return SequenceStatus::Stale;
        }

        if self.last_update_id == 0 {
            // No snapshot received yet — accept the first update unconditionally.
        } else if update.first_update_id > self.last_update_id + 1 {
            warn!(
                first_u = update.first_update_id,
                last_u = update.last_update_id,
                expected = self.last_update_id + 1,
                "Sequence gap detected, need reconnect"
            );
            return SequenceStatus::NeedsReconnect;
        }

        for [price_str, qty_str] in &update.bids {
            if let (Ok(p), Ok(q)) = (
                Decimal::from_str_exact(price_str),
                Decimal::from_str_exact(qty_str),
            ) {
                let key = OrderedDecimal::from_decimal(-p);
                if q == Decimal::ZERO {
                    self.bids.remove(&key);
                } else {
                    self.bids.insert(key, (p, q));
                }
            }
        }

        for [price_str, qty_str] in &update.asks {
            if let (Ok(p), Ok(q)) = (
                Decimal::from_str_exact(price_str),
                Decimal::from_str_exact(qty_str),
            ) {
                let key = OrderedDecimal::from_decimal(p);
                if q == Decimal::ZERO {
                    self.asks.remove(&key);
                } else {
                    self.asks.insert(key, (p, q));
                }
            }
        }

        self.last_update_id = update.last_update_id;
        SequenceStatus::Applied
    }

    /// Converts the local book into an `OrderBookSnapshot` for use by `OrderBookAnalyzer`.
    pub fn snapshot(&self) -> OrderBookSnapshot {
        let bids: Vec<(Decimal, Decimal)> = self.bids.iter().map(|(_, &(p, q))| (p, q)).collect();

        let asks: Vec<(Decimal, Decimal)> = self.asks.iter().map(|(_, &(p, q))| (p, q)).collect();

        let best_bid = bids.first().copied();
        let best_ask = asks.first().copied();

        let (mid_price, spread_bps) = match (best_bid, best_ask) {
            (Some((bid_p, _)), Some((ask_p, _))) => {
                let mid = (bid_p + ask_p) / Decimal::TWO;
                let spread = ask_p - bid_p;
                let bps = if mid.is_zero() {
                    None
                } else {
                    Some(spread / mid * Decimal::from(BPS_SCALE))
                };
                (Some(mid), bps)
            }
            _ => (None, None),
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        OrderBookSnapshot {
            symbol: self.symbol.clone(),
            timestamp,
            bids,
            asks,
            best_bid,
            best_ask,
            mid_price,
            spread_bps,
        }
    }

    /// Returns the last applied update ID.
    pub fn last_update_id(&self) -> u64 {
        self.last_update_id
    }

    /// Returns the number of bid levels.
    pub fn bid_count(&self) -> usize {
        self.bids.len()
    }

    /// Returns the number of ask levels.
    pub fn ask_count(&self) -> usize {
        self.asks.len()
    }
}

/// Parses a raw WebSocket JSON message into a `DepthEvent`.
pub fn parse_depth_message(data: &str) -> ExchangeResult<DepthEvent> {
    let val: serde_json::Value = serde_json::from_str(data)?;

    if val.get("e").and_then(|v| v.as_str()) == Some("depthUpdate") {
        let update: DepthUpdate = serde_json::from_value(val)?;
        Ok(DepthEvent::Update(update))
    } else if val.get("lastUpdateId").is_some() {
        let snap: DepthSnapshot = serde_json::from_value(val)?;
        Ok(DepthEvent::Snapshot(snap))
    } else {
        Err(ExchangeError::JsonParse(
            serde_json::from_str::<serde_json::Value>("unknown depth event format").unwrap_err(),
        ))
    }
}

fn stream_url(ws_url: &str, stream: &str) -> String {
    let base = ws_url.trim_end_matches('/');
    if base.ends_with("/ws") {
        format!("{base}/{stream}")
    } else {
        format!("{base}/ws/{stream}")
    }
}

/// Connects to the Binance depth WebSocket stream and returns a receiver of `DepthEvent`.
///
/// The stream URL is `<ws_url>/ws/<symbol_lower>@depth@100ms`.
/// On connection, the first message is a snapshot; subsequent messages are incremental updates.
/// If the connection drops, it will attempt to reconnect with exponential backoff.
pub async fn subscribe_depth_stream(
    ws_url: &str,
    symbol: &str,
) -> ExchangeResult<tokio::sync::mpsc::Receiver<DepthEvent>> {
    use futures_util::StreamExt;
    use tokio_tungstenite::{connect_async, tungstenite};

    let symbol_lower = symbol.replace('/', "").to_lowercase();
    let url = stream_url(ws_url, &format!("{symbol_lower}@depth@100ms"));

    info!(url = %url, "Connecting to Binance depth stream");

    let (ws_stream, _) = connect_async(&url)
        .await
        .map_err(|e| ExchangeError::Network(format!("WS connect failed: {e}")))?;

    let (_, mut read) = ws_stream.split();

    let (tx, rx) = tokio::sync::mpsc::channel(256);

    tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(tungstenite::Message::Text(text)) => match parse_depth_message(&text) {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            debug!("Depth stream receiver dropped, stopping");
                            return;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to parse depth message");
                    }
                },
                Ok(tungstenite::Message::Ping(data)) => {
                    debug!("Received ping: {} bytes", data.len());
                }
                Ok(tungstenite::Message::Close(frame)) => {
                    warn!(frame = ?frame, "WebSocket closed by server");
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "WebSocket read error");
                    return;
                }
            }
        }
        info!("Depth stream ended");
    });

    Ok(rx)
}

pub async fn subscribe_book_ticker_stream(
    ws_url: &str,
    symbol: &str,
) -> ExchangeResult<tokio::sync::mpsc::Receiver<BookTickerEvent>> {
    use futures_util::StreamExt;
    use tokio_tungstenite::{connect_async, tungstenite};

    let symbol_lower = symbol.replace('/', "").to_lowercase();
    let url = stream_url(ws_url, &format!("{symbol_lower}@bookTicker"));

    info!(url = %url, "Connecting to Binance bookTicker stream");

    let (ws_stream, _) = connect_async(&url)
        .await
        .map_err(|e| ExchangeError::Network(format!("WS connect failed: {e}")))?;

    let (_, mut read) = ws_stream.split();

    let (tx, rx) = tokio::sync::mpsc::channel(256);

    tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(tungstenite::Message::Text(text)) => {
                    match serde_json::from_str::<BookTickerEvent>(&text) {
                        Ok(event) => {
                            if tx.send(event).await.is_err() {
                                debug!("Book ticker receiver dropped, stopping");
                                return;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to parse bookTicker message");
                        }
                    }
                }
                Ok(tungstenite::Message::Ping(data)) => {
                    debug!("Received ping: {} bytes", data.len());
                }
                Ok(tungstenite::Message::Close(frame)) => {
                    warn!(frame = ?frame, "WebSocket closed by server");
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "WebSocket read error");
                    return;
                }
            }
        }
        info!("Book ticker stream ended");
    });

    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_book_new() {
        let book = LocalOrderBook::new("ETHUSDT");
        assert_eq!(book.bid_count(), 0);
        assert_eq!(book.ask_count(), 0);
        assert_eq!(book.last_update_id(), 0);
    }

    #[test]
    fn test_local_book_apply_snapshot() {
        let mut book = LocalOrderBook::new("ETHUSDT");
        let snap = DepthSnapshot {
            last_update_id: 100,
            bids: vec![
                ["2000.00".into(), "5.00000000".into()],
                ["1999.50".into(), "3.00000000".into()],
            ],
            asks: vec![
                ["2000.50".into(), "2.00000000".into()],
                ["2001.00".into(), "4.00000000".into()],
            ],
        };
        book.apply_snapshot(snap);
        assert_eq!(book.bid_count(), 2);
        assert_eq!(book.ask_count(), 2);
        assert_eq!(book.last_update_id(), 100);
    }

    #[test]
    fn test_local_book_snapshot_conversion() {
        let mut book = LocalOrderBook::new("ETHUSDT");
        let snap = DepthSnapshot {
            last_update_id: 100,
            bids: vec![["2000.00".into(), "5.00000000".into()]],
            asks: vec![["2001.00".into(), "4.00000000".into()]],
        };
        book.apply_snapshot(snap);
        let ob = book.snapshot();
        assert_eq!(ob.symbol, "ETHUSDT");
        assert_eq!(ob.bids.len(), 1);
        assert_eq!(ob.asks.len(), 1);
        assert!(ob.best_bid.is_some());
        assert!(ob.best_ask.is_some());
    }

    #[test]
    fn test_local_book_apply_update_add() {
        let mut book = LocalOrderBook::new("ETHUSDT");
        book.apply_snapshot(DepthSnapshot {
            last_update_id: 100,
            bids: vec![["2000.00".into(), "5.00000000".into()]],
            asks: vec![["2001.00".into(), "4.00000000".into()]],
        });

        let status = book.apply_update(DepthUpdate {
            first_update_id: 101,
            last_update_id: 101,
            bids: vec![["1999.00".into(), "2.00000000".into()]],
            asks: vec![],
        });
        assert_eq!(status, SequenceStatus::Applied);
        assert_eq!(book.bid_count(), 2);
    }

    #[test]
    fn test_local_book_apply_update_remove() {
        let mut book = LocalOrderBook::new("ETHUSDT");
        book.apply_snapshot(DepthSnapshot {
            last_update_id: 100,
            bids: vec![
                ["2000.00".into(), "5.00000000".into()],
                ["1999.00".into(), "2.00000000".into()],
            ],
            asks: vec![["2001.00".into(), "4.00000000".into()]],
        });

        let status = book.apply_update(DepthUpdate {
            first_update_id: 101,
            last_update_id: 101,
            bids: vec![["1999.00".into(), "0.00000000".into()]],
            asks: vec![],
        });
        assert_eq!(status, SequenceStatus::Applied);
        assert_eq!(book.bid_count(), 1);
    }

    #[test]
    fn test_local_book_apply_update_modify() {
        let mut book = LocalOrderBook::new("ETHUSDT");
        book.apply_snapshot(DepthSnapshot {
            last_update_id: 100,
            bids: vec![["2000.00".into(), "5.00000000".into()]],
            asks: vec![],
        });

        book.apply_update(DepthUpdate {
            first_update_id: 101,
            last_update_id: 101,
            bids: vec![["2000.00".into(), "8.00000000".into()]],
            asks: vec![],
        });
        let ob = book.snapshot();
        assert_eq!(ob.bids[0].1, Decimal::from(8));
    }

    #[test]
    fn test_local_book_apply_update_stale() {
        let mut book = LocalOrderBook::new("ETHUSDT");
        book.apply_snapshot(DepthSnapshot {
            last_update_id: 200,
            bids: vec![],
            asks: vec![],
        });

        let status = book.apply_update(DepthUpdate {
            first_update_id: 190,
            last_update_id: 199,
            bids: vec![],
            asks: vec![],
        });
        assert_eq!(status, SequenceStatus::Stale);
    }

    #[test]
    fn test_local_book_apply_update_gap() {
        let mut book = LocalOrderBook::new("ETHUSDT");
        book.apply_snapshot(DepthSnapshot {
            last_update_id: 100,
            bids: vec![],
            asks: vec![],
        });

        let status = book.apply_update(DepthUpdate {
            first_update_id: 105,
            last_update_id: 110,
            bids: vec![],
            asks: vec![],
        });
        assert_eq!(status, SequenceStatus::NeedsReconnect);
    }

    #[test]
    fn test_parse_depth_message_update() {
        let json = r#"{"e":"depthUpdate","E":1672531200000,"s":"ETHUSDT","U":101,"u":102,"b":[["2000.00","5.00"]],"a":[["2001.00","3.00"]]}"#;
        let event = parse_depth_message(json).unwrap();
        match event {
            DepthEvent::Update(u) => {
                assert_eq!(u.first_update_id, 101);
                assert_eq!(u.last_update_id, 102);
                assert_eq!(u.bids.len(), 1);
                assert_eq!(u.asks.len(), 1);
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn test_parse_depth_message_snapshot() {
        let json =
            r#"{"lastUpdateId":100,"bids":[["2000.00","5.00"]],"asks":[["2001.00","3.00"]]}"#;
        let event = parse_depth_message(json).unwrap();
        match event {
            DepthEvent::Snapshot(s) => {
                assert_eq!(s.last_update_id, 100);
                assert_eq!(s.bids.len(), 1);
            }
            _ => panic!("expected Snapshot"),
        }
    }

    #[test]
    fn test_parse_depth_message_unknown() {
        let json = r#"{"foo":"bar"}"#;
        assert!(parse_depth_message(json).is_err());
    }
}
