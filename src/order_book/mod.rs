pub mod validation;

use std::time::{Duration, Instant};
use thiserror::Error;

use crate::types::{Price, Quantity, Symbol};
use validation::{validate, ValidationConfig, ValidationResult};

// ─── Data structures ─────────────────────────────────────────────────────────

/// A single price level in the order book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceLevel {
    pub price: Price,
    pub quantity: Quantity,
}

/// An immutable, cheaply cloneable snapshot of the full order book.
/// Published via watch::Sender after every successful update.
/// Bids are sorted descending (best bid at index 0).
/// Asks are sorted ascending  (best ask at index 0).
#[derive(Debug, Clone)]
pub struct BookSnapshot {
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub instrument: Symbol,
    pub sequence: u64,
    pub timestamp: Instant,
}

impl BookSnapshot {
    pub fn new(
        mut bids: Vec<PriceLevel>,
        mut asks: Vec<PriceLevel>,
        instrument: Symbol,
        sequence: u64,
    ) -> Self {
        // Ensure correct sort order regardless of input
        bids.sort_by(|a, b| b.price.cmp(&a.price)); // descending
        asks.sort_by(|a, b| a.price.cmp(&b.price)); // ascending

        // Drop any levels with non-positive quantity
        bids.retain(|l| l.quantity > 0);
        asks.retain(|l| l.quantity > 0);

        Self {
            bids,
            asks,
            instrument,
            sequence,
            timestamp: Instant::now(),
        }
    }

    /// Best bid price — O(1).
    pub fn best_bid(&self) -> Option<Price> {
        self.bids.first().map(|l| l.price)
    }

    /// Best ask price — O(1).
    pub fn best_ask(&self) -> Option<Price> {
        self.asks.first().map(|l| l.price)
    }

    /// Mid price — (best_bid + best_ask) / 2.
    /// Uses integer division — truncates. Strategy should use
    /// (best_bid + best_ask) directly when full precision matters.
    pub fn mid(&self) -> Option<Price> {
        Some((self.best_bid()? + self.best_ask()?) / 2)
    }

    /// Spread in raw price units.
    pub fn spread(&self) -> Option<Price> {
        Some(self.best_ask()? - self.best_bid()?)
    }

    /// Top N levels from each side. Returns fewer than N if book is shallow.
    pub fn depth(&self, n: usize) -> (&[PriceLevel], &[PriceLevel]) {
        let bid_depth = n.min(self.bids.len());
        let ask_depth = n.min(self.asks.len());
        (&self.bids[..bid_depth], &self.asks[..ask_depth])
    }

    /// True if best_bid >= best_ask — indicates a corrupted or crossed book.
    pub fn is_crossed(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => bid >= ask,
            _ => false,
        }
    }

    /// True if the snapshot has not been updated within the given duration.
    pub fn is_stale(&self, threshold: Duration) -> bool {
        self.timestamp.elapsed() > threshold
    }
}

// ─── Book update types ───────────────────────────────────────────────────────

/// Unified update type consumed by the order book task.
/// Both L2 (full snapshot) and L4 (incremental delta) feeds
/// are normalized to this type by their respective adapters.
#[derive(Debug)]
pub enum BookUpdate {
    /// L2: replace the entire book.
    Full {
        bids: Vec<PriceLevel>,
        asks: Vec<PriceLevel>,
    },
    /// L4: apply incremental changes.
    /// A level with quantity=0 means remove that price level.
    Delta {
        bid_changes: Vec<PriceLevel>,
        ask_changes: Vec<PriceLevel>,
    },
}

// ─── Book state ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookState {
    /// Book is valid and being published.
    Valid,
    /// Book is invalid — sequence gap, crossed book, or validation failure.
    /// Not publishing until next successful full snapshot.
    Invalid { reason: String },
}

// ─── Order book ──────────────────────────────────────────────────────────────

/// Maintains the internal order book from a Hyperliquid feed.
///
/// Supports both L2 (full snapshot) and L4 (incremental delta) ingestion.
/// Validates every snapshot before publishing.
/// Tracks sequence numbers to detect gaps.
pub struct OrderBook {
    instrument: Symbol,
    bids: Vec<PriceLevel>,
    asks: Vec<PriceLevel>,
    last_seq: u64,
    last_mid: Option<Price>,
    state: BookState,
    validation_config: ValidationConfig,
}

impl OrderBook {
    pub fn new(instrument: Symbol) -> Self {
        Self {
            instrument,
            bids: Vec::new(),
            asks: Vec::new(),
            last_seq: 0,
            last_mid: None,
            state: BookState::Invalid { reason: "not yet initialized".into() },
            validation_config: ValidationConfig::default(),
        }
    }

    pub fn with_validation_config(mut self, config: ValidationConfig) -> Self {
        self.validation_config = config;
        self
    }

    pub fn state(&self) -> &BookState {
        &self.state
    }

    pub fn is_valid(&self) -> bool {
        self.state == BookState::Valid
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_seq
    }

    /// Apply a BookUpdate with sequence number validation.
    /// Returns Ok(Some(snapshot)) if the update produces a publishable snapshot.
    /// Returns Ok(None) if the update was silently ignored (duplicate seq).
    /// Returns Err if the update caused a state transition to Invalid.
    pub fn apply(
        &mut self,
        update: BookUpdate,
        seq: u64,
    ) -> Result<Option<BookSnapshot>, BookError> {
        // ── Sequence validation ──────────────────────────────────────────────
        match self.check_sequence(seq) {
            SequenceCheck::Expected => {}
            SequenceCheck::Duplicate => {
                tracing::debug!(seq, "duplicate sequence number — ignoring");
                return Ok(None);
            }
            SequenceCheck::Gap { expected, got } => {
                let reason = format!("sequence gap: expected {expected}, got {got}");
                tracing::warn!(expected, got, "sequence gap detected");
                self.state = BookState::Invalid { reason: reason.clone() };
                // Advance last_seq to the received seq so the next L2 full snapshot
                // can recover — without this, every subsequent message looks like another gap.
                self.last_seq = got;
                return Err(BookError::SequenceGap { expected, got });
            }
            SequenceCheck::Regression { expected, got } => {
                let reason = format!("sequence regression: expected {expected}, got {got}");
                tracing::error!(expected, got, "sequence regression detected");
                self.state = BookState::Invalid { reason: reason.clone() };
                return Err(BookError::SequenceRegression { expected, got });
            }
        }

        // ── Apply the update ─────────────────────────────────────────────────
        match update {
            BookUpdate::Full { bids, asks } => {
                self.apply_full(bids, asks);
            }
            BookUpdate::Delta { bid_changes, ask_changes } => {
                // Delta updates are only safe when book is already valid.
                // If we're in Invalid state, we must wait for a Full snapshot.
                if !self.is_valid() {
                    tracing::debug!("ignoring delta update — book is invalid, awaiting full snapshot");
                    self.last_seq = seq; // advance seq so we don't get stuck
                    return Ok(None);
                }
                self.apply_delta(bid_changes, ask_changes);
            }
        }

        self.last_seq = seq;

        // ── Build and validate snapshot ──────────────────────────────────────
        let snapshot = BookSnapshot::new(
            self.bids.clone(),
            self.asks.clone(),
            self.instrument.clone(),
            seq,
        );

        match validate(&snapshot, self.last_mid, &self.validation_config) {
            ValidationResult::Ok => {
                self.last_mid = snapshot.mid();
                self.state = BookState::Valid;
                Ok(Some(snapshot))
            }
            ValidationResult::Warn(warnings) => {
                for w in &warnings {
                    tracing::warn!(?w, "book snapshot validation warning");
                }
                self.last_mid = snapshot.mid();
                self.state = BookState::Valid;
                Ok(Some(snapshot))
            }
            ValidationResult::Reject(rejection) => {
                tracing::error!(?rejection, "book snapshot rejected");
                self.state = BookState::Invalid {
                    reason: format!("{rejection:?}"),
                };
                Err(BookError::ValidationRejected(format!("{rejection:?}")))
            }
        }
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    fn apply_full(&mut self, bids: Vec<PriceLevel>, asks: Vec<PriceLevel>) {
        self.bids = bids;
        self.asks = asks;
        // Sort: bids descending, asks ascending
        self.bids.sort_by(|a, b| b.price.cmp(&a.price));
        self.asks.sort_by(|a, b| a.price.cmp(&b.price));
        // Drop non-positive quantities
        self.bids.retain(|l| l.quantity > 0);
        self.asks.retain(|l| l.quantity > 0);
    }

    fn apply_delta(&mut self, bid_changes: Vec<PriceLevel>, ask_changes: Vec<PriceLevel>) {
        apply_side_delta(&mut self.bids, bid_changes);
        apply_side_delta(&mut self.asks, ask_changes);
        // Re-sort after delta
        self.bids.sort_by(|a, b| b.price.cmp(&a.price));
        self.asks.sort_by(|a, b| a.price.cmp(&b.price));
    }

    fn check_sequence(&self, seq: u64) -> SequenceCheck {
        // First update — any sequence is acceptable
        if self.last_seq == 0 {
            return SequenceCheck::Expected;
        }
        let expected = self.last_seq + 1;
        match seq.cmp(&expected) {
            std::cmp::Ordering::Equal => SequenceCheck::Expected,
            std::cmp::Ordering::Less => {
                if seq == self.last_seq {
                    SequenceCheck::Duplicate
                } else {
                    SequenceCheck::Regression { expected, got: seq }
                }
            }
            std::cmp::Ordering::Greater => SequenceCheck::Gap { expected, got: seq },
        }
    }
}

/// Apply a set of delta changes to one side of the book.
/// quantity=0 means remove the level.
fn apply_side_delta(side: &mut Vec<PriceLevel>, changes: Vec<PriceLevel>) {
    for change in changes {
        match side.iter().position(|l| l.price == change.price) {
            Some(idx) => {
                if change.quantity == 0 {
                    side.remove(idx);
                } else {
                    side[idx].quantity = change.quantity;
                }
            }
            None => {
                if change.quantity > 0 {
                    side.push(change);
                }
                // quantity=0 with no existing level — nothing to remove
            }
        }
    }
}

// ─── Sequence check result ───────────────────────────────────────────────────

enum SequenceCheck {
    Expected,
    Duplicate,
    Gap { expected: u64, got: u64 },
    Regression { expected: u64, got: u64 },
}

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum BookError {
    #[error("Sequence gap: expected {expected}, got {got}")]
    SequenceGap { expected: u64, got: u64 },

    #[error("Sequence regression: expected {expected}, got {got}")]
    SequenceRegression { expected: u64, got: u64 },

    #[error("Snapshot validation rejected: {0}")]
    ValidationRejected(String),
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn level(price: i64, quantity: i64) -> PriceLevel {
        PriceLevel { price, quantity }
    }

    fn full_update(bids: Vec<(i64, i64)>, asks: Vec<(i64, i64)>) -> BookUpdate {
        BookUpdate::Full {
            bids: bids.into_iter().map(|(p, q)| level(p, q)).collect(),
            asks: asks.into_iter().map(|(p, q)| level(p, q)).collect(),
        }
    }

    fn delta_update(bids: Vec<(i64, i64)>, asks: Vec<(i64, i64)>) -> BookUpdate {
        BookUpdate::Delta {
            bid_changes: bids.into_iter().map(|(p, q)| level(p, q)).collect(),
            ask_changes: asks.into_iter().map(|(p, q)| level(p, q)).collect(),
        }
    }

    // ── BookSnapshot query tests ─────────────────────────────────────────────

    #[test]
    fn snapshot_best_bid_ask() {
        let snap = BookSnapshot::new(
            vec![level(1800, 100), level(1790, 200), level(1780, 300)],
            vec![level(1810, 100), level(1820, 200)],
            "TSLA".into(),
            1,
        );
        assert_eq!(snap.best_bid(), Some(1800));
        assert_eq!(snap.best_ask(), Some(1810));
    }

    #[test]
    fn snapshot_mid_price() {
        let snap = BookSnapshot::new(
            vec![level(1800, 100)],
            vec![level(1810, 100)],
            "TSLA".into(),
            1,
        );
        assert_eq!(snap.mid(), Some(1805));
    }

    #[test]
    fn snapshot_mid_truncates() {
        // (1800 + 1801) / 2 = 1800 (integer division truncates)
        let snap = BookSnapshot::new(
            vec![level(1800, 100)],
            vec![level(1801, 100)],
            "TSLA".into(),
            1,
        );
        assert_eq!(snap.mid(), Some(1800));
    }

    #[test]
    fn snapshot_depth() {
        let snap = BookSnapshot::new(
            vec![level(1800, 100), level(1790, 200), level(1780, 300)],
            vec![level(1810, 100), level(1820, 200), level(1830, 300)],
            "TSLA".into(),
            1,
        );
        let (bids, asks) = snap.depth(2);
        assert_eq!(bids.len(), 2);
        assert_eq!(asks.len(), 2);
        assert_eq!(bids[0].price, 1800); // best bid first
        assert_eq!(asks[0].price, 1810); // best ask first
    }

    #[test]
    fn snapshot_depth_capped_at_book_size() {
        let snap = BookSnapshot::new(
            vec![level(1800, 100)],
            vec![level(1810, 100)],
            "TSLA".into(),
            1,
        );
        let (bids, asks) = snap.depth(10);
        assert_eq!(bids.len(), 1); // only 1 level available
        assert_eq!(asks.len(), 1);
    }

    #[test]
    fn snapshot_is_crossed() {
        let snap = BookSnapshot::new(
            vec![level(1820, 100)], // bid > ask = crossed
            vec![level(1810, 100)],
            "TSLA".into(),
            1,
        );
        assert!(snap.is_crossed());
    }

    #[test]
    fn snapshot_not_crossed() {
        let snap = BookSnapshot::new(
            vec![level(1800, 100)],
            vec![level(1810, 100)],
            "TSLA".into(),
            1,
        );
        assert!(!snap.is_crossed());
    }

    #[test]
    fn snapshot_sorts_unsorted_input() {
        // Input bids are in wrong order — should be sorted descending
        let snap = BookSnapshot::new(
            vec![level(1780, 300), level(1800, 100), level(1790, 200)],
            vec![level(1830, 300), level(1810, 100), level(1820, 200)],
            "TSLA".into(),
            1,
        );
        assert_eq!(snap.bids[0].price, 1800);
        assert_eq!(snap.bids[1].price, 1790);
        assert_eq!(snap.bids[2].price, 1780);
        assert_eq!(snap.asks[0].price, 1810);
        assert_eq!(snap.asks[1].price, 1820);
        assert_eq!(snap.asks[2].price, 1830);
    }

    #[test]
    fn snapshot_drops_zero_quantity_levels() {
        let snap = BookSnapshot::new(
            vec![level(1800, 100), level(1790, 0)], // zero qty should be dropped
            vec![level(1810, 0), level(1820, 100)],
            "TSLA".into(),
            1,
        );
        assert_eq!(snap.bids.len(), 1);
        assert_eq!(snap.asks.len(), 1);
        assert_eq!(snap.bids[0].price, 1800);
        assert_eq!(snap.asks[0].price, 1820);
    }

    #[test]
    fn snapshot_empty_sides_return_none() {
        let snap = BookSnapshot::new(vec![], vec![], "TSLA".into(), 1);
        assert_eq!(snap.best_bid(), None);
        assert_eq!(snap.best_ask(), None);
        assert_eq!(snap.mid(), None);
    }

    #[test]
    fn snapshot_staleness() {
        let snap = BookSnapshot::new(vec![level(1800, 100)], vec![level(1810, 100)], "TSLA".into(), 1);
        // Just created — should not be stale for 1 second threshold
        assert!(!snap.is_stale(Duration::from_secs(1)));
        // Should be stale for 0 duration
        assert!(snap.is_stale(Duration::from_nanos(0)));
    }

    // ── OrderBook apply tests ────────────────────────────────────────────────

    #[test]
    fn apply_full_snapshot_initializes_book() {
        let mut book = OrderBook::new("TSLA".into());
        let result = book.apply(full_update(vec![(1800, 100)], vec![(1810, 100)]), 1);
        assert!(result.is_ok());
        let snap = result.unwrap().expect("should produce snapshot");
        assert_eq!(snap.best_bid(), Some(1800));
        assert_eq!(snap.best_ask(), Some(1810));
        assert!(book.is_valid());
    }

    #[test]
    fn apply_full_snapshot_replaces_book() {
        let mut book = OrderBook::new("TSLA".into());
        book.apply(full_update(vec![(1800, 100)], vec![(1810, 100)]), 1).unwrap();
        // Second snapshot with completely different levels
        let snap = book
            .apply(full_update(vec![(1850, 200)], vec![(1860, 200)]), 2)
            .unwrap()
            .unwrap();
        assert_eq!(snap.best_bid(), Some(1850));
        assert_eq!(snap.best_ask(), Some(1860));
    }

    #[test]
    fn apply_delta_updates_existing_level() {
        let mut book = OrderBook::new("TSLA".into());
        book.apply(full_update(vec![(1800, 100)], vec![(1810, 100)]), 1).unwrap();
        let snap = book
            .apply(delta_update(vec![(1800, 200)], vec![]), 2) // update bid qty
            .unwrap()
            .unwrap();
        assert_eq!(snap.bids[0].quantity, 200);
    }

    #[test]
    fn apply_delta_removes_level_on_zero_quantity() {
        let mut book = OrderBook::new("TSLA".into());
        book.apply(
            full_update(vec![(1800, 100), (1790, 200)], vec![(1810, 100)]),
            1,
        ).unwrap();
        // Remove the 1800 bid level
        let snap = book
            .apply(delta_update(vec![(1800, 0)], vec![]), 2)
            .unwrap()
            .unwrap();
        assert_eq!(snap.bids.len(), 1);
        assert_eq!(snap.best_bid(), Some(1790));
    }

    #[test]
    fn apply_delta_adds_new_level() {
        let mut book = OrderBook::new("TSLA".into());
        book.apply(full_update(vec![(1800, 100)], vec![(1810, 100)]), 1).unwrap();
        let snap = book
            .apply(delta_update(vec![(1795, 150)], vec![]), 2) // new level
            .unwrap()
            .unwrap();
        assert_eq!(snap.bids.len(), 2);
        assert_eq!(snap.bids[0].price, 1800); // still best bid
        assert_eq!(snap.bids[1].price, 1795); // new level
    }

    #[test]
    fn apply_duplicate_sequence_ignored() {
        let mut book = OrderBook::new("TSLA".into());
        book.apply(full_update(vec![(1800, 100)], vec![(1810, 100)]), 1).unwrap();
        // Same seq again — should be silently ignored
        let result = book.apply(full_update(vec![(9999, 1)], vec![(9999, 1)]), 1).unwrap();
        assert!(result.is_none()); // no snapshot produced
        // Book should still have original values
        assert!(book.is_valid());
    }

    #[test]
    fn apply_sequence_gap_marks_invalid() {
        let mut book = OrderBook::new("TSLA".into());
        book.apply(full_update(vec![(1800, 100)], vec![(1810, 100)]), 1).unwrap();
        // Skip seq 2, jump to 5 — gap
        let err = book.apply(full_update(vec![(1800, 100)], vec![(1810, 100)]), 5);
        assert!(matches!(err, Err(BookError::SequenceGap { expected: 2, got: 5 })));
        assert!(!book.is_valid());
    }

    #[test]
    fn apply_full_snapshot_recovers_from_invalid() {
        let mut book = OrderBook::new("TSLA".into());
        book.apply(full_update(vec![(1800, 100)], vec![(1810, 100)]), 1).unwrap();
        // Force invalid via gap
        let _ = book.apply(full_update(vec![(1800, 100)], vec![(1810, 100)]), 5);
        assert!(!book.is_valid());
        // Full snapshot (L2) should recover the book — seq 6 is next after 5
        let snap = book
            .apply(full_update(vec![(1850, 100)], vec![(1860, 100)]), 6)
            .unwrap()
            .unwrap();
        assert!(book.is_valid());
        assert_eq!(snap.best_bid(), Some(1850));
    }

    #[test]
    fn apply_delta_ignored_when_book_invalid() {
        let mut book = OrderBook::new("TSLA".into());
        // Book never initialized — still invalid
        let result = book.apply(delta_update(vec![(1800, 100)], vec![]), 1).unwrap();
        // Delta ignored — book still invalid, no snapshot
        assert!(result.is_none());
    }

    #[test]
    fn apply_crossed_book_rejected() {
        let mut book = OrderBook::new("TSLA".into());
        // bid > ask = crossed — should be rejected
        let err = book.apply(full_update(vec![(1820, 100)], vec![(1810, 100)]), 1);
        assert!(matches!(err, Err(BookError::ValidationRejected(_))));
        assert!(!book.is_valid());
    }

    #[test]
    fn apply_both_sides_empty_rejected() {
        let mut book = OrderBook::new("TSLA".into());
        let err = book.apply(full_update(vec![], vec![]), 1);
        assert!(matches!(err, Err(BookError::ValidationRejected(_))));
    }

    #[test]
    fn spread_calculation() {
        let snap = BookSnapshot::new(
            vec![level(1800, 100)],
            vec![level(1810, 100)],
            "TSLA".into(),
            1,
        );
        assert_eq!(snap.spread(), Some(10));
    }
}

// ─── Order book task ──────────────────────────────────────────────────────────

/// Message sent from hl_feed task to order_book task.
/// Contains raw parsed levels — no normalization decisions here,
/// that already happened at the driver boundary.
pub struct RawBookUpdate {
    pub update: BookUpdate,
    pub seq: u64,
}

/// Runs the order book task.
///
/// Receives `RawBookUpdate` from the hl_feed task via mpsc,
/// applies to internal `OrderBook`, and publishes `BookSnapshot`
/// via watch channel to all downstream consumers
/// (strategy, watchdog, data_recorder).
///
/// This is a long-running task — it runs for the lifetime of the process.
/// If the mpsc sender is dropped (hl_feed shuts down), this task exits.
pub async fn run_order_book_task(
    instrument: Symbol,
    mut update_rx: tokio::sync::mpsc::Receiver<RawBookUpdate>,
    snapshot_tx: tokio::sync::watch::Sender<Option<BookSnapshot>>,
) {
    let mut book = OrderBook::new(instrument.clone());
    tracing::info!(instrument = %instrument, "order_book task started");

    while let Some(raw) = update_rx.recv().await {
        match book.apply(raw.update, raw.seq) {
            Ok(Some(snapshot)) => {
                tracing::debug!(
                    seq = snapshot.sequence,
                    bid = ?snapshot.best_bid(),
                    ask = ?snapshot.best_ask(),
                    "book snapshot published"
                );
                // Ignore error — means all receivers dropped (shutdown)
                let _ = snapshot_tx.send(Some(snapshot));
            }
            Ok(None) => {
                // Duplicate sequence — no-op
            }
            Err(e) => {
                tracing::error!(error = %e, "book update rejected — publishing None");
                let _ = snapshot_tx.send(None);
            }
        }
    }

    tracing::info!(instrument = %instrument, "order_book task exiting — update channel closed");
}