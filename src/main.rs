//! Integration harness — wires all components together and runs the system.
//!
//! Component topology (matches architecture doc):
//!
//!   HlDriver (hl_feed task)
//!       │  mpsc::Sender<RawBookUpdate>
//!       ▼
//!   order_book task  ←── owns OrderBook state
//!       │  watch::Sender<Option<BookSnapshot>>
//!       ├──► book_consumer task  (simulates world_view)
//!       └──► connection_monitor  (simulates watchdog)

use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

use dual_venue_mm::exchange::hyperliquid::HlDriver;
use dual_venue_mm::exchange::{ConnectionState, ExchangeDriver};
use dual_venue_mm::order_book::{run_order_book_task, BookSnapshot};
use dual_venue_mm::types::InstrumentSpec;

// ─── Configuration ────────────────────────────────────────────────────────────

const INSTRUMENT: &str = "xyz:TSLA";
const PRICE_SCALE: u32 = 2;
const QTY_SCALE: u32 = 3;
const STALE_THRESHOLD: Duration = Duration::from_secs(5);
const RUN_DURATION: Duration = Duration::from_secs(30);

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    tracing::info!("=== dual_venue_mm integration test ===");
    tracing::info!(instrument = INSTRUMENT, "starting up");

    let spec = InstrumentSpec::new(INSTRUMENT, PRICE_SCALE, QTY_SCALE);

    // ── Channel topology ─────────────────────────────────────────────────────
    //
    //  HlDriver ──(mpsc RawBookUpdate)──► order_book task
    //                                         │
    //                                   (watch BookSnapshot)
    //                                         ├──► book_consumer
    //                                         └──► (future: strategy, watchdog, recorder)
    //
    //  HlDriver ──(watch ConnectionState)──► connection_monitor

    let (update_tx, update_rx) = mpsc::channel(256);
    let (snapshot_tx, snapshot_rx) = watch::channel::<Option<BookSnapshot>>(None);
    let (conn_tx, conn_rx) = watch::channel(ConnectionState::Reconnecting {
        since: Instant::now(),
        attempts: 0,
    });

    // ── Build and connect HlDriver ───────────────────────────────────────────
    let mut hl_driver = HlDriver::new(spec.clone(), update_tx, conn_tx);

    tracing::info!("connecting to Hyperliquid WebSocket...");
    if let Err(e) = hl_driver.connect().await {
        tracing::error!(error = %e, "failed to connect");
        std::process::exit(1);
    }
    if let Err(e) = hl_driver.subscribe_market_data().await {
        tracing::error!(error = %e, "failed to subscribe");
        std::process::exit(1);
    }

    // ── Spawn order_book task ────────────────────────────────────────────────
    // Owns OrderBook state. Receives RawBookUpdate from HlDriver,
    // applies to book, publishes BookSnapshot via watch.
    tokio::spawn(run_order_book_task(
        INSTRUMENT.to_string(),
        update_rx,
        snapshot_tx,
    ));

    // ── Spawn downstream consumer tasks ──────────────────────────────────────
    tokio::spawn(connection_monitor(conn_rx));
    tokio::spawn(book_consumer(snapshot_rx, spec.clone()));

    tracing::info!(seconds = RUN_DURATION.as_secs(), "running — press Ctrl+C to stop");
    tokio::time::sleep(RUN_DURATION).await;

    tracing::info!("run complete — disconnecting");
    hl_driver.disconnect().await.ok();
}

// ─── Connection monitor ───────────────────────────────────────────────────────

async fn connection_monitor(mut conn_rx: watch::Receiver<ConnectionState>) {
    loop {
        if conn_rx.changed().await.is_err() {
            break;
        }
        match &*conn_rx.borrow() {
            ConnectionState::Connected => {
                tracing::info!("✅ HL WebSocket connected");
            }
            ConnectionState::Reconnecting { attempts, .. } => {
                tracing::warn!(attempts, "⚠️  HL reconnecting...");
            }
            ConnectionState::Failed { reason } => {
                tracing::error!(reason, "❌ HL connection failed");
            }
        }
    }
}

// ─── Book consumer ────────────────────────────────────────────────────────────

async fn book_consumer(
    mut snapshot_rx: watch::Receiver<Option<BookSnapshot>>,
    spec: InstrumentSpec,
) {
    let mut count = 0u64;
    let start = Instant::now();

    loop {
        if snapshot_rx.changed().await.is_err() {
            break;
        }

        match snapshot_rx.borrow().clone() {
            None => {
                tracing::warn!("book invalid — watchdog would pull quotes here");
            }
            Some(snap) => {
                count += 1;
                let bid = snap.best_bid().map(|p| spec.denormalize_price(p));
                let ask = snap.best_ask().map(|p| spec.denormalize_price(p));
                let mid = snap.mid().map(|p| spec.denormalize_price(p));
                let spread = snap.spread().map(|p| spec.denormalize_price(p));
                let (bid_depth, ask_depth) = snap.depth(3);

                tracing::info!(
                    seq        = snap.sequence,
                    bid        = ?bid,
                    ask        = ?ask,
                    mid        = ?mid,
                    spread     = ?spread,
                    bid_levels = bid_depth.len(),
                    ask_levels = ask_depth.len(),
                    stale      = snap.is_stale(STALE_THRESHOLD),
                    "📖 book snapshot #{count}"
                );

                if count % 10 == 0 {
                    let rate = count as f64 / start.elapsed().as_secs_f64();
                    tracing::info!(
                        snapshots = count,
                        rate = format!("{:.2} snaps/s", rate),
                        "📊 throughput"
                    );
                }
            }
        }
    }
}