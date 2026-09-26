//! Funding mechanics: tethering derivative prices to their economic anchor.
//!
//! ## Perpetual futures — the BitMEX premium + interest model
//!
//! A perp is a synthetic that must not drift from spot. The tether is a
//! periodic cash flow: when the mark trades rich to the index, longs pay
//! shorts (and vice versa), which gives traders standing to push the mark
//! back:
//!
//! ```text
//! premium_bps = clamp( (TWAP(mark) - TWAP(index)) / TWAP(index),  ±premium_clamp )
//! rate_bps    = clamp( interest_bps + premium_bps,               ±rate_cap      )
//! ```
//!
//! * **positive rate → longs pay shorts**, the universal convention;
//! * the **interest component** is the cash-and-carry basis of the underlying
//!   (BitMEX's 0.01%/8h ≈ 1.1% p.a. on crypto);
//! * **clamps** bound both the observed premium (so one manipulated print
//!   cannot set the rate) and the final rate (so funding can never exceed
//!   the liquidation-relevant magnitude in one interval).
//!
//! ## Everlasting options — the Paradigm roll
//!
//! Dated options force holders to manage expiry: as maturity approaches,
//! gamma explodes and the position must be rolled. *Everlasting options*
//! (Paradigm, "Everlasting Options", White & SBF 2021) remove the expiry by
//! making the long pay the short the option's full mark value every funding
//! interval — economically, the long continuously re-buys a fresh option.
//! The equilibrium price of the everlasting claim equals the weighted
//! average of the corresponding expiring options across maturities, so the
//! market itself decides the effective maturity. We take the TWAP of the
//! mark premium over the interval (rather than a terminal snapshot) for the
//! same manipulation-resistance reason the perp index uses TWAPs.

use poc_core::{mul_div, mul_div_ceil, to_i128, FundingParams, Rounding};

/// The computed funding rate for one interval.
///
/// Sign convention: `rate_bps > 0` means **longs pay shorts**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FundingQuote {
    /// Final funding rate applied to notional, basis points, signed.
    pub rate_bps: i64,
    /// Clamped premium index component, basis points, signed.
    pub premium_bps: i64,
    /// Interest-rate component, basis points, signed.
    pub interest_bps: i64,
}

impl FundingQuote {
    /// A quote of exactly zero funding (balanced market).
    pub const ZERO: FundingQuote = FundingQuote {
        rate_bps: 0,
        premium_bps: 0,
        interest_bps: 0,
    };
}

/// Perpetual-futures funding rate calculation.
pub struct FundingCalculator;

impl FundingCalculator {
    /// Compute the interval funding rate from the TWAP of the mark and the
    /// TWAP of the index (both quote-minor per `1.0` base).
    ///
    /// Returns `None` on degenerate inputs (zero index) or arithmetic
    /// overflow (impossible for production parameters).
    #[must_use]
    pub fn perp_funding(
        mark_twap_quote_minor: u128,
        index_twap_quote_minor: u128,
        params: &FundingParams,
    ) -> Option<FundingQuote> {
        if index_twap_quote_minor == 0 {
            return None;
        }

        // Signed premium in bps: (mark - index) * 10_000 / index.
        let diff = to_i128(mark_twap_quote_minor) - to_i128(index_twap_quote_minor);
        let premium_bps_raw = diff * 10_000 / to_i128(index_twap_quote_minor);

        // Clamp the premium index.
        let clamp = i128::from(params.premium_clamp_bps);
        let premium_bps = premium_bps_raw.clamp(-clamp, clamp);

        // Add the interest component and clamp the final rate.
        let rate = premium_bps + i128::from(params.interest_rate_bps_per_interval);
        let rate_cap = i128::from(params.rate_cap_bps);
        let rate = rate.clamp(-rate_cap, rate_cap);

        Some(FundingQuote {
            rate_bps: i64::try_from(rate).ok()?,
            premium_bps: i64::try_from(premium_bps).ok()?,
            interest_bps: params.interest_rate_bps_per_interval,
        })
    }

    /// Signed funding payment **per lot** for a market quoted in
    /// `lot_size` base-minor units with `base_decimals` precision.
    ///
    /// Positive means one *long* lot pays that amount to the shorts. The
    /// amount is a single integer per lot applied uniformly to both sides,
    /// so aggregate funding over a closed position set conserves exactly.
    ///
    /// Rounding: the magnitude rounds **up** — funding is a transfer owed by
    /// one side, and rounding against the payer follows the house rule.
    #[must_use]
    pub fn payment_per_lot(
        rate_bps: i64,
        mark_quote_minor_per_base: u128,
        lot_size_base_minor: u128,
        base_decimals: u32,
    ) -> Option<i128> {
        if rate_bps == 0 {
            return Some(0);
        }
        let base_unit = 10_u128.checked_pow(base_decimals)?;
        // notional of one lot at mark:
        //   mark * lot_size / base_unit   (quote minor)
        let notional = mul_div(
            mark_quote_minor_per_base,
            lot_size_base_minor,
            base_unit,
            Rounding::Ceil,
        )?;
        let magnitude = mul_div_ceil(notional, u128::from(rate_bps.unsigned_abs()), 10_000)?;
        let signed = to_i128(magnitude) * i128::from(rate_bps.signum());
        Some(signed)
    }
}

/// Everlasting-option roll payments (Paradigm's "Everlasting Options").
///
/// The long pays the short the mark value of the option each interval. The
/// claim never expires; the funding *is* the roll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EverlastingRoll;

impl EverlastingRoll {
    /// Roll payment per lot owed by a long: the TWAP of the option's mark
    /// premium (quote minor per lot) over the funding interval.
    ///
    /// The result is already expressed per lot, so no further scaling is
    /// needed by the caller — options quote in premium per lot directly.
    #[must_use]
    pub fn payment_per_lot(premium_twap_quote_minor_per_lot: u128) -> u128 {
        premium_twap_quote_minor_per_lot
    }

    /// Effective "target maturity" of an everlasting option as implied by
    /// the market's chosen funding interval.
    ///
    /// From the paper: the everlasting claim's price equals the weighted
    /// average of expiring options across maturities `t, 2t, 3t, ...`
    /// with geometrically decaying weights, concentrating mass near the
    /// funding interval itself. This helper reports the interval in days
    /// for risk-reporting purposes (analytics only).
    #[must_use]
    pub fn concentration_days(funding_interval_ms: u64) -> f64 {
        funding_interval_ms as f64 / 86_400_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> FundingParams {
        FundingParams::default() // 8h, +1bp interest, ±5bp premium clamp, ±75bp cap
    }

    #[test]
    fn balanced_market_pays_only_interest() {
        let q = FundingCalculator::perp_funding(100_000, 100_000, &params()).unwrap();
        assert_eq!(q.premium_bps, 0);
        assert_eq!(q.rate_bps, 1, "longs pay the 1bp interest component");
    }

    #[test]
    fn rich_mark_makes_longs_pay() {
        // mark 0.3% rich = 30 bps premium, clamped to 5.
        let q = FundingCalculator::perp_funding(100_300, 100_000, &params()).unwrap();
        assert_eq!(q.premium_bps, 5);
        assert_eq!(q.rate_bps, 6, "clamped premium + 1bp interest");
        assert!(q.rate_bps > 0, "longs pay shorts");
    }

    #[test]
    fn cheap_mark_makes_shorts_pay() {
        // mark 0.2% cheap = -20 bps, clamped to -5.
        let q = FundingCalculator::perp_funding(99_800, 100_000, &params()).unwrap();
        assert_eq!(q.premium_bps, -5);
        assert_eq!(q.rate_bps, -4, "-5 + 1 = -4: shorts pay longs net");
    }

    #[test]
    fn rate_cap_binds() {
        let p = FundingParams {
            interest_rate_bps_per_interval: 90,
            rate_cap_bps: 75,
            ..params()
        };
        let q = FundingCalculator::perp_funding(100_500, 100_000, &p).unwrap();
        assert_eq!(q.premium_bps, 5);
        assert_eq!(q.rate_bps, 75, "rate capped despite 95bp raw");
    }

    #[test]
    fn zero_index_is_rejected() {
        assert!(FundingCalculator::perp_funding(1, 0, &params()).is_none());
    }

    #[test]
    fn payment_per_lot_sign_and_magnitude() {
        // BTC-shaped: $80,000.00 mark, 0.001 BTC lots, 5dp base.
        // notional/lot = 80_000 minor * 100 / 10^5 = $80.00.
        // +2 bps on $80.00 = $0.016 -> ceil -> 2 minor (cents).
        let p = FundingCalculator::payment_per_lot(2, 8_000_000, 100, 5).unwrap();
        assert_eq!(p, 2, "ceil(8000 * 2 / 10000) = 2");
        let n = FundingCalculator::payment_per_lot(-2, 8_000_000, 100, 5).unwrap();
        assert_eq!(n, -2);
        assert_eq!(
            FundingCalculator::payment_per_lot(0, 8_000_000, 100, 5),
            Some(0)
        );
    }

    #[test]
    fn funding_conserves_over_closed_positions() {
        // One long 3 lots vs one short 3 lots must net to zero exactly.
        let per_lot = FundingCalculator::payment_per_lot(7, 8_000_000, 100, 5).unwrap();
        let long_credit = -(i128::from(3) * per_lot); // longs pay when positive
        let short_credit = -(i128::from(-3) * per_lot);
        assert_eq!(long_credit + short_credit, 0);
    }

    #[test]
    fn everlasting_roll_is_the_premium_twap() {
        assert_eq!(EverlastingRoll::payment_per_lot(1_234), 1_234);
        // 8h interval = 1/3 day.
        let days = EverlastingRoll::concentration_days(8 * 3_600_000);
        assert!((days - 0.333).abs() < 0.01, "got {days}");
    }
}
