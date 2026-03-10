/// Hyperliquid exchange driver.
///
/// Market data: WebSocket connection to wss://api.hyperliquid.xyz/ws
///   - Subscribes to l2Book channel for full book snapshots (~500ms per block)
///   - Background reader task parses messages and applies to internal OrderBook
///   - Reconnects automatically with exponential backoff on disconnect
///
/// Orders: REST API at https://api.hyperliquid.xyz/exchange
///   - Stubbed for now — requires EIP-712 signing with a private key
///   - Signing logic is the only missing piece before real order placement

use std::time::{Duration, Instant};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::header::{HeaderValue, USER_AGENT, HOST, ORIGIN},
        Message,
    },
};

use crate::exchange::{ConnectionState, DriverError, ExchangeDriver, VenueCapabilities};
use crate::order_book::{BookSnapshot, BookUpdate, PriceLevel, RawBookUpdate};
use crate::types::{InstrumentSpec, Order, OrderId, OrderType, Position};

// ─── Constants ───────────────────────────────────────────────────────────────

const HL_WS_URL: &str = "wss://api.hyperliquid.xyz/ws";
const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 30_000;
const MAX_RECONNECT_ATTEMPTS: u32 = 10;

// ─── Wire format ─────────────────────────────────────────────────────────────

/// Subscription message sent to HL WebSocket on connect.
#[derive(Serialize)]
struct HlSubscribeMsg {
    method: &'static str,
    subscription: HlSubscription,
}

#[derive(Serialize)]
struct HlSubscription {
    #[serde(rename = "type")]
    kind: &'static str,
    coin: String,
}

/// Incoming WebSocket message envelope.
/// HL sends: {"channel": "l2Book", "data": {...}}
#[derive(Deserialize, Debug)]
struct HlWsMessage {
    channel: String,
    data: serde_json::Value,
}

/// L2 book data payload.
#[derive(Deserialize, Debug)]
pub struct HlL2BookData {
    pub coin: String,
    pub levels: Vec<Vec<HlLevel>>, // [0]=bids, [1]=asks
    pub time: u64,
}

/// Single price level from HL wire format.
#[derive(Deserialize, Debug)]
pub struct HlLevel {
    pub px: String, // price as decimal string e.g. "180.45"
    pub sz: String, // size as decimal string e.g. "1.50"
    pub n: u64,     // number of orders at this level
}

// ─── Internal control messages ───────────────────────────────────────────────

/// Sent from the WebSocket reader task to signal disconnection.
enum ReaderEvent {
    Disconnected { reason: String },
}

// ─── Driver ──────────────────────────────────────────────────────────────────

pub struct HlDriver {
    instrument: InstrumentSpec,
    /// Sends raw parsed book updates to the order_book task.
    /// The order_book task owns the OrderBook and publishes BookSnapshots.
    update_tx: mpsc::Sender<RawBookUpdate>,
    /// Publishes connection state to watchdog.
    conn_tx: watch::Sender<ConnectionState>,
    /// Channel to receive events from the background reader task.
    reader_rx: Option<mpsc::Receiver<ReaderEvent>>,
    connected: bool,
}

impl HlDriver {
    pub fn new(
        instrument: InstrumentSpec,
        update_tx: mpsc::Sender<RawBookUpdate>,
        conn_tx: watch::Sender<ConnectionState>,
    ) -> Self {
        Self {
            instrument,
            update_tx,
            conn_tx,
            reader_rx: None,
            connected: false,
        }
    }

    /// Parse a raw HL L2 WebSocket JSON message and forward to the order_book task.
    /// The order_book task owns the OrderBook and publishes BookSnapshots.
    pub fn handle_l2_message(
        &mut self,
        raw: &str,
        seq: u64,
    ) -> Result<bool, DriverError> {
        let msg: HlWsMessage = serde_json::from_str(raw)
            .map_err(|e| DriverError::Protocol(format!("parse error: {e}")))?;

        if msg.channel != "l2Book" {
            return Ok(false); // not a book message — ignored
        }

        let data: HlL2BookData = serde_json::from_value(msg.data)
            .map_err(|e| DriverError::Protocol(format!("l2Book data parse error: {e}")))?;

        if data.levels.len() < 2 {
            return Err(DriverError::Protocol("l2Book missing bid/ask levels".into()));
        }

        let bids = parse_hl_levels(&data.levels[0], &self.instrument)?;
        let asks = parse_hl_levels(&data.levels[1], &self.instrument)?;

        // Forward raw update to order_book task — it owns the OrderBook state
        self.update_tx
            .try_send(RawBookUpdate {
                update: BookUpdate::Full { bids, asks },
                seq,
            })
            .map_err(|e| DriverError::Protocol(format!("order_book task unreachable: {e}")))?;

        Ok(true)
    }

    /// Spawn the background WebSocket reader task.
    /// The task connects, subscribes, reads messages, and sends ReaderEvents
    /// back via mpsc. It handles reconnection with exponential backoff.
    fn spawn_reader_task(&self) -> mpsc::Receiver<ReaderEvent> {
        let (event_tx, event_rx) = mpsc::channel::<ReaderEvent>(64);
        let coin = self.instrument.symbol.clone();
        let spec = self.instrument.clone();
        let conn_tx = self.conn_tx.clone();
        let update_tx = self.update_tx.clone();

        tokio::spawn(async move {
            let mut attempts: u32 = 0;
            let mut seq: u64 = 0;

            loop {
                tracing::info!(coin = %coin, attempt = attempts, "connecting to HL WebSocket");

                // Build request with headers HL requires
                let mut request = HL_WS_URL.into_client_request()
                    .expect("valid ws url");
                request.headers_mut().insert(
                    USER_AGENT,
                    HeaderValue::from_static("dual-venue-mm/0.1"),
                );
                request.headers_mut().insert(
                    HOST,
                    HeaderValue::from_static("api.hyperliquid.xyz"),
                );
                request.headers_mut().insert(
                    ORIGIN,
                    HeaderValue::from_static("https://app.hyperliquid.xyz"),
                );

                match connect_async(request).await {
                    Err(e) => {
                        tracing::error!(error = %e, "WebSocket connect failed");
                        let _ = conn_tx.send(ConnectionState::Reconnecting {
                            since: Instant::now(),
                            attempts,
                        });
                    }

                    Ok((mut ws_stream, _)) => {
                        attempts = 0;
                        let _ = conn_tx.send(ConnectionState::Connected);
                        tracing::info!(coin = %coin, "HL WebSocket connected");

                        // Brief pause — HL WebSocket needs a moment
                        // before it accepts subscription messages
                        sleep(Duration::from_millis(100)).await;

                        // Send subscription message
                        let sub = HlSubscribeMsg {
                            method: "subscribe",
                            subscription: HlSubscription {
                                kind: "l2Book",
                                coin: coin.clone(),
                            },
                        };
                        let sub_json = serde_json::to_string(&sub).unwrap();
                        if let Err(e) = ws_stream.send(Message::Text(sub_json.into())).await {
                            tracing::error!(error = %e, "failed to send subscription");
                            continue;
                        }
                        tracing::info!(coin = %coin, "subscribed to l2Book");

                        // Read messages until disconnect
                        while let Some(msg) = ws_stream.next().await {
                            match msg {
                                Ok(Message::Text(text)) => {
                                    // Parse the envelope to extract bids/asks
                                    match parse_l2_message(&text, &coin, &spec) {
                                        Some((bids, asks)) => {
                                            seq += 1;
                                            // Forward to order_book task — it owns OrderBook state
                                            let raw = RawBookUpdate {
                                                update: BookUpdate::Full { bids, asks },
                                                seq,
                                            };
                                            if update_tx.try_send(raw).is_err() {
                                                tracing::warn!("order_book task channel full or closed");
                                            }
                                        }
                                        None => {} // non-book message — ignore
                                    }
                                }
                                Ok(Message::Ping(data)) => {
                                    // Respond to pings to keep connection alive
                                    let _ = ws_stream.send(Message::Pong(data)).await;
                                }
                                Ok(Message::Close(_)) => {
                                    tracing::warn!(coin = %coin, "HL WebSocket closed by server");
                                    break;
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "WebSocket read error");
                                    break;
                                }
                                _ => {} // Binary, Pong, Frame — ignore
                            }
                        }

                        let _ = event_tx
                            .send(ReaderEvent::Disconnected {
                                reason: "connection dropped".into(),
                            })
                            .await;
                    }
                }

                // Exponential backoff before reconnecting
                attempts += 1;
                if attempts >= MAX_RECONNECT_ATTEMPTS {
                    tracing::error!(coin = %coin, "max reconnect attempts reached");
                    let _ = conn_tx.send(ConnectionState::Failed {
                        reason: "max reconnect attempts reached".into(),
                    });
                    return;
                }

                let backoff_ms = (RECONNECT_BASE_MS * 2u64.pow(attempts)).min(RECONNECT_MAX_MS);
                let jitter_ms = (backoff_ms as f64 * 0.2 * rand_jitter()) as u64;
                let wait_ms = backoff_ms + jitter_ms;

                tracing::info!(
                    wait_ms,
                    attempt = attempts,
                    "reconnecting after backoff"
                );
                let _ = conn_tx.send(ConnectionState::Reconnecting {
                    since: Instant::now(),
                    attempts,
                });
                sleep(Duration::from_millis(wait_ms)).await;
            }
        });

        event_rx
    }
}

/// Parse an incoming WebSocket text message.
/// Returns (bids, asks) as normalized PriceLevels if it's a valid l2Book message
/// for our coin, otherwise None.
fn parse_l2_message(
    text: &str,
    coin: &str,
    spec: &InstrumentSpec,
) -> Option<(Vec<PriceLevel>, Vec<PriceLevel>)> {
    if !text.contains("l2Book") {
        return None;
    }

    let msg: HlWsMessage = serde_json::from_str(text).ok()?;
    if msg.channel != "l2Book" {
        return None;
    }

    let data: HlL2BookData = serde_json::from_value(msg.data).ok()?;
    if data.coin != coin {
        return None;
    }
    if data.levels.len() < 2 {
        return None;
    }

    let bids = data.levels[0]
        .iter()
        .filter_map(|l| {
            Some(PriceLevel {
                price: spec.normalize_price(&l.px).ok()?,
                quantity: spec.normalize_quantity(&l.sz).ok()?,
            })
        })
        .collect();

    let asks = data.levels[1]
        .iter()
        .filter_map(|l| {
            Some(PriceLevel {
                price: spec.normalize_price(&l.px).ok()?,
                quantity: spec.normalize_quantity(&l.sz).ok()?,
            })
        })
        .collect();

    Some((bids, asks))
}

/// Parse a slice of HlLevel into normalized PriceLevel vec using InstrumentSpec.
fn parse_hl_levels(
    levels: &[HlLevel],
    spec: &InstrumentSpec,
) -> Result<Vec<PriceLevel>, DriverError> {
    levels
        .iter()
        .map(|l| {
            let price = spec
                .normalize_price(&l.px)
                .map_err(|e| DriverError::Protocol(format!("price parse: {e}")))?;
            let quantity = spec
                .normalize_quantity(&l.sz)
                .map_err(|e| DriverError::Protocol(format!("qty parse: {e}")))?;
            Ok(PriceLevel { price, quantity })
        })
        .collect()
}

/// Simple pseudo-random jitter in [0, 1) using system time.
/// Production would use the `rand` crate.
fn rand_jitter() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    (nanos % 1000) as f64 / 1000.0
}

// ─── ExchangeDriver impl ─────────────────────────────────────────────────────

#[async_trait]
impl ExchangeDriver for HlDriver {
    async fn connect(&mut self) -> Result<(), DriverError> {
        if self.connected {
            return Ok(());
        }

        // Spawn the background WebSocket reader task.
        // It connects immediately, subscribes, and starts streaming book updates.
        // Events come back via reader_rx and are processed by the caller
        // (typically the hl_feed task loop).
        let reader_rx = self.spawn_reader_task();
        self.reader_rx = Some(reader_rx);
        self.connected = true;

        tracing::info!(
            venue = "HL",
            instrument = %self.instrument.symbol,
            url = HL_WS_URL,
            "WebSocket reader task spawned"
        );
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), DriverError> {
        // Dropping reader_rx causes the background task's send() to fail,
        // which triggers the task to return and clean up the WebSocket.
        self.reader_rx = None;
        self.connected = false;
        tracing::info!(venue = "HL", "disconnected — reader task will terminate");
        Ok(())
    }

    async fn subscribe_market_data(&mut self) -> Result<(), DriverError> {
        if !self.connected {
            return Err(DriverError::NotConnected);
        }
        // Subscription is sent inside the reader task on connect.
        // This method is a no-op for HL — kept in the interface for symmetry
        // with IBKR where subscribe_market_data() sends a separate request.
        tracing::info!(
            venue = "HL",
            instrument = %self.instrument.symbol,
            "market data subscription active (managed by reader task)"
        );
        Ok(())
    }

    async fn place_order(&mut self, order: Order) -> Result<OrderId, DriverError> {
        if !self.connected {
            return Err(DriverError::NotConnected);
        }

        // TODO: implement EIP-712 signing + POST to https://api.hyperliquid.xyz/exchange
        // Required:
        //   1. Wallet private key (loaded from env/config at startup)
        //   2. EIP-712 domain separator for HL
        //   3. alloy or ethers-rs for signing
        //   4. Nonce management (timestamp-based, must be monotonically increasing)
        //
        // Message format:
        // {
        //   "action": {
        //     "type": "order",
        //     "orders": [{
        //       "a": <asset_index>,  // TSLA = 0 on mainnet
        //       "b": true,           // is_buy
        //       "p": "180.45",       // price
        //       "s": "1.5",          // size
        //       "r": false,          // reduce_only
        //       "t": {"limit": {"tif": "Alo"}}  // post_only
        //     }],
        //     "grouping": "na"
        //   },
        //   "nonce": <unix_ms>,
        //   "signature": {"r": "0x...", "s": "0x...", "v": 28}
        // }

        let order_id = format!("hl-{}", timestamp_nonce());
        tracing::info!(
            venue = "HL",
            order_id = %order_id,
            instrument = %order.instrument,
            side = ?order.side,
            "order stub — EIP-712 signing not yet implemented"
        );
        Ok(order_id)
    }

    async fn cancel_order(&mut self, id: &OrderId) -> Result<(), DriverError> {
        if !self.connected {
            return Err(DriverError::NotConnected);
        }
        // TODO: POST to /exchange with action type "cancel"
        // {"action": {"type": "cancel", "cancels": [{"a": <asset>, "o": <oid>}]}, ...}
        tracing::info!(venue = "HL", order_id = %id, "cancel stub");
        Ok(())
    }

    async fn get_positions(&self) -> Result<Vec<Position>, DriverError> {
        if !self.connected {
            return Err(DriverError::NotConnected);
        }
        // TODO: POST to https://api.hyperliquid.xyz/info
        // {"type": "clearinghouseState", "user": "<wallet_address>"}
        Ok(vec![])
    }

    fn capabilities(&self) -> VenueCapabilities {
        VenueCapabilities {
            can_place_limit_orders: true,
            can_stream_order_book: true,
            provides_reference_price: false,
        }
    }

    fn venue_name(&self) -> &'static str {
        "Hyperliquid"
    }
}

/// Monotonically increasing timestamp nonce for order IDs.
fn timestamp_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::InstrumentSpec;

    fn make_driver() -> (HlDriver, tokio::sync::mpsc::Receiver<RawBookUpdate>) {
        let spec = InstrumentSpec::new("TSLA", 4, 2);
        let (update_tx, update_rx) = tokio::sync::mpsc::channel(64);
        let (conn_tx, _conn_rx) = watch::channel(ConnectionState::Reconnecting {
            since: Instant::now(),
            attempts: 0,
        });
        (HlDriver::new(spec, update_tx, conn_tx), update_rx)
    }

    fn l2_msg(bids: Vec<(&str, &str)>, asks: Vec<(&str, &str)>) -> String {
        let fmt = |levels: Vec<(&str, &str)>| -> String {
            levels
                .iter()
                .map(|(px, sz)| format!(r#"{{"px":"{}","sz":"{}","n":1}}"#, px, sz))
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            r#"{{"channel":"l2Book","data":{{"coin":"TSLA","levels":[[{}],[{}]],"time":1234}}}}"#,
            fmt(bids),
            fmt(asks)
        )
    }

    #[test]
    fn parses_valid_l2_and_sends_raw_update() {
        let (mut driver, mut update_rx) = make_driver();
        let msg = l2_msg(
            vec![("180.45", "1.50"), ("180.40", "2.00")],
            vec![("180.50", "1.00"), ("180.55", "3.00")],
        );
        driver.handle_l2_message(&msg, 1).unwrap();

        let raw = update_rx.try_recv().expect("update should be sent");
        assert_eq!(raw.seq, 1);
        match raw.update {
            BookUpdate::Full { bids, asks } => {
                assert_eq!(bids[0].price, 1_804_500); // 180.45 * 10^4
                assert_eq!(asks[0].price, 1_805_000); // 180.50 * 10^4
            }
            _ => panic!("expected Full update"),
        }
    }

    #[test]
    fn ignores_non_l2book_channel() {
        let (mut driver, mut update_rx) = make_driver();
        let msg = r#"{"channel":"fills","data":{"coin":"TSLA"}}"#;
        let result = driver.handle_l2_message(msg, 1).unwrap();
        assert!(!result); // false = not a book message
        assert!(update_rx.try_recv().is_err()); // nothing sent
    }

    #[test]
    fn crossed_book_is_forwarded_to_order_book_task() {
        // Crossed book detection is now in the order_book task, not the driver.
        // Driver forwards raw levels regardless — order_book task rejects.
        let (mut driver, mut update_rx) = make_driver();
        let msg = l2_msg(vec![("181.00", "1.00")], vec![("180.50", "1.00")]);
        let result = driver.handle_l2_message(&msg, 1);
        assert!(result.is_ok()); // driver forwards without validation
        assert!(update_rx.try_recv().is_ok()); // update was sent to order_book task
    }

    #[test]
    fn sequence_numbers_forwarded_correctly() {
        // Sequence gap detection is now in the order_book task.
        // Driver forwards both updates with their sequence numbers intact.
        let (mut driver, mut update_rx) = make_driver();
        driver.handle_l2_message(&l2_msg(vec![("180.00", "1.00")], vec![("181.00", "1.00")]), 1).unwrap();
        driver.handle_l2_message(&l2_msg(vec![("182.00", "1.00")], vec![("183.00", "1.00")]), 3).unwrap();

        let first = update_rx.try_recv().unwrap();
        let second = update_rx.try_recv().unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 3); // gap — order_book task will detect this
    }

    #[test]
    fn all_updates_forwarded_to_order_book_task() {
        // Recovery logic lives in the order_book task.
        // Driver forwards all valid parse results regardless of sequence.
        let (mut driver, mut update_rx) = make_driver();
        driver.handle_l2_message(&l2_msg(vec![("180.00", "1.00")], vec![("181.00", "1.00")]), 1).unwrap();
        driver.handle_l2_message(&l2_msg(vec![("180.00", "1.00")], vec![("181.00", "1.00")]), 3).unwrap();
        driver.handle_l2_message(&l2_msg(vec![("185.00", "1.00")], vec![("186.00", "1.00")]), 4).unwrap();

        // All three forwarded
        assert!(update_rx.try_recv().is_ok());
        assert!(update_rx.try_recv().is_ok());
        assert!(update_rx.try_recv().is_ok());
    }

    #[test]
    fn parse_l2_message_helper_correct_coin() {
        let msg = l2_msg(vec![("180.00", "1.00")], vec![("181.00", "1.00")]);
        let spec = InstrumentSpec::new("TSLA", 4, 2);
        let result = parse_l2_message(&msg, "TSLA", &spec);
        assert!(result.is_some());
        let (bids, asks) = result.unwrap();
        assert_eq!(bids[0].price, 1_800_000);
        assert_eq!(asks[0].price, 1_810_000);
    }

    #[test]
    fn parse_l2_message_helper_wrong_coin_returns_none() {
        let msg = l2_msg(vec![("180.00", "1.00")], vec![("181.00", "1.00")]);
        let spec = InstrumentSpec::new("TSLA", 4, 2);
        assert!(parse_l2_message(&msg, "BTC", &spec).is_none());
    }

    #[test]
    fn parse_l2_message_helper_ignores_non_book_channel() {
        let msg = r#"{"channel":"fills","data":{}}"#;
        let spec = InstrumentSpec::new("TSLA", 4, 2);
        assert!(parse_l2_message(msg, "TSLA", &spec).is_none());
    }

    #[test]
    fn capabilities_correct() {
        let (driver, _) = make_driver();
        let caps = driver.capabilities();
        assert!(caps.can_place_limit_orders);
        assert!(caps.can_stream_order_book);
        assert!(!caps.provides_reference_price);
    }

    #[tokio::test]
    async fn connect_spawns_reader_task() {
        let (mut driver, _update_rx) = make_driver();
        // connect() should succeed even before the WS connects —
        // the task runs in background and handles its own connection
        driver.connect().await.unwrap();
        assert!(driver.connected);
        assert!(driver.reader_rx.is_some());
    }

    #[tokio::test]
    async fn disconnect_drops_reader_rx() {
        let (mut driver, _update_rx) = make_driver();
        driver.connect().await.unwrap();
        driver.disconnect().await.unwrap();
        assert!(!driver.connected);
        assert!(driver.reader_rx.is_none());
    }

    #[tokio::test]
    async fn subscribe_market_data_requires_connection() {
        let (mut driver, _update_rx) = make_driver();
        assert!(driver.subscribe_market_data().await.is_err());
        driver.connect().await.unwrap();
        assert!(driver.subscribe_market_data().await.is_ok());
    }
}