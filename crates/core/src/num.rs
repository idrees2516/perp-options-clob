//! Exact integer money arithmetic.
//!
//! Every monetary computation in the engine funnels through [`mul_div`] so
//! that (a) overflow is impossible by construction, and (b) the rounding
//! direction is an explicit, auditable decision rather than an accident.
//!
//! Rounding policy (mirrors house practice at major CLOBs):
//!
//! * Amounts **owed to the house** (fees) round **up**.
//! * Amounts **credited to a user** (rebates, proceeds) round **down**.
//! * Mid-flight risk figures round half-up; they are never posted to a ledger
//!   without a final directional decision.

/// Rounding direction for integer division.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rounding {
    /// Truncate toward zero.
    Floor,
    /// Round away from zero (ceiling on non-negative operands).
    Ceil,
    /// Round to nearest, ties away from zero.
    NearestHalfUp,
}

/// Checked `(a * b) / d` for `u128` operands.
///
/// Returns `None` on multiplication overflow, division by zero, or a quotient
/// that does not fit in `u128` (impossible for practical engine parameters,
/// but handled defensively).
#[must_use]
pub fn mul_div(a: u128, b: u128, d: u128, rounding: Rounding) -> Option<u128> {
    let prod = a.checked_mul(b)?;
    div_round(prod, d, rounding)
}

/// `(a * b) / d` truncated toward zero.
#[must_use]
pub fn mul_div_floor(a: u128, b: u128, d: u128) -> Option<u128> {
    mul_div(a, b, d, Rounding::Floor)
}

/// `(a * b) / d` rounded away from zero.
#[must_use]
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    mul_div(a, b, d, Rounding::Ceil)
}

fn div_round(n: u128, d: u128, rounding: Rounding) -> Option<u128> {
    if d == 0 {
        return None;
    }
    let q = n / d;
    let r = n % d;
    let round_up = match rounding {
        Rounding::Floor => false,
        Rounding::Ceil => r > 0,
        Rounding::NearestHalfUp => r * 2 >= d,
    };
    if round_up {
        q.checked_add(1)
    } else {
        Some(q)
    }
}

/// Apply a signed basis-point rate to an amount.
///
/// `bps > 0` scales the amount up; `bps < 0` scales it down (a rebate-style
/// negative fee). Results round **away from zero** for positive rates (the
/// house is owed) and toward zero for negative rates (the user is credited).
///
/// ```text
/// apply_bps(1_000_000, 5)   = 500      // 0.05% fee
/// apply_bps(1_000_000, -15) = 1500     // 0.15% rebate
/// ```
#[must_use]
pub fn apply_bps(amount: u128, bps: i64) -> Option<u128> {
    if bps >= 0 {
        mul_div(
            amount,
            u128::from(bps.unsigned_abs()),
            10_000,
            Rounding::Ceil,
        )
    } else {
        mul_div(amount, u128::from((-bps) as u64), 10_000, Rounding::Floor)
    }
}

/// Saturating `u128 → i128` conversion for money and prices.
///
/// Values above `i128::MAX` clamp instead of panicking — production
/// parameters never approach the boundary, but degraded inputs must stay
/// deterministic rather than abort the engine.
#[must_use]
pub fn to_i128(x: u128) -> i128 {
    i128::try_from(x).unwrap_or(i128::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_rounding_modes() {
        // 7 / 2 = 3.5
        assert_eq!(mul_div(7, 1, 2, Rounding::Floor), Some(3));
        assert_eq!(mul_div(7, 1, 2, Rounding::Ceil), Some(4));
        assert_eq!(mul_div(7, 1, 2, Rounding::NearestHalfUp), Some(4));
        // 6 / 2 = 3.0 -> no rounding anywhere
        for r in [Rounding::Floor, Rounding::Ceil, Rounding::NearestHalfUp] {
            assert_eq!(mul_div(6, 1, 2, r), Some(3));
        }
    }

    #[test]
    fn mul_div_detects_overflow() {
        let max = u128::MAX;
        assert_eq!(mul_div(max, 2, 1, Rounding::Floor), None);
        // (2^128-1) * 1 / (2^128-1) == 1
        assert_eq!(mul_div(max, 1, max, Rounding::Floor), Some(1));
    }

    #[test]
    fn mul_div_rejects_zero_denominator() {
        assert_eq!(mul_div(1, 1, 0, Rounding::Floor), None);
    }

    #[test]
    fn apply_bps_signed_rates() {
        assert_eq!(apply_bps(1_000_000, 5), Some(500));
        assert_eq!(apply_bps(1_000_000, -15), Some(1500));
        assert_eq!(apply_bps(1_000_000, 0), Some(0));
        // tiny amounts round up on fees, down on rebates
        assert_eq!(apply_bps(3, 1), Some(1));
        assert_eq!(apply_bps(3, -1), Some(0));
    }

    #[test]
    fn to_i128_saturates() {
        assert_eq!(to_i128(0), 0);
        assert_eq!(to_i128(42), 42);
        assert_eq!(to_i128(u128::MAX), i128::MAX);
    }
}
