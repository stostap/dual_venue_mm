pub mod hyperliquid;
pub mod ibkr;

use std::time::Instant;
use async_trait::async_trait;
use thiserror::Error;

use crate::types::{Order, OrderId, Position, Price, Quantity, Symbol};

// ─── Venue capabilities ──────────────────────────────────────────────────────

/// Declares what a venue supports. Checked at runtime before attempting
/// unsupported operations, rather than encoding in the trait signature.
#[derive(Debug, Clone)]
pub struct VenueCapabilities {
    /// Can place and cancel limit orders.
    pub can_place_limit_orders: bool,
    /// Streams a full order book (L2 or L4).
    pub can_stream_order_book: bool,
    /// Provides a reference price for an underlying asset (e.g. TSLA stock).
    pub provides_reference_price: bool,
}

// ─── Market data snapshots ───────────────────────────────────────────────────

/// Top-of-book market data from IBKR.
/// IBKR is used only for hedging and reference price — no full book needed.
#[derive(Debug, Clone)]
pub struct IbkrMarketData {
    pub best_bid: Option<Price>,
    pub best_ask: Option<Price>,
    pub last_trade: Option<Price>,
    pub timestamp: Instant,
}

impl IbkrMarketData {
    pub fn mid(&self) -> Option<Price> {
        Some((self.best_bid? + self.best_ask?) / 2)
    }

    pub fn is_stale(&self, threshold: std::time::Duration) -> bool {
        self.timestamp.elapsed() > threshold
    }
}

/// Reference price state, accounting for market hours.
/// IBKR only provides live TSLA price during US market hours (9:30-16:00 ET).
/// HL perpetuals trade 24/7 — outside hours, strategy must operate conservatively.
#[derive(Debug, Clone)]
pub enum ReferencePrice {
    /// Live data from IBKR — use directly.
    Live(IbkrMarketData),
    /// IBKR recently went stale (e.g. market just closed).
    Stale {
        last: IbkrMarketData,
        age: std::time::Duration,
    },
    /// No recent IBKR data — overnight or weekend.
    Unavailable,
}

// ─── Connection state ────────────────────────────────────────────────────────

/// Published by each driver via watch channel.
/// Watchdog subscribes to detect and react to connectivity issues.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Connected,
    Reconnecting {
        since: Instant,
        attempts: u32,
    },
    Failed {
        reason: String,
    },
}

// ─── Order result ────────────────────────────────────────────────────────────

/// Response to an order placement attempt.
/// Returned via oneshot channel to the caller (strategy).
#[derive(Debug)]
pub enum OrderResult {
    Acknowledged { order_id: OrderId },
    Rejected { reason: String },
    Timeout,
}

// ─── Exchange driver trait ───────────────────────────────────────────────────

/// Unified interface for all venues.
/// Protocol-specific complexity is fully encapsulated in each implementation.
/// Adding a third venue requires only implementing this trait —
/// no changes to strategy, order_book, hedge_manager, or watchdog.
#[async_trait]
pub trait ExchangeDriver: Send + Sync {
    /// Establish connection to the venue.
    async fn connect(&mut self) -> Result<(), DriverError>;

    /// Gracefully disconnect.
    async fn disconnect(&mut self) -> Result<(), DriverError>;

    /// Subscribe to market data feed.
    /// Data is published via watch channels injected at construction.
    async fn subscribe_market_data(&mut self) -> Result<(), DriverError>;

    /// Place an order. Returns the venue-assigned OrderId on success.
    /// IBKR only supports Market orders — Limit orders return NotSupported.
    async fn place_order(&mut self, order: Order) -> Result<OrderId, DriverError>;

    /// Cancel an open order by ID.
    async fn cancel_order(&mut self, id: &OrderId) -> Result<(), DriverError>;

    /// Query current positions from the venue.
    /// Used for startup reconciliation and periodic external reconciliation.
    async fn get_positions(&self) -> Result<Vec<Position>, DriverError>;

    /// What this venue supports. Checked before calling optional operations.
    fn capabilities(&self) -> VenueCapabilities;

    /// Human-readable venue name for logging.
    fn venue_name(&self) -> &'static str;
}

// ─── Driver errors ───────────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum DriverError {
    #[error("Not connected")]
    NotConnected,

    #[error("Not supported by this venue")]
    NotSupported,

    #[error("Order rejected by venue: {reason}")]
    OrderRejected { reason: String },

    #[error("Order not found: {id}")]
    OrderNotFound { id: OrderId },

    #[error("Connection error: {0}")]
    Connection(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Timeout after {ms}ms")]
    Timeout { ms: u64 },
}