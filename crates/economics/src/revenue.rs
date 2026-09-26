//! Fee-revenue routing: house, insurance fund, and buyback pool.
//!
//! ## Economic rationale
//!
//! Where fee income goes is a *platform design decision*, not an accounting
//! accident. The split implemented here mirrors the stack used by exchanges
//! that survived full market cycles (Deribit's insurance accumulation, dYdX's
//! treasury + safety allocation, and the buyback pattern popularised by
//! fee-switch protocols):
//!
//! * **House** — operating revenue of the venue operator.
//! * **Insurance fund** — the only backstop standing between a liquidation
//!   shortfall and socialized losses. Every unit routed here directly buys
//!   *deleveraging headroom*, letting the venue charge lower maintenance
//!   margins (and therefore offer higher leverage) safely.
//! * **Buyback pool** — accumulate-and-burn of the platform token,
//!   redistributing growth to holders; the growth-flywheel leg.
//!
//! The allocator is **exact**: shares are floored per destination and the
//! remainder is paid to the house, so `Σ allocations == amount` always. A
//! routing rule that could lose or create quote units would break the
//! ledger-conservation invariant the whole engine is built on.

use poc_core::{mul_div_floor, Rounding};

/// Destination shares in basis points of routed revenue.
///
/// The three fields must sum to exactly `10_000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevenueSplit {
    /// Share routed to the house, bps.
    pub house_bps: u64,
    /// Share routed to the insurance fund, bps.
    pub insurance_bps: u64,
    /// Share routed to the buyback pool, bps.
    pub buyback_bps: u64,
}

impl Default for RevenueSplit {
    fn default() -> Self {
        // 60 / 30 / 10 — growth flywheel with a fat insurance cushion.
        Self {
            house_bps: 6_000,
            insurance_bps: 3_000,
            buyback_bps: 1_000,
        }
    }
}

impl RevenueSplit {
    /// Validate that shares sum to 10_000 bps exactly.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.house_bps
            .saturating_add(self.insurance_bps)
            .saturating_add(self.buyback_bps)
            == 10_000
    }
}

/// One revenue allocation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    /// To the house (includes the rounding remainder).
    pub house: u128,
    /// To the insurance fund.
    pub insurance: u128,
    /// To the buyback pool.
    pub buyback: u128,
}

impl Allocation {
    /// Total routed (must equal the input amount — conservation).
    #[must_use]
    pub fn total(&self) -> u128 {
        self.house
            .saturating_add(self.insurance)
            .saturating_add(self.buyback)
    }
}

/// Splits gross fee revenue into destination buckets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevenueRouter {
    split: RevenueSplit,
    /// Cumulative totals per destination (reporting / audit trail).
    cumulative: Allocation,
}

impl Default for RevenueRouter {
    fn default() -> Self {
        Self::new(RevenueSplit::default()).unwrap_or(Self::fallback())
    }
}

impl RevenueRouter {
    /// Build a router from a split. Returns `None` if the split is not
    /// exactly 10_000 bps — a misconfigured router must fail loudly at
    /// construction, not silently misroute money.
    pub fn new(split: RevenueSplit) -> Option<Self> {
        if split.is_consistent() {
            Some(Self {
                split,
                cumulative: Allocation {
                    house: 0,
                    insurance: 0,
                    buyback: 0,
                },
            })
        } else {
            None
        }
    }

    fn fallback() -> Self {
        // Reached only if the default split itself were invalid, which the
        // unit tests prove impossible; keeps Default total.
        Self {
            split: RevenueSplit::default(),
            cumulative: Allocation {
                house: 0,
                insurance: 0,
                buyback: 0,
            },
        }
    }

    /// The active split.
    #[must_use]
    pub fn split(&self) -> RevenueSplit {
        self.split
    }

    /// Lifetime routed amounts per destination.
    #[must_use]
    pub fn cumulative(&self) -> Allocation {
        self.cumulative
    }

    /// Route `amount` (quote minor) into destination buckets and record it.
    ///
    /// Insurance and buyback shares are floored; the house receives the
    /// remainder. Exact conservation is asserted in tests across a wide
    /// parameter sweep.
    pub fn route(&mut self, amount: u128) -> Option<Allocation> {
        let insurance = mul_div_floor(amount, u128::from(self.split.insurance_bps), 10_000)?;
        let buyback = mul_div_floor(amount, u128::from(self.split.buyback_bps), 10_000)?;
        let house = amount
            .checked_sub(insurance)
            .and_then(|r| r.checked_sub(buyback))?;
        let alloc = Allocation {
            house,
            insurance,
            buyback,
        };
        self.cumulative.house = self.cumulative.house.saturating_add(house);
        self.cumulative.insurance = self.cumulative.insurance.saturating_add(insurance);
        self.cumulative.buyback = self.cumulative.buyback.saturating_add(buyback);
        Some(alloc)
    }

    /// Pure (non-recording) preview of a route.
    #[must_use]
    pub fn preview(&self, amount: u128) -> Option<Allocation> {
        let insurance = mul_div_floor(amount, u128::from(self.split.insurance_bps), 10_000)?;
        let buyback = mul_div_floor(amount, u128::from(self.split.buyback_bps), 10_000)?;
        let house = amount
            .checked_sub(insurance)
            .and_then(|r| r.checked_sub(buyback))?;
        Some(Allocation {
            house,
            insurance,
            buyback,
        })
    }
}

/// Convenience: half-up bps share (kept for parity with other crates).
#[must_use]
pub fn share_half_up(amount: u128, bps: u64) -> Option<u128> {
    poc_core::mul_div(amount, u128::from(bps), 10_000, Rounding::NearestHalfUp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_split_sums_to_one() {
        assert!(RevenueSplit::default().is_consistent());
        assert!(!RevenueSplit {
            house_bps: 5_000,
            insurance_bps: 3_000,
            buyback_bps: 1_000
        }
        .is_consistent());
    }

    #[test]
    fn invalid_split_rejected_at_construction() {
        assert!(RevenueRouter::new(RevenueSplit {
            house_bps: 6_001,
            insurance_bps: 3_000,
            buyback_bps: 1_000
        })
        .is_none());
    }

    #[test]
    fn routing_conserves_exactly() {
        let mut router = RevenueRouter::default();
        for amount in [0_u128, 1, 2, 3, 7, 97, 10_000, 123_457, 1 << 100] {
            let a = router.route(amount).unwrap();
            assert_eq!(a.total(), amount, "conservation violated for {amount}");
        }
        assert_eq!(router.route(0).unwrap().total(), 0);
    }

    #[test]
    fn default_ratios_match_60_30_10() {
        let mut router = RevenueRouter::default();
        let a = router.route(10_000_000).unwrap();
        assert_eq!(a.insurance, 3_000_000);
        assert_eq!(a.buyback, 1_000_000);
        assert_eq!(a.house, 6_000_000, "house gets 60% + no remainder needed");
    }

    #[test]
    fn remainder_goes_to_house() {
        let mut router = RevenueRouter::default();
        // 33 routed: insurance floor(33*0.3)=9, buyback floor(33*0.1)=3,
        // house = 33-9-3 = 21 (vs exact 19.8).
        let a = router.route(33).unwrap();
        assert_eq!(a.insurance, 9);
        assert_eq!(a.buyback, 3);
        assert_eq!(a.house, 21);
        assert_eq!(a.total(), 33);
    }

    #[test]
    fn cumulative_tracks_lifetime() {
        let mut router = RevenueRouter::default();
        router.route(10_000).unwrap();
        router.route(10_000).unwrap();
        let c = router.cumulative();
        assert_eq!(c.house, 12_000);
        assert_eq!(c.insurance, 6_000);
        assert_eq!(c.buyback, 2_000);
    }

    #[test]
    fn preview_does_not_record() {
        let mut router = RevenueRouter::default();
        let p = router.preview(5_000).unwrap();
        assert_eq!(p.total(), 5_000);
        assert_eq!(router.cumulative().total(), 0, "preview must not record");
        assert_eq!(router.route(5_000).unwrap(), p);
    }

    #[test]
    fn conservation_sweep_across_splits() {
        for (h, i, b) in [
            (10_000u64, 0u64, 0u64),
            (0, 10_000, 0),
            (0, 0, 10_000),
            (3_333, 3_333, 3_334),
            (9_999, 1, 0),
        ] {
            let mut router = RevenueRouter::new(RevenueSplit {
                house_bps: h,
                insurance_bps: i,
                buyback_bps: b,
            })
            .unwrap();
            for amount in [1u128, 2, 3, 5, 7, 11, 13, 999, 65_537, 4_294_967_297] {
                let a = router.route(amount).unwrap();
                assert_eq!(a.total(), amount, "split {h}/{i}/{b} amount {amount}");
            }
        }
    }
}
