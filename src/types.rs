/// Core domain types for the dual-venue market making system.
///
/// All prices and quantities are normalized to fixed-point i64 at the
/// exchange driver boundary. No floating-point arithmetic occurs downstream.

// ─── Primitive type aliases ──────────────────────────────────────────────────

/// Normalized fixed-point price. Units depend on InstrumentSpec.price_scale.
/// e.g. with scale=4, the value 1804500 represents 180.4500
pub type Price = i64;

/// Normalized fixed-point quantity. Units depend on InstrumentSpec.quantity_scale.
pub type Quantity = i64;

/// Instrument symbol e.g. "TSLA"
pub type Symbol = String;

/// Unique order identifier assigned by the exchange
pub type OrderId = String;

// ─── Instrument specification ────────────────────────────────────────────────

/// Per-instrument normalization parameters.
/// Determined at startup as max(hl_precision, ibkr_precision) for each field.
/// Immutable after initialization — changing scale mid-session would
/// invalidate all stored prices.
#[derive(Debug, Clone)]
pub struct InstrumentSpec {
    pub symbol: Symbol,
    /// Number of decimal places for price normalization.
    /// e.g. 4 means prices are stored in units of 10^-4
    pub price_scale: u32,
    /// Number of decimal places for quantity normalization.
    pub quantity_scale: u32,
}

impl InstrumentSpec {
    pub fn new(symbol: impl Into<Symbol>, price_scale: u32, quantity_scale: u32) -> Self {
        Self {
            symbol: symbol.into(),
            price_scale,
            quantity_scale,
        }
    }

    /// Normalize a decimal price string to fixed-point i64.
    /// e.g. "180.45" with scale=4 → 1804500
    pub fn normalize_price(&self, raw: &str) -> Result<Price, NormalizationError> {
        normalize_decimal(raw, self.price_scale)
    }

    /// Normalize a decimal quantity string to fixed-point i64.
    pub fn normalize_quantity(&self, raw: &str) -> Result<Quantity, NormalizationError> {
        normalize_decimal(raw, self.quantity_scale)
    }

    /// Normalize an f64 price to fixed-point i64.
    /// Used for IBKR which returns float prices from its API.
    pub fn normalize_price_f64(&self, raw: f64) -> Price {
        (raw * 10f64.powi(self.price_scale as i32)).round() as i64
    }

    /// Convert a normalized price back to f64 for display/logging.
    pub fn denormalize_price(&self, price: Price) -> f64 {
        price as f64 / 10f64.powi(self.price_scale as i32)
    }
}

/// Normalize a decimal string to fixed-point i64 with the given scale.
fn normalize_decimal(raw: &str, scale: u32) -> Result<i64, NormalizationError> {
    let raw = raw.trim();
    let negative = raw.starts_with('-');
    let raw = if negative { &raw[1..] } else { raw };

    let (int_part, frac_part) = match raw.find('.') {
        Some(dot) => (&raw[..dot], &raw[dot + 1..]),
        None => (raw, ""),
    };

    let frac_len = frac_part.len() as u32;
    if frac_len > scale {
        return Err(NormalizationError::ExcessivePrecision {
            value: raw.to_string(),
            max_scale: scale,
            got_scale: frac_len,
        });
    }

    let int_val: i64 = int_part
        .parse()
        .map_err(|_| NormalizationError::InvalidNumber(raw.to_string()))?;

    let frac_val: i64 = if frac_part.is_empty() {
        0
    } else {
        frac_part
            .parse()
            .map_err(|_| NormalizationError::InvalidNumber(raw.to_string()))?
    };

    // Pad fractional part to full scale
    let padding = 10i64.pow(scale - frac_len);
    let result = int_val * 10i64.pow(scale) + frac_val * padding;

    Ok(if negative { -result } else { result })
}

// ─── Order side ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(&self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

// ─── Order types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderType {
    Limit { price: Price, post_only: bool },
    Market,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeInForce {
    Gtc, // Good Till Cancelled
    Ioc, // Immediate Or Cancel
    Fok, // Fill Or Kill
}

#[derive(Debug, Clone)]
pub struct Order {
    pub instrument: Symbol,
    pub side: Side,
    pub quantity: Quantity,
    pub order_type: OrderType,
    pub tif: TimeInForce,
}

// ─── Fill ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Fill {
    pub order_id: OrderId,
    pub instrument: Symbol,
    pub side: Side,
    pub price: Price,
    pub quantity: Quantity,
    pub timestamp: std::time::Instant,
}

// ─── Position ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Position {
    pub instrument: Symbol,
    /// Net position in normalized quantity units.
    /// Positive = long, negative = short.
    pub quantity: i64,
}

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(thiserror::Error, Debug)]
pub enum NormalizationError {
    #[error("Invalid number: {0}")]
    InvalidNumber(String),

    #[error("Value '{value}' has {got_scale} decimal places, max allowed is {max_scale}")]
    ExcessivePrecision {
        value: String,
        max_scale: u32,
        got_scale: u32,
    },
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> InstrumentSpec {
        InstrumentSpec::new("TSLA", 4, 2)
    }

    #[test]
    fn normalize_integer_price() {
        assert_eq!(spec().normalize_price("180").unwrap(), 1_800_000);
    }

    #[test]
    fn normalize_decimal_price() {
        assert_eq!(spec().normalize_price("180.45").unwrap(), 1_804_500);
    }

    #[test]
    fn normalize_full_precision_price() {
        assert_eq!(spec().normalize_price("180.4567").unwrap(), 1_804_567);
    }

    #[test]
    fn normalize_negative_price() {
        assert_eq!(spec().normalize_price("-10.5").unwrap(), -105_000);
    }

    #[test]
    fn normalize_pads_short_fractional() {
        // "180.4" with scale=4 → 1804000, not 18040
        assert_eq!(spec().normalize_price("180.4").unwrap(), 1_804_000);
    }

    #[test]
    fn normalize_f64_price() {
        let price = spec().normalize_price_f64(180.4567);
        assert_eq!(price, 1_804_567);
    }

    #[test]
    fn normalize_excessive_precision_errors() {
        // scale=4, but value has 5 decimal places
        assert!(spec().normalize_price("180.45678").is_err());
    }

    #[test]
    fn denormalize_roundtrip() {
        let spec = spec();
        let original = "180.4567";
        let normalized = spec.normalize_price(original).unwrap();
        let denormalized = spec.denormalize_price(normalized);
        assert!((denormalized - 180.4567f64).abs() < 1e-10);
    }

    #[test]
    fn opposite_side() {
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
    }
}