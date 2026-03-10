# Dual-Venue Market Making System

Rust implementation of a dual-venue market making infrastructure for
Hyperliquid (HL) perpetual futures with IBKR hedging.

## Prerequisites

- [Docker Desktop](https://www.docker.com/products/docker-desktop/) installed and running

That's it. No Rust installation needed locally.

## Quick Start

```bash
# 1. Clone / download the project
cd dual_venue_mm

# 2. Build the Docker image (first time: ~2-3 min, downloads Rust + deps)
docker compose build

# 3. Run the test suite
docker compose run rust-dev cargo test

# 4. Run tests with output (see println! and tracing logs)
docker compose run rust-dev cargo test -- --nocapture

# 5. Run a specific test
docker compose run rust-dev cargo test order_book

# 6. Open an interactive shell inside the container
docker compose run rust-dev bash
# then inside: cargo test, cargo build, cargo check, etc.
```

## Auto-recompile on file changes (watch mode)

```bash
# Inside the container shell:
cargo watch -x test

# Or directly:
docker compose run rust-dev cargo watch -x test
```

Source files are mounted as a volume — edits on your host machine are
immediately visible inside the container without rebuilding the image.

## Project Structure

```
src/
  lib.rs                    # module declarations
  main.rs                   # entry point
  types.rs                  # Price, Quantity, InstrumentSpec, Order, Fill
  order_book/
    mod.rs                  # OrderBook, BookSnapshot, BookUpdate (Part 2a)
    validation.rs           # snapshot validation pipeline
  exchange/
    mod.rs                  # ExchangeDriver trait, VenueCapabilities (Part 2b)
    hyperliquid.rs          # HL WebSocket driver implementation
    ibkr.rs                 # IBKR TCP driver stub
```

## Running Tests

```
cargo test                          # all tests
cargo test types                    # tests in types.rs
cargo test order_book               # all order book tests
cargo test snapshot_best_bid_ask    # specific test by name
cargo test -- --nocapture           # show log output
```

## Architecture

See the architecture document (arch_doc_v2.docx) for full design rationale,
component diagrams, and decision records.
