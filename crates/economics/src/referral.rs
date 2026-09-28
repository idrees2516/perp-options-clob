//! Referral attribution (G-36): a share of taker fees flows to the
//! referrer, with anti-abuse rails.
//!
//! The dYdX/Hyperliquid referral shape, hardened:
//!
//! * Each subaccount may name **one referrer** (immutable once set —
//!   no re-parenting fee-harvesting).
//! * The referrer earns `share_bps` of every taker fee the referee
//!   pays, **paid from the venue's house share**, never on top of the
//!   client's fee (the referee's economics are untouched).
//! * **Self-referral is refused** (an account, and its referrer, cannot
//!   be the same user), and the registry is one-directional (A referred
//!   by B cannot also refer B).
//! * Accruals are tracked per referrer and paid by explicit settlement
//!   (the operator drains them on the reward cadence).

use std::collections::{BTreeMap, BTreeSet};

/// Referral program configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferralConfig {
    /// Share of the referee's taker fee credited to the referrer, bps.
    pub share_bps: u64,
    /// Maximum share (hard cap on config mistakes).
    pub max_share_bps: u64,
    /// Fees below this (quote minor) do not accrue (dust guard).
    pub min_fee_quote_minor: u128,
}

impl Default for ReferralConfig {
    fn default() -> Self {
        Self {
            share_bps: 2_500, // 25% of the taker fee
            max_share_bps: 5_000,
            min_fee_quote_minor: 0,
        }
    }
}

/// Why a referral action was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferralError {
    /// Accounts cannot refer themselves.
    SelfReferral,
    /// The account already has a referrer.
    AlreadyReferred,
    /// The link would create a cycle.
    WouldCycle,
    /// Share bps above the configured cap.
    ShareTooHigh,
}

/// The referral registry and accrual ledger.
#[derive(Debug, Default)]
pub struct ReferralProgram {
    config: ReferralConfig,
    /// referee → referrer.
    parents: BTreeMap<u64, u64>,
    /// Transitive ancestor closure (cycle prevention), per referee.
    ancestors: BTreeMap<u64, BTreeSet<u64>>,
    /// Accrued referral income per referrer, quote minor.
    accrued: BTreeMap<u64, u128>,
    /// Lifetime fees attributed (reporting).
    lifetime_attributed_quote_minor: u128,
}

impl ReferralProgram {
    /// New program with a config.
    #[must_use]
    pub fn new(config: ReferralConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// Link a referee to a referrer (immutable).
    pub fn link(&mut self, referee: u64, referrer: u64) -> Result<(), ReferralError> {
        if referee == referrer {
            return Err(ReferralError::SelfReferral);
        }
        if self.parents.contains_key(&referee) {
            return Err(ReferralError::AlreadyReferred);
        }
        // Cycle: the referrer must not already descend from the referee.
        if self
            .ancestors
            .get(&referrer)
            .is_some_and(|set| set.contains(&referee))
        {
            return Err(ReferralError::WouldCycle);
        }
        let mut anc = self.ancestors.get(&referrer).cloned().unwrap_or_default();
        anc.insert(referrer);
        self.ancestors.insert(referee, anc);
        self.parents.insert(referee, referrer);
        Ok(())
    }

    /// Attribute a taker fee: returns the referrer's share (and debits it
    /// from the caller's notion of the house share — the split itself
    /// is the caller's accounting; this ledger tracks accruals only).
    pub fn attribute_taker_fee(&mut self, referee: u64, fee_quote_minor: u128) -> Option<u128> {
        if fee_quote_minor < self.config.min_fee_quote_minor {
            return None;
        }
        let referrer = *self.parents.get(&referee)?;
        let share = poc_core::mul_div(
            fee_quote_minor,
            u128::from(self.config.share_bps),
            10_000,
            poc_core::Rounding::Floor,
        )?;
        if share == 0 {
            return None;
        }
        let e = self.accrued.entry(referrer).or_insert(0);
        *e = e.saturating_add(share);
        self.lifetime_attributed_quote_minor =
            self.lifetime_attributed_quote_minor.saturating_add(share);
        Some(share)
    }

    /// Drain accrued income for settlement (returns and zeroes).
    pub fn settle(&mut self, referrer: u64) -> u128 {
        self.accrued.remove(&referrer).unwrap_or(0)
    }

    /// Accrued-but-unpaid income of one referrer.
    #[must_use]
    pub fn accrued_for(&self, referrer: u64) -> u128 {
        self.accrued.get(&referrer).copied().unwrap_or(0)
    }

    /// The referrer of a referee, if any.
    #[must_use]
    pub fn referrer_of(&self, referee: u64) -> Option<u64> {
        self.parents.get(&referee).copied()
    }

    /// Lifetime attributed share (reporting).
    #[must_use]
    pub fn lifetime_attributed(&self) -> u128 {
        self.lifetime_attributed_quote_minor
    }

    /// Validate the config (constructor-grade sanity).
    #[must_use]
    pub fn config(&self) -> ReferralConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_and_accrue() {
        let mut p = ReferralProgram::new(ReferralConfig {
            share_bps: 2_500,
            max_share_bps: 5_000,
            min_fee_quote_minor: 0,
        });
        p.link(10, 1).unwrap();
        assert_eq!(p.referrer_of(10), Some(1));
        // 25% of a 40-minor taker fee.
        assert_eq!(p.attribute_taker_fee(10, 40), Some(10));
        assert_eq!(p.attribute_taker_fee(10, 41), Some(10)); // floor
        assert_eq!(p.accrued_for(1), 20);
        // Unattributed accounts accrue nothing.
        assert_eq!(p.attribute_taker_fee(99, 40), None);
        // Settle drains.
        assert_eq!(p.settle(1), 20);
        assert_eq!(p.accrued_for(1), 0);
        assert_eq!(p.lifetime_attributed(), 20);
    }

    #[test]
    fn self_referral_and_immutable_links() {
        let mut p = ReferralProgram::new(ReferralConfig::default());
        assert_eq!(p.link(5, 5).unwrap_err(), ReferralError::SelfReferral);
        p.link(5, 1).unwrap();
        assert_eq!(p.link(5, 2).unwrap_err(), ReferralError::AlreadyReferred);
    }

    #[test]
    fn cycles_are_refused() {
        let mut p = ReferralProgram::new(ReferralConfig::default());
        p.link(2, 1).unwrap(); // 2 referred by 1
        p.link(3, 2).unwrap(); // 3 referred by 2 (ancestors: 2, 1)
                               // 1 referred by 3 would create 1 -> 3 -> 2 -> 1.
        assert_eq!(p.link(1, 3).unwrap_err(), ReferralError::WouldCycle);
        // Deeper: 4 by 3, then 1 by 4.
        p.link(4, 3).unwrap();
        assert_eq!(p.link(1, 4).unwrap_err(), ReferralError::WouldCycle);
        // Non-cyclic links still work.
        p.link(5, 4).unwrap();
    }

    #[test]
    fn dust_guard_and_zero_share() {
        let mut p = ReferralProgram::new(ReferralConfig {
            share_bps: 2_500,
            max_share_bps: 5_000,
            min_fee_quote_minor: 5,
        });
        p.link(7, 1).unwrap();
        assert_eq!(p.attribute_taker_fee(7, 3), None, "below dust guard");
        // 25% of 5 = 1.25 -> floor 1.
        assert_eq!(p.attribute_taker_fee(7, 5), Some(1));
    }
}
