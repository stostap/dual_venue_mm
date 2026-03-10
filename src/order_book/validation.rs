use crate::order_book::BookSnapshot;

/// Result of snapshot validation pipeline.
#[derive(Debug, PartialEq)]
pub enum ValidationResult {
    /// Snapshot is valid — publish normally.
    Ok,
    /// Snapshot has anomalies but is usable — publish with warning.
    Warn(Vec<ValidationWarning>),
    /// Snapshot is invalid — do not publish, mark book as invalid.
    Reject(ValidationRejection),
}

#[derive(Debug, PartialEq)]
pub enum ValidationWarning {
    OneSideEmpty { side: &'static str },
    AbnormalSpread { spread_bps: i64, threshold_bps: i64 },
    LargePrice { mid_move_bps: i64, threshold_bps: i64 },
    NegativeQuantityDropped { count: usize },
    SortOrderViolated,
}

#[derive(Debug, PartialEq)]
pub enum ValidationRejection {
    CrossedBook { best_bid: i64, best_ask: i64 },
    BothSidesEmpty,
}

pub struct ValidationConfig {
    /// Maximum acceptable spread in basis points before warning.
    pub max_spread_bps: i64,
    /// Maximum mid price move vs last snapshot before warning.
    pub max_mid_move_bps: i64,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            max_spread_bps: 500,   // 5% spread is suspicious
            max_mid_move_bps: 200, // 2% move between snapshots is suspicious
        }
    }
}

/// Run the full validation pipeline on a snapshot.
/// Returns Ok, Warn (publish with caveats), or Reject (do not publish).
pub fn validate(
    snapshot: &BookSnapshot,
    last_mid: Option<i64>,
    config: &ValidationConfig,
) -> ValidationResult {
    // ── Hard rejections ──────────────────────────────────────────────────────

    if snapshot.bids.is_empty() && snapshot.asks.is_empty() {
        return ValidationResult::Reject(ValidationRejection::BothSidesEmpty);
    }

    if snapshot.is_crossed() {
        return ValidationResult::Reject(ValidationRejection::CrossedBook {
            best_bid: snapshot.best_bid().unwrap_or(0),
            best_ask: snapshot.best_ask().unwrap_or(0),
        });
    }

    // ── Warnings (publish but flag) ──────────────────────────────────────────

    let mut warnings = Vec::new();

    if snapshot.bids.is_empty() {
        warnings.push(ValidationWarning::OneSideEmpty { side: "bids" });
    }
    if snapshot.asks.is_empty() {
        warnings.push(ValidationWarning::OneSideEmpty { side: "asks" });
    }

    // Spread check
    if let (Some(bid), Some(ask)) = (snapshot.best_bid(), snapshot.best_ask()) {
        if let Some(mid) = snapshot.mid() {
            if mid > 0 {
                let spread_bps = (ask - bid) * 10_000 / mid;
                if spread_bps > config.max_spread_bps {
                    warnings.push(ValidationWarning::AbnormalSpread {
                        spread_bps,
                        threshold_bps: config.max_spread_bps,
                    });
                }

                // Price continuity check vs last snapshot
                if let Some(last) = last_mid {
                    if last > 0 {
                        let move_bps = (mid - last).abs() * 10_000 / last;
                        if move_bps > config.max_mid_move_bps {
                            warnings.push(ValidationWarning::LargePrice {
                                mid_move_bps: move_bps,
                                threshold_bps: config.max_mid_move_bps,
                            });
                        }
                    }
                }
            }
        }
    }

    if warnings.is_empty() {
        ValidationResult::Ok
    } else {
        ValidationResult::Warn(warnings)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order_book::{BookSnapshot, PriceLevel};

    fn make_snapshot(bids: Vec<(i64, i64)>, asks: Vec<(i64, i64)>) -> BookSnapshot {
        BookSnapshot::new(
            bids.into_iter().map(|(p, q)| PriceLevel { price: p, quantity: q }).collect(),
            asks.into_iter().map(|(p, q)| PriceLevel { price: p, quantity: q }).collect(),
            "TSLA".to_string(),
            1,
        )
    }

    #[test]
    fn valid_book_passes() {
        let snap = make_snapshot(vec![(1800, 100)], vec![(1810, 100)]);
        assert_eq!(validate(&snap, None, &ValidationConfig::default()), ValidationResult::Ok);
    }

    #[test]
    fn rejects_both_sides_empty() {
        let snap = make_snapshot(vec![], vec![]);
        assert!(matches!(
            validate(&snap, None, &ValidationConfig::default()),
            ValidationResult::Reject(ValidationRejection::BothSidesEmpty)
        ));
    }

    #[test]
    fn rejects_crossed_book() {
        // bid > ask = crossed
        let snap = make_snapshot(vec![(1820, 100)], vec![(1810, 100)]);
        assert!(matches!(
            validate(&snap, None, &ValidationConfig::default()),
            ValidationResult::Reject(ValidationRejection::CrossedBook { .. })
        ));
    }

    #[test]
    fn warns_on_abnormal_spread() {
        let config = ValidationConfig { max_spread_bps: 10, max_mid_move_bps: 200 };
        // spread = (2000 - 1000) / 1500 * 10000 = 6666 bps >> 10
        let snap = make_snapshot(vec![(1000, 100)], vec![(2000, 100)]);
        assert!(matches!(
            validate(&snap, None, &config),
            ValidationResult::Warn(_)
        ));
    }

    #[test]
    fn warns_on_large_price_move() {
        let config = ValidationConfig { max_spread_bps: 500, max_mid_move_bps: 50 };
        let snap = make_snapshot(vec![(1800, 100)], vec![(1810, 100)]);
        // last_mid = 1000, new mid ~1805 → move = 805/1000 * 10000 = 8050 bps >> 50
        assert!(matches!(
            validate(&snap, Some(1000), &config),
            ValidationResult::Warn(_)
        ));
    }
}