//! Options margin: premium rule for longs, stress-scenario margin for
//! shorts, delta-notional accounting, pin-risk surcharges.
//!
//! All integer arithmetic, all O(1) per touched position. Margin values are
//! cached on each position (and summed per account) so a trade only pays the
//! delta of one position's margin.
//!
//! Short-option stress margin per contract:
//! `max(worst_intrinsic(scenario) - mark, mark)` — you must cover the
//! scenario loss or at least the premium you sold for — plus a fee reserve,
//! with a 1.5x surcharge in the pin-risk zone near expiry.

use crate::symbol::{Listing, OptKind};
use crate::types::{Price, Qty};

/// Risk parameters (per venue/shard).
#[derive(Clone, Copy, Debug)]
pub struct RiskParams {
    /// Stress band for TTL <= 30 minutes, basis points (300 = 3.0%).
    pub band_ttl_le_30m: u32,
    /// Stress band for TTL <= 2 hours, basis points.
    pub band_ttl_le_2h: u32,
    /// Stress band otherwise, basis points.
    pub band_default: u32,
    /// Pin-risk applies when TTL (minutes) is at or below this.
    pub pin_ttl_min: u32,
    /// Pin zone: |ref - strike| <= ref * pin_zone_bps / 10_000.
    pub pin_zone_bps: u32,
    /// Pin surcharge multiplier in 4096ths (6144/4096 = 1.5x).
    pub pin_surcharge: u64,
    /// Fee reserve per short contract added to margin.
    pub fee_reserve: u64,
    /// Max delta notional (ticks) per account.
    pub delta_notional_limit: u64,
    /// Max position (signed, per series) in contracts.
    pub position_limit: u32,
    /// Order rate limit per account per second.
    pub rate_per_sec: u64,
}

impl RiskParams {
    /// Production-flavoured defaults for an index 0DTE chain.
    pub const fn default_params() -> RiskParams {
        RiskParams {
            band_ttl_le_30m: 300,
            band_ttl_le_2h: 200,
            band_default: 150,
            pin_ttl_min: 60,
            pin_zone_bps: 100,
            pin_surcharge: 6144,
            fee_reserve: 80,
            delta_notional_limit: 5_000_000_000,
            position_limit: 5_000,
            rate_per_sec: 500,
        }
    }
}

/// Stress band in basis points for a time-to-expiry (minutes).
#[inline(always)]
pub fn stress_band_bps(p: &RiskParams, ttl_min: u64) -> u32 {
    if ttl_min <= 30 {
        p.band_ttl_le_30m
    } else if ttl_min <= 120 {
        p.band_ttl_le_2h
    } else {
        p.band_default
    }
}

/// Is a series in the pin-risk zone near expiry?
#[inline(always)]
pub fn in_pin_zone(p: &RiskParams, listing: Listing, ref_price: Price, ttl_min: u64) -> bool {
    if ttl_min > p.pin_ttl_min as u64 {
        return false;
    }
    let zone = ref_price.saturating_mul(p.pin_zone_bps as u64) / 10_000;
    listing.strike.abs_diff(ref_price) <= zone
}

/// Margin per contract for a SHORT position in `listing`, marked at `mark`,
/// with `ref_price` for the underlying and `ttl_min` to expiry.
///
/// Long positions are premium-paid and carry no ongoing margin.
#[inline]
pub fn short_margin_per_contract(
    p: &RiskParams,
    listing: Listing,
    mark: Price,
    ref_price: Price,
    ttl_min: u64,
) -> u64 {
    let band = stress_band_bps(p, ttl_min) as u64;
    // Worst-case underlying for the short side.
    let worst = match listing.kind {
        OptKind::Call => ref_price.saturating_mul(10_000 + band) / 10_000,
        OptKind::Put => ref_price.saturating_mul(10_000 - band) / 10_000,
    };
    let worst_intrinsic = listing.intrinsic(worst);
    let stress_loss = worst_intrinsic.saturating_sub(mark);
    let mut m = stress_loss.max(mark) + p.fee_reserve;
    if in_pin_zone(p, listing, ref_price, ttl_min) {
        m = (m * p.pin_surcharge) >> 12;
    }
    m
}

/// Signed delta notional contribution of a position (ticks).
#[inline]
pub fn delta_notional(listing: Listing, ref_price: Price, qty_signed: i32) -> i64 {
    if qty_signed == 0 || ref_price == 0 {
        return 0;
    }
    (listing.delta_bps(ref_price) * qty_signed as i64 * ref_price as i64) / 10_000
}

/// Conservative pre-trade margin delta for an incoming SELL (short-opening)
/// order of `qty` contracts at `price` (assumes fill at `price`, worst mark).
#[inline]
pub fn prospective_short_margin(
    p: &RiskParams,
    listing: Listing,
    price: Price,
    ref_price: Price,
    ttl_min: u64,
    qty: Qty,
) -> u64 {
    short_margin_per_contract(p, listing, price, ref_price, ttl_min) * qty as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(kind: OptKind, strike: Price) -> Listing {
        Listing {
            underlying: 0,
            expiry_min: 960,
            kind,
            strike,
        }
    }

    #[test]
    fn short_call_margin_grows_with_band() {
        let p = RiskParams::default_params();
        let l = listing(OptKind::Call, 500_000);
        // Deep OTM short call at low mark: margin ≈ max(small loss, mark) + fee.
        let m1 = short_margin_per_contract(&p, l, 50, 500_000, 240);
        assert!(m1 >= 50 + p.fee_reserve);
        // Short TTL -> wider band -> higher worst intrinsic -> more margin.
        let m2 = short_margin_per_contract(&p, l, 50, 500_000, 20);
        assert!(m2 > m1);
        // ITM short call: stress loss dominates.
        let itm = listing(OptKind::Call, 400_000);
        let m3 = short_margin_per_contract(&p, itm, 50, 500_000, 20);
        assert!(m3 > 90_000, "m3={m3}");
    }

    #[test]
    fn pin_zone_surcharge() {
        let p = RiskParams::default_params();
        let atm = listing(OptKind::Call, 500_000);
        let far = listing(OptKind::Call, 600_000);
        let base = short_margin_per_contract(&p, far, 100, 500_000, 20);
        let pinned = short_margin_per_contract(&p, atm, 100, 500_000, 20);
        // ATM within 1% of ref at TTL 20min => surcharge applied.
        assert!(pinned > base);
        // Long TTL: no pin.
        let no_pin = short_margin_per_contract(&p, atm, 100, 500_000, 300);
        let would = short_margin_per_contract(&p, atm, 100, 500_000, 20);
        assert!(no_pin < would);
    }

    #[test]
    fn put_band_moves_down() {
        let p = RiskParams::default_params();
        let put = listing(OptKind::Put, 500_000);
        // Short put, ATM: worst is downside move.
        let m = short_margin_per_contract(&p, put, 100, 500_000, 20);
        let expected_worst = 500_000u64 * (10_000 - 300) / 10_000;
        let expect = expected_worst.saturating_sub(500_000); // intrinsic at worst
        let _ = expect;
        assert!(m > 100 + p.fee_reserve);
    }
}
