/// Interactive Brokers (IBKR) exchange driver.
///
/// IBKR uses a proprietary binary protocol (TWS API) over TCP.
/// The TWS API is request/response based with numeric reqIds:
///   client sends reqMktData(reqId=1, symbol="TSLA")
///   server later sends tickPrice(reqId=1, field=BID, price=180.45)
///
/// There is no official Rust SDK. Options:
///   - ibtwsapi crate (partial, community maintained)
///   - tws-rs crate (another community effort)
///   - Implement the binary protocol directly (documented by IBKR)
///
/// This implementation provides a realistic stub with the correct interface.
/// The TCP connection and binary protocol parsing are represented as comments
/// showing what production code would do at each point.
///
/// IBKR role in this system:
///   - Market orders ONLY (hedging requires immediate execution, not price improvement)
///   - Top-of-book market data (best bid/ask/last for reference price and hedge cost estimation)
///   - Limit orders return DriverError::NotSupported

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use async_trait::async_trait;
use tokio::sync::{watch, Mutex};

use crate::exchange::{ConnectionState, DriverError, ExchangeDriver, IbkrMarketData, VenueCapabilities};
use crate::types::{Order, OrderId, OrderType, Position, Symbol, InstrumentSpec};

// ─── IBKR protocol constants (subset) ───────────────────────────────────────
// These would be used by the binary encoder/decoder in production.

#[allow(dead_code)]
mod protocol {
    pub const REQ_MKT_DATA: u32 = 1;
    pub const CANCEL_MKT_DATA: u32 = 2;
    pub const PLACE_ORDER: u32 = 3;
    pub const CANCEL_ORDER: u32 = 4;
    pub const REQ_POSITIONS: u32 = 61;

    pub const TICK_PRICE: u32 = 1;
    pub const ORDER_STATUS: u32 = 3;
    pub const POSITION: u32 = 61;

    // Tick field IDs for market data
    pub const FIELD_BID: i32 = 1;
    pub const FIELD_ASK: i32 = 2;
    pub const FIELD_LAST: i32 = 4;
}

// ─── Pending request tracking ────────────────────────────────────────────────

/// Matches async reqId-based responses back to their callers.
/// IBKR sends responses asynchronously tagged with the original reqId.
struct PendingOrders {
    /// reqId → oneshot sender waiting for order acknowledgement
    inner: HashMap<u32, tokio::sync::oneshot::Sender<OrderAck>>,
    next_req_id: u32,
}

impl PendingOrders {
    fn new() -> Self {
        Self { inner: HashMap::new(), next_req_id: 1 }
    }

    fn next_req_id(&mut self) -> u32 {
        let id = self.next_req_id;
        self.next_req_id += 1;
        id
    }
}

#[derive(Debug)]
enum OrderAck {
    Filled { order_id: String },
    Rejected { reason: String },
}

// ─── Driver ──────────────────────────────────────────────────────────────────

pub struct IbkrDriver {
    instrument: InstrumentSpec,
    /// Publishes top-of-book market data to world_view and watchdog.
    market_data_tx: watch::Sender<Option<IbkrMarketData>>,
    /// Publishes connection state to watchdog.
    conn_tx: watch::Sender<ConnectionState>,
    /// Tracks pending order requests awaiting async IBKR responses.
    pending: Arc<Mutex<PendingOrders>>,
    connected: bool,
}

impl IbkrDriver {
    pub fn new(
        instrument: InstrumentSpec,
        market_data_tx: watch::Sender<Option<IbkrMarketData>>,
        conn_tx: watch::Sender<ConnectionState>,
    ) -> Self {
        Self {
            instrument,
            market_data_tx,
            conn_tx,
            pending: Arc::new(Mutex::new(PendingOrders::new())),
            connected: false,
        }
    }

    /// Called by the background TCP reader loop when a tick price arrives.
    /// Production: called after parsing binary TICK_PRICE message.
    pub fn handle_tick_price(&self, req_id: u32, field: i32, price: f64) {
        // This would update the internal market data state.
        // In production the reader loop maintains a mutable MarketDataState
        // and publishes to market_data_tx after each relevant tick.
        tracing::debug!(req_id, field, price, "tick price received (stub)");
    }

    /// Simulate receiving market data — used in tests to drive the stub.
    pub fn inject_market_data(&self, bid: f64, ask: f64, last: f64) {
        let data = IbkrMarketData {
            best_bid: Some(self.instrument.normalize_price_f64(bid)),
            best_ask: Some(self.instrument.normalize_price_f64(ask)),
            last_trade: Some(self.instrument.normalize_price_f64(last)),
            timestamp: Instant::now(),
        };
        let _ = self.market_data_tx.send(Some(data));
    }
}

#[async_trait]
impl ExchangeDriver for IbkrDriver {
    async fn connect(&mut self) -> Result<(), DriverError> {
        // Production:
        //   1. Open TCP connection to TWS/Gateway (default port 7497 paper, 7496 live)
        //   2. Send CLIENT_VERSION handshake
        //   3. Receive SERVER_VERSION + connection time
        //   4. Send START_API with clientId
        //   5. Spawn background reader task that calls handle_tick_price() etc.
        //   6. Negotiate version, log if different from last session (protocol change detection)

        self.connected = true;
        let _ = self.conn_tx.send(ConnectionState::Connected);
        tracing::info!(venue = "IBKR", instrument = %self.instrument.symbol, "connected (stub)");
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<(), DriverError> {
        self.connected = false;
        tracing::info!(venue = "IBKR", "disconnected");
        Ok(())
    }

    async fn subscribe_market_data(&mut self) -> Result<(), DriverError> {
        if !self.connected {
            return Err(DriverError::NotConnected);
        }

        let mut pending = self.pending.lock().await;
        let req_id = pending.next_req_id();
        drop(pending);

        // Production:
        //   Send REQ_MKT_DATA(reqId, contract) over TCP
        //   contract for TSLA CFD:
        //     symbol="TSLA", secType="CFD", exchange="SMART", currency="USD"
        //   Server will stream TICK_PRICE messages with reqId for BID/ASK/LAST

        tracing::info!(
            venue = "IBKR",
            req_id,
            instrument = %self.instrument.symbol,
            "subscribed to market data (stub)"
        );
        Ok(())
    }

    async fn place_order(&mut self, order: Order) -> Result<OrderId, DriverError> {
        if !self.connected {
            return Err(DriverError::NotConnected);
        }

        // IBKR is used only for hedging — limit orders are not supported.
        // Hedging requires immediate execution at market price.
        match &order.order_type {
            OrderType::Limit { .. } => {
                return Err(DriverError::NotSupported);
            }
            OrderType::Market => {}
        }

        let mut pending = self.pending.lock().await;
        let req_id = pending.next_req_id();
        drop(pending);

        // Production:
        //   1. Build IBKR Order struct (orderId, contract, action=BUY/SELL, orderType=MKT, totalQuantity)
        //   2. Serialize to binary PLACE_ORDER message
        //   3. Send over TCP
        //   4. Await ORDER_STATUS response with matching orderId
        //   5. Map IBKR order states (PreSubmitted, Submitted, Filled, Cancelled, Inactive)

        let order_id = format!("ibkr-order-{}", req_id);
        tracing::info!(
            venue = "IBKR",
            order_id = %order_id,
            instrument = %order.instrument,
            side = ?order.side,
            "market order placed (stub)"
        );
        Ok(order_id)
    }

    async fn cancel_order(&mut self, id: &OrderId) -> Result<(), DriverError> {
        if !self.connected {
            return Err(DriverError::NotConnected);
        }
        // Production: send CANCEL_ORDER message over TCP
        // Note: IBKR hedge orders are Market orders — they fill immediately
        // so cancellation is rarely needed, but included for completeness.
        tracing::info!(venue = "IBKR", order_id = %id, "cancel sent (stub)");
        Ok(())
    }

    async fn get_positions(&self) -> Result<Vec<Position>, DriverError> {
        if !self.connected {
            return Err(DriverError::NotConnected);
        }
        // Production:
        //   Send REQ_POSITIONS over TCP
        //   Receive stream of POSITION messages until POSITION_END
        //   Parse and return as Vec<Position>
        Ok(vec![])
    }

    fn capabilities(&self) -> VenueCapabilities {
        VenueCapabilities {
            can_place_limit_orders: false, // hedging only — market orders only
            can_stream_order_book: false,  // only top-of-book tick data
            provides_reference_price: true, // real TSLA stock price during market hours
        }
    }

    fn venue_name(&self) -> &'static str {
        "IBKR"
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InstrumentSpec, Side, TimeInForce};

    fn make_driver() -> (IbkrDriver, watch::Receiver<Option<IbkrMarketData>>) {
        let spec = InstrumentSpec::new("TSLA", 4, 2);
        let (market_data_tx, market_data_rx) = watch::channel(None);
        let (conn_tx, _conn_rx) = watch::channel(ConnectionState::Reconnecting {
            since: Instant::now(),
            attempts: 0,
        });
        let driver = IbkrDriver::new(spec, market_data_tx, conn_tx);
        (driver, market_data_rx)
    }

    #[tokio::test]
    async fn connect_publishes_connected_state() {
        let spec = InstrumentSpec::new("TSLA", 4, 2);
        let (market_data_tx, _) = watch::channel(None);
        let (conn_tx, mut conn_rx) = watch::channel(ConnectionState::Reconnecting {
            since: Instant::now(),
            attempts: 0,
        });
        let mut driver = IbkrDriver::new(spec, market_data_tx, conn_tx);
        driver.connect().await.unwrap();
        assert_eq!(*conn_rx.borrow(), ConnectionState::Connected);
    }

    #[tokio::test]
    async fn place_market_order_succeeds() {
        let (mut driver, _) = make_driver();
        driver.connect().await.unwrap();

        let order = Order {
            instrument: "TSLA".into(),
            side: Side::Buy,
            quantity: 100,
            order_type: OrderType::Market,
            tif: TimeInForce::Ioc,
        };

        let result = driver.place_order(order).await;
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with("ibkr-order-"));
    }

    #[tokio::test]
    async fn place_limit_order_returns_not_supported() {
        let (mut driver, _) = make_driver();
        driver.connect().await.unwrap();

        let order = Order {
            instrument: "TSLA".into(),
            side: Side::Buy,
            quantity: 100,
            order_type: OrderType::Limit { price: 1_800_000, post_only: false },
            tif: TimeInForce::Gtc,
        };

        let result = driver.place_order(order).await;
        assert!(matches!(result, Err(DriverError::NotSupported)));
    }

    #[tokio::test]
    async fn place_order_when_not_connected_returns_error() {
        let (mut driver, _) = make_driver();
        // Not connected

        let order = Order {
            instrument: "TSLA".into(),
            side: Side::Buy,
            quantity: 100,
            order_type: OrderType::Market,
            tif: TimeInForce::Ioc,
        };

        let result = driver.place_order(order).await;
        assert!(matches!(result, Err(DriverError::NotConnected)));
    }

    #[test]
    fn capabilities_reflect_ibkr_role() {
        let (driver, _) = make_driver();
        let caps = driver.capabilities();
        assert!(!caps.can_place_limit_orders);
        assert!(!caps.can_stream_order_book);
        assert!(caps.provides_reference_price);
    }

    #[test]
    fn market_data_injection_normalizes_prices() {
        let (driver, mut rx) = make_driver();
        driver.inject_market_data(180.45, 180.50, 180.47);

        let data = rx.borrow().clone().expect("should have market data");
        assert_eq!(data.best_bid, Some(1_804_500));
        assert_eq!(data.best_ask, Some(1_805_000));
        assert_eq!(data.last_trade, Some(1_804_700));
    }

    #[test]
    fn ibkr_market_data_mid() {
        let data = IbkrMarketData {
            best_bid: Some(1_804_500),
            best_ask: Some(1_805_000),
            last_trade: Some(1_804_700),
            timestamp: Instant::now(),
        };
        assert_eq!(data.mid(), Some(1_804_750));
    }

    #[test]
    fn ibkr_market_data_staleness() {
        let data = IbkrMarketData {
            best_bid: Some(1_804_500),
            best_ask: Some(1_805_000),
            last_trade: None,
            timestamp: Instant::now(),
        };
        use std::time::Duration;
        assert!(!data.is_stale(Duration::from_secs(1)));
        assert!(data.is_stale(Duration::from_nanos(0)));
    }

    #[test]
    fn venue_name_is_ibkr() {
        let (driver, _) = make_driver();
        assert_eq!(driver.venue_name(), "IBKR");
    }
}