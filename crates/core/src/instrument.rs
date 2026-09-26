//! Instrument definitions: linear perpetuals and European cash-settled options.
//!
//! Both instrument families share one numeric scheme (see crate docs):
//!
//! * price ticks are quote-minor units per `1.0` base unit scaled by
//!   `tick_size_quote_minor`;
//! * quantity lots are base-minor units scaled by `lot_size_base_minor`;
//! * notional = `(ticks * tick_size) * (lots * lot_size) / 10^base_decimals`.

use crate::num::{mul_div, Rounding};
use crate::{CoreError, Symbol, TimestampMs};

/// Per-market funding parameters (BitMEX-style premium + interest model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingParams {
    /// Funding interval in ms. Industry standard: 8 hours.
    pub interval_ms: TimestampMs,
    /// Fixed interest-rate differential per interval, basis points.
    /// Defaults to 1 bp / 8h (~1.1% p.a.) mirroring BitMEX's 0.01%/8h.
    pub interest_rate_bps_per_interval: i64,
    /// Premium index is clamped to ±this many bps before entering the rate.
    pub premium_clamp_bps: u64,
    /// Final funding rate is clamped to ±this many bps per interval.
    pub rate_cap_bps: u64,
}

impl Default for FundingParams {
    fn default() -> Self {
        Self {
            interval_ms: 8 * 60 * 60 * 1000,
            interest_rate_bps_per_interval: 1,
            premium_clamp_bps: 5,
            rate_cap_bps: 75,
        }
    }
}

/// Linear perpetual market.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerpMarket {
    /// Instrument symbol, e.g. `BTC-PERP`.
    pub symbol: Symbol,
    /// Underlying base asset symbol, e.g. `BTC`. Drives oracle + margin grouping.
    pub base_symbol: String,
    /// Quote currency precision (minor units per major, e.g. 2 for USD cents).
    pub quote_decimals: u32,
    /// Base currency precision (minor units per major, e.g. 5 for 0.00001 BTC).
    pub base_decimals: u32,
    /// Price tick: quote minor units per `1.0` base. `$1.00` steps -> `100`.
    pub tick_size_quote_minor: u128,
    /// Quantity lot: base minor units per lot. `0.001 BTC` -> `100` @ 5dp.
    pub lot_size_base_minor: u128,
    /// Initial margin ratio for the perp leg (cross/portfolio margin floors),
    /// basis points.
    pub initial_margin_ratio_bps: u64,
    /// Maintenance margin ratio (liquidation trigger), basis points.
    pub maintenance_margin_ratio_bps: u64,
    /// Order placement price band around the mark, basis points.
    pub price_band_bps: u64,
    /// Largest single order accepted, in lots.
    pub max_order_lots: u64,
    /// Funding configuration.
    pub funding: FundingParams,
}

impl Default for PerpMarket {
    fn default() -> Self {
        // A BTC-PERP shaped default: $1.00 ticks, 0.001 BTC lots, 20x max.
        Self {
            symbol: "BTC-PERP".into(),
            base_symbol: "BTC".into(),
            quote_decimals: 2,
            base_decimals: 5,
            tick_size_quote_minor: 100,
            lot_size_base_minor: 100,
            initial_margin_ratio_bps: 500,
            maintenance_margin_ratio_bps: 375,
            price_band_bps: 1_000,
            max_order_lots: 100_000,
            funding: FundingParams::default(),
        }
    }
}

/// European option flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OptionKind {
    /// Right to buy at strike.
    Call,
    /// Right to sell at strike.
    Put,
}

impl OptionKind {
    /// Letter used in symbol suffix, Deribit-style (`C` / `P`).
    #[must_use]
    pub fn letter(self) -> char {
        match self {
            OptionKind::Call => 'C',
            OptionKind::Put => 'P',
        }
    }
}

/// Portfolio-margin knobs for option markets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionMarginParams {
    /// Short-option minimum charge: floor on margin for each short option,
    /// expressed in bps of spot per `1.0` base shorted (CME SPAN "SOMC").
    pub short_option_min_bps: u64,
    /// Liquidation fee charged on the liquidated side, bps of notional.
    pub liquidation_fee_bps: u64,
}

impl Default for OptionMarginParams {
    fn default() -> Self {
        Self {
            short_option_min_bps: 500, // 5% of spot per 1.0 base short
            liquidation_fee_bps: 125,
        }
    }
}

/// European, cash-settled option market.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionMarket {
    /// Instrument symbol, e.g. `BTC-20260327-80000-C`.
    pub symbol: Symbol,
    /// Underlying base symbol the option references (oracle key).
    pub base_symbol: String,
    /// Call or put.
    pub kind: OptionKind,
    /// Strike in quote minor units per `1.0` base. `$80,000.00` -> `8_000_000`.
    pub strike_quote_minor: u128,
    /// Expiry (ms epoch). Settlement uses the 30-minute TWAP ending at expiry.
    pub expiry_ts_ms: TimestampMs,
    /// Quote currency precision (minor units per major).
    pub quote_decimals: u32,
    /// Base currency precision (minor units per major).
    pub base_decimals: u32,
    /// Premium tick: quote minor units per `1.0` base. `$0.50` -> `50`.
    pub tick_size_quote_minor: u128,
    /// Contract lot: base minor units per lot (`0.01 BTC` -> `1_000` @ 5dp).
    pub lot_size_base_minor: u128,
    /// Order placement band around the mark premium, bps.
    pub price_band_bps: u64,
    /// Largest single order accepted, in lots.
    pub max_order_lots: u64,
    /// Margin parameters.
    pub margin: OptionMarginParams,
}

/// Uniform view over tradable instruments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instrument {
    /// Linear perpetual.
    Perp(PerpMarket),
    /// European option.
    Option(OptionMarket),
}

impl Default for OptionMarket {
    fn default() -> Self {
        // A BTC $80,000 call shaped default: $0.50 ticks, 0.01 BTC lots.
        Self {
            symbol: "BTC-19800-80000-C".into(),
            base_symbol: "BTC".into(),
            kind: OptionKind::Call,
            strike_quote_minor: 8_000_000,
            expiry_ts_ms: 0,
            quote_decimals: 2,
            base_decimals: 5,
            tick_size_quote_minor: 50,
            lot_size_base_minor: 1_000,
            price_band_bps: 2_000,
            max_order_lots: 10_000,
            margin: OptionMarginParams::default(),
        }
    }
}

impl Instrument {
    /// Instrument symbol.
    #[must_use]
    pub fn symbol(&self) -> &str {
        match self {
            Instrument::Perp(m) => &m.symbol,
            Instrument::Option(m) => &m.symbol,
        }
    }

    /// Base asset symbol (oracle key, margin grouping).
    #[must_use]
    pub fn base_symbol(&self) -> &str {
        match self {
            Instrument::Perp(m) => &m.base_symbol,
            Instrument::Option(m) => &m.base_symbol,
        }
    }

    /// Quote currency decimals.
    #[must_use]
    pub fn quote_decimals(&self) -> u32 {
        match self {
            Instrument::Perp(m) => m.quote_decimals,
            Instrument::Option(m) => m.quote_decimals,
        }
    }

    /// Base currency decimals.
    #[must_use]
    pub fn base_decimals(&self) -> u32 {
        match self {
            Instrument::Perp(m) => m.base_decimals,
            Instrument::Option(m) => m.base_decimals,
        }
    }

    /// Price tick size (quote minor per `1.0` base).
    #[must_use]
    pub fn tick_size(&self) -> u128 {
        match self {
            Instrument::Perp(m) => m.tick_size_quote_minor,
            Instrument::Option(m) => m.tick_size_quote_minor,
        }
    }

    /// Quantity lot size (base minor units).
    #[must_use]
    pub fn lot_size(&self) -> u128 {
        match self {
            Instrument::Perp(m) => m.lot_size_base_minor,
            Instrument::Option(m) => m.lot_size_base_minor,
        }
    }

    /// Order placement price band, bps.
    #[must_use]
    pub fn price_band_bps(&self) -> u64 {
        match self {
            Instrument::Perp(m) => m.price_band_bps,
            Instrument::Option(m) => m.price_band_bps,
        }
    }

    /// Maximum single-order size in lots.
    #[must_use]
    pub fn max_order_lots(&self) -> u64 {
        match self {
            Instrument::Perp(m) => m.max_order_lots,
            Instrument::Option(m) => m.max_order_lots,
        }
    }

    fn base_unit(&self) -> Option<u128> {
        10_u128.checked_pow(self.base_decimals())
    }

    /// Convert a tick price to quote-minor per `1.0` base unit.
    #[must_use]
    pub fn price_quote_minor(&self, ticks: u64) -> Option<u128> {
        u128::from(ticks).checked_mul(self.tick_size())
    }

    /// Convert a quote-minor per-base price to ticks (must be on-grid).
    #[must_use]
    pub fn ticks_from_quote_minor(&self, price_quote_minor: u128) -> Option<u64> {
        let t = self.tick_size();
        if t == 0 || price_quote_minor % t != 0 {
            return None;
        }
        let ticks = price_quote_minor / t;
        u64::try_from(ticks).ok()
    }

    /// Notional value of `qty_lots` at `price_ticks`, in quote minor units.
    ///
    /// Rounds half-up: a risk figure, never posted to a ledger directly.
    #[must_use]
    pub fn notional_quote_minor(&self, price_ticks: u64, qty_lots: u64) -> Option<u128> {
        let price = u128::from(price_ticks).checked_mul(self.tick_size())?;
        let base_minor = u128::from(qty_lots).checked_mul(self.lot_size())?;
        mul_div(
            price,
            base_minor,
            self.base_unit()?,
            Rounding::NearestHalfUp,
        )
    }

    /// Notional of a signed position at a mark given in quote-minor per
    /// `1.0` base. `qty_lots` sign: positive long, negative short.
    #[must_use]
    pub fn position_notional_minor(
        &self,
        mark_quote_minor_per_base: u128,
        signed_qty_lots: i64,
    ) -> Option<u128> {
        let abs_lots = u128::from(signed_qty_lots.unsigned_abs());
        let base_minor = abs_lots.checked_mul(self.lot_size())?;
        mul_div(
            mark_quote_minor_per_base,
            base_minor,
            self.base_unit()?,
            Rounding::NearestHalfUp,
        )
    }

    /// Linear PnL for a position entered at `entry` and marked at `exit`
    /// (both quote-minor per `1.0` base), signed by side.
    ///
    /// `pnl = side * qty_base * (exit - entry)`
    #[must_use]
    pub fn linear_pnl_quote_minor(
        &self,
        entry_quote_minor: u128,
        exit_quote_minor: u128,
        signed_qty_lots: i64,
    ) -> Option<i128> {
        if signed_qty_lots == 0 {
            return Some(0);
        }
        let base_minor = u128::from(signed_qty_lots.unsigned_abs()).checked_mul(self.lot_size())?;
        let d = if exit_quote_minor >= entry_quote_minor {
            let abs = mul_div(
                exit_quote_minor - entry_quote_minor,
                base_minor,
                self.base_unit()?,
                Rounding::NearestHalfUp,
            )?;
            i128::try_from(abs).ok()?
        } else {
            let abs = mul_div(
                entry_quote_minor - exit_quote_minor,
                base_minor,
                self.base_unit()?,
                Rounding::NearestHalfUp,
            )?;
            -i128::try_from(abs).ok()?
        };
        Some(d * i128::from(signed_qty_lots.signum()))
    }

    /// Cash-settlement value of one lot at settlement price `s`
    /// (quote-minor per `1.0` base): `max(0, s - K)` or `max(0, K - s)`.
    #[must_use]
    pub fn option_intrinsic_quote_minor(
        &self,
        market: &OptionMarket,
        settlement_quote_minor: u128,
    ) -> Option<u128> {
        let raw = match market.kind {
            OptionKind::Call => settlement_quote_minor.saturating_sub(market.strike_quote_minor),
            OptionKind::Put => market
                .strike_quote_minor
                .saturating_sub(settlement_quote_minor),
        };
        let base_minor = self.lot_size();
        mul_div(raw, base_minor, self.base_unit()?, Rounding::NearestHalfUp)
    }

    /// Validate quantity is a positive whole number of lots within limits.
    pub fn validate_qty(&self, qty_lots: u64) -> Result<(), CoreError> {
        if qty_lots == 0 {
            return Err(CoreError::InvalidOrder("quantity must be positive".into()));
        }
        if qty_lots > self.max_order_lots() {
            return Err(CoreError::InvalidOrder(format!(
                "quantity {qty_lots} lots exceeds max {}",
                self.max_order_lots()
            )));
        }
        Ok(())
    }
}

impl From<PerpMarket> for Instrument {
    fn from(m: PerpMarket) -> Self {
        Instrument::Perp(m)
    }
}

impl From<OptionMarket> for Instrument {
    fn from(m: OptionMarket) -> Self {
        Instrument::Option(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn btc_perp() -> PerpMarket {
        PerpMarket::default()
    }

    #[test]
    fn notional_math_btc() {
        let perp = btc_perp();
        // 0.5 BTC at $65,000.00 = $32,500.00 = 3_250_000 minor units.
        let ticks = perp.tick_size_quote_minor * 65_000 / 100; // price_ticks = 65,000 ($1 ticks)
        let notional = Instrument::Perp(perp)
            .notional_quote_minor(u64::try_from(ticks).unwrap(), 500)
            .unwrap();
        assert_eq!(notional, 3_250_000);
    }

    #[test]
    fn linear_pnl_signs() {
        let inst = Instrument::Perp(btc_perp());
        // Long 1000 lots (1.0 BTC) from 60k to 65k -> +5000.00
        let pnl = inst
            .linear_pnl_quote_minor(6_000_000, 6_500_000, 1000)
            .unwrap();
        assert_eq!(pnl, 500_000);
        // Short the same move -> -5000.00
        let pnl = inst
            .linear_pnl_quote_minor(6_000_000, 6_500_000, -1000)
            .unwrap();
        assert_eq!(pnl, -500_000);
        // Mark below entry for a long -> negative
        let pnl = inst
            .linear_pnl_quote_minor(6_000_000, 5_000_000, 1000)
            .unwrap();
        assert_eq!(pnl, -1_000_000);
    }

    #[test]
    fn option_intrinsic() {
        let opt = OptionMarket {
            strike_quote_minor: 8_000_000,
            lot_size_base_minor: 1_000, // 0.01 BTC
            ..OptionMarket::default()
        };
        let inst = Instrument::Option(opt.clone());
        // Call struck at 80k, settles 100k: ITM $20,000 * 0.01 BTC = $200 = 20_000 minor.
        let v = inst.option_intrinsic_quote_minor(&opt, 10_000_000).unwrap();
        assert_eq!(v, 20_000);
        // OTM call settles worthless.
        assert_eq!(
            inst.option_intrinsic_quote_minor(&opt, 7_500_000).unwrap(),
            0
        );
    }

    #[test]
    fn tick_roundtrip_requires_grid() {
        let inst = Instrument::Perp(btc_perp());
        assert_eq!(inst.ticks_from_quote_minor(6_500_000), Some(65_000));
        // Half-tick prices are off-grid.
        assert_eq!(inst.ticks_from_quote_minor(6_500_050), None);
    }
}
