//! 0DTE option symbol space: listing keys, packed codec, and strike ladder.
//!
//! An option listing is identified by `(underlying, expiry_minute, kind,
//! strike)`. It packs losslessly into a single `u64` key:
//!
//! ```text
//! [63..48] underlying : u16
//! [47..21] expiry     : epoch minute, u27 (~year 2225)
//! [20]    kind        : 0 = call, 1 = put
//! [19..0] strike      : ticks (max 1_048_575 = 10,485.75 pts)
//! ```
//!
//! The codec is branch-light integer arithmetic — measurable in a couple of
//! nanoseconds — and is used for registry lookups on the cold path only; the
//! hot path works on `SymbolId` slab indices.

use crate::types::{Price, UnderlyingId};

/// Option type.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum OptKind {
    /// Call option (right to buy).
    Call = 0,
    /// Put option (right to sell).
    Put = 1,
}

impl OptKind {
    /// Parse from bit value.
    #[inline(always)]
    pub fn from_bit(bit: u64) -> OptKind {
        if bit == 0 {
            OptKind::Call
        } else {
            OptKind::Put
        }
    }
}

/// A fully specified option listing.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Listing {
    /// Underlying index.
    pub underlying: UnderlyingId,
    /// Expiry as minutes since the Unix epoch (0DTE = same trading day).
    pub expiry_min: u32,
    /// Call or put.
    pub kind: OptKind,
    /// Strike in ticks.
    pub strike: Price,
}

/// Max representable strike in ticks (2^20 - 1).
pub const MAX_STRIKE_TICKS: u64 = (1u64 << 20) - 1;
/// Max representable expiry minute (2^27 - 1, ~year 2225).
pub const MAX_EXPIRY_MIN: u32 = (1u32 << 27) - 1;

impl Listing {
    /// Pack into a u64 registry key. Panics (deliberately, at registration
    /// time only) if strike or expiry exceed the representable range.
    #[inline]
    pub fn key(self) -> u64 {
        assert!(self.strike <= MAX_STRIKE_TICKS, "strike out of range");
        assert!(self.expiry_min <= MAX_EXPIRY_MIN, "expiry out of range");
        ((self.underlying as u64) << 48)
            | ((self.expiry_min as u64) << 21)
            | ((self.kind as u64) << 20)
            | self.strike
    }

    /// Unpack from a u64 registry key.
    #[inline]
    pub fn unpack(key: u64) -> Listing {
        Listing {
            underlying: (key >> 48) as UnderlyingId,
            expiry_min: ((key >> 21) & 0x7FF_FFFF) as u32,
            kind: OptKind::from_bit((key >> 20) & 1),
            strike: key & MAX_STRIKE_TICKS,
        }
    }

    /// Intrinsic value in ticks given an underlying settlement price.
    #[inline(always)]
    pub fn intrinsic(self, settle: Price) -> Price {
        match self.kind {
            OptKind::Call => settle.saturating_sub(self.strike),
            OptKind::Put => self.strike.saturating_sub(settle),
        }
    }

    /// Approximate delta in basis points of underlying (heuristic used for
    /// pre-trade delta-notional limits, not for pricing).
    ///
    /// Linear around the strike: 5,000 bps at the money, +500 bps per 1%
    /// of moneyness, clamped to [2,500, 10,000] in magnitude. Deterministic,
    /// integer-only, oracle-free.
    #[inline]
    pub fn delta_bps(self, ref_price: Price) -> i64 {
        if ref_price == 0 {
            return 0;
        }
        let k = self.strike;
        let s = ref_price;
        // Signed moneyness in ticks; 1% of strike == 500 bps of delta.
        let per_pct = (k / 100).max(1) as i64;
        let m = match self.kind {
            OptKind::Call => s as i64 - k as i64,
            OptKind::Put => k as i64 - s as i64,
        };
        let d = (5_000 + (m / per_pct) * 500).clamp(2_500, 10_000);
        match self.kind {
            OptKind::Call => d,
            OptKind::Put => -d,
        }
    }
}

/// Round a strike/// Round a strike (in ticks) to a standard chain increment (in ticks).
#[inline]
pub fn round_strike(strike: Price, increment: Price) -> Price {
    if increment <= 1 {
        return strike;
    }
    (strike / increment) * increment
}

/// Standard 0DTE session expiry ladder in minutes-past-midnight (ET):
/// hourly expiries 10:00 ..= 16:00, the last one being the MOC close.
pub const HOUR_EXPIRIES: [u32; 7] = [600, 660, 720, 780, 840, 900, 960];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_roundtrip() {
        let l = Listing {
            underlying: 7,
            expiry_min: 30_900_000,
            kind: OptKind::Put,
            strike: 523_456,
        };
        assert_eq!(Listing::unpack(l.key()), l);
        let c = Listing {
            underlying: 0,
            expiry_min: 0,
            kind: OptKind::Call,
            strike: 0,
        };
        assert_eq!(Listing::unpack(c.key()), c);
    }

    #[test]
    fn intrinsic() {
        let call = Listing {
            underlying: 0,
            expiry_min: 960,
            kind: OptKind::Call,
            strike: 500_000,
        };
        let put = Listing {
            underlying: 0,
            expiry_min: 960,
            kind: OptKind::Put,
            strike: 500_000,
        };
        assert_eq!(call.intrinsic(500_100), 100);
        assert_eq!(call.intrinsic(499_900), 0);
        assert_eq!(put.intrinsic(499_900), 100);
        assert_eq!(put.intrinsic(500_100), 0);
    }

    #[test]
    fn delta_bounds() {
        let l = Listing {
            underlying: 0,
            expiry_min: 960,
            kind: OptKind::Call,
            strike: 500_000,
        };
        assert_eq!(l.delta_bps(2_000_000), 10_000);
        assert_eq!(l.delta_bps(100_000), 2_500);
        let d = l.delta_bps(500_000);
        assert!((4_000..=6_000).contains(&d), "atm delta {d}");
        let p = Listing {
            underlying: 0,
            expiry_min: 960,
            kind: OptKind::Put,
            strike: 500_000,
        };
        assert!(p.delta_bps(500_000) < 0);
        assert_eq!(p.delta_bps(2_000_000), -2_500);
    }
}
