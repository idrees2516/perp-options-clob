//! Volume-tiered maker/taker fee schedule.
//!
//! ## Why tiers + rebates
//!
//! Production CLOBs (dYdX v4, Hyperliquid, Derive V3) all converge on the
//! same ladder: taker fees descend with 30-day volume, maker fees descend
//! faster and can go *negative* (a rebate). The economic logic:
//!
//! * Takers consume depth; they pay for immediacy.
//! * Makers *provide* the depth that makes takers willing to arrive at all;
//!   at scale they are paid to do it.
//! * Rebates are capped so the house never loses money on a trade *net of
//!   funding and spread capture* — the tier-4 rebate is smaller than the
//!   lowest taker fee it is paired against in practice.

use poc_core::{apply_bps, mul_div_floor, to_i128, Rounding};

/// One rung of the fee ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeTier {
    /// Human-readable tier name (e.g. `"VIP-3"`).
    pub name: &'static str,
    /// Maker fee in basis points of notional. Negative = rebate (credit).
    pub maker_bps: i64,
    /// Taker fee in basis points of notional. Always `>= 0`.
    pub taker_bps: i64,
    /// 30-day traded volume (quote minor units) required to qualify,
    /// inclusive. Tiers are ordered by ascending threshold.
    pub volume_threshold_quote_minor: u128,
}

/// A fee computation result. Signed: positive = owed by the user, negative =
/// rebate credited to the user.
pub type FeeQuote = i128;

/// The ladder of fee tiers for one venue configuration.
///
/// `resolve` picks the *highest* tier whose threshold the trailing 30-day
/// volume meets — dYdX v4 semantics (best tier wins, no exclusivity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeSchedule {
    /// Tiers sorted ascending by volume threshold. Index 0 is the base tier.
    tiers: Vec<FeeTier>,
    /// 30-day volume per subaccount, quote minor units.
    volumes: std::collections::HashMap<u64, u128>,
}

impl Default for FeeSchedule {
    fn default() -> Self {
        Self::mainnet()
    }
}

impl FeeSchedule {
    /// A Hyperliquid-shaped default ladder (cents precision):
    ///
    /// | Tier | 30d volume | Maker | Taker |
    /// |------|-----------|-------|-------|
    /// | 0    | $0        | 1.0 bp | 4.5 bps |
    /// | 1    | $10k      | 0.8 bp | 3.5 bps |
    /// | 2    | $100k     | 0.5 bp | 2.5 bps |
    /// | 3    | $1M       | 0.2 bp | 1.5 bps |
    /// | 4    | $10M      | -0.5 bp (rebate) | 0.9 bp |
    #[must_use]
    pub fn mainnet() -> Self {
        Self {
            tiers: vec![
                FeeTier {
                    name: "Base",
                    maker_bps: 1,
                    taker_bps: 4,
                    volume_threshold_quote_minor: 0,
                },
                FeeTier {
                    name: "VIP-1",
                    maker_bps: 1,
                    taker_bps: 3,
                    volume_threshold_quote_minor: 1_000_000, // $10k in cents
                },
                FeeTier {
                    name: "VIP-2",
                    maker_bps: 0,
                    taker_bps: 2,
                    volume_threshold_quote_minor: 10_000_000,
                },
                FeeTier {
                    name: "VIP-3",
                    maker_bps: 0,
                    taker_bps: 1,
                    volume_threshold_quote_minor: 100_000_000,
                },
                FeeTier {
                    name: "VIP-4",
                    maker_bps: -1, // maker rebate
                    taker_bps: 1,
                    volume_threshold_quote_minor: 1_000_000_000,
                },
            ],
            volumes: std::collections::HashMap::new(),
        }
    }

    /// Replace the tier ladder. Tiers are re-sorted ascending by threshold.
    ///
    /// Returns `None` if the ladder is empty.
    pub fn with_tiers(mut self, mut tiers: Vec<FeeTier>) -> Option<Self> {
        if tiers.is_empty() {
            return None;
        }
        tiers.sort_by_key(|t| t.volume_threshold_quote_minor);
        self.tiers = tiers;
        Some(self)
    }

    /// The tier a subaccount currently sits in.
    #[must_use]
    pub fn tier_for(&self, subaccount: u64) -> &FeeTier {
        let volume = self.volumes.get(&subaccount).copied().unwrap_or(0);
        self.resolve(volume)
    }

    /// Resolve the tier for a raw 30-day volume figure.
    #[must_use]
    pub fn resolve(&self, volume_quote_minor: u128) -> &FeeTier {
        let mut best = &self.tiers[0];
        for tier in &self.tiers {
            if volume_quote_minor >= tier.volume_threshold_quote_minor {
                best = tier;
            }
        }
        best
    }

    /// Record traded notional into a subaccount's trailing volume figure.
    ///
    /// The engine calls this on every fill; a production system decays the
    /// figure as the 30-day window slides (see `decay_volume`), which the
    /// engine invokes once per funding interval.
    pub fn record_volume(&mut self, subaccount: u64, notional_quote_minor: u128) {
        let entry = self.volumes.entry(subaccount).or_insert(0);
        *entry = entry.saturating_add(notional_quote_minor);
    }

    /// Decay all volumes by `bps` (the 30-day sliding window moving one
    /// interval forward). Flooring means tiny volumes eventually reach zero
    /// through repeated decay rather than getting stuck above a threshold.
    pub fn decay_volume(&mut self, bps: u64) {
        for volume in self.volumes.values_mut() {
            if let Some(decayed) = apply_bps(*volume, -(bps as i64)) {
                *volume = decayed;
            }
        }
    }

    /// Number of tracked subaccounts (diagnostics).
    #[must_use]
    pub fn tracked_accounts(&self) -> usize {
        self.volumes.len()
    }
}

/// Computes signed fees on fills.
pub struct FeeCalculator;

impl FeeCalculator {
    /// Taker fee for a fill of `notional_quote_minor` in `tier`.
    ///
    /// Positive = owed by the taker. Fees round **up** (house is owed).
    #[must_use]
    pub fn taker_fee(tier: &FeeTier, notional_quote_minor: u128) -> Option<FeeQuote> {
        let fee = apply_bps(notional_quote_minor, tier.taker_bps)?;
        Some(to_i128(fee))
    }

    /// Maker fee for a fill of `notional_quote_minor` in `tier`.
    ///
    /// Positive = owed by the maker; negative = rebate credited. Fees round
    /// up, rebates round **down** (users are credited) — both behaviours are
    /// provided by [`apply_bps`]'s sign-aware rounding policy. Note
    /// [`apply_bps`] returns the *magnitude* for negative rates, so the
    /// credit sign is applied here.
    #[must_use]
    pub fn maker_fee(tier: &FeeTier, notional_quote_minor: u128) -> Option<FeeQuote> {
        let fee = apply_bps(notional_quote_minor, tier.maker_bps)?;
        Some(if tier.maker_bps >= 0 {
            to_i128(fee)
        } else {
            -to_i128(fee)
        })
    }

    /// Worst-case total fee for an order that is entirely taker — used by the
    /// risk engine's pre-trade margin check, so it rounds up deliberately.
    #[must_use]
    pub fn worst_case_fee(tier: &FeeTier, notional_quote_minor: u128) -> Option<FeeQuote> {
        Self::taker_fee(tier, notional_quote_minor)
    }

    /// Split a notional into fee + settlement-ready amounts for a maker fill:
    /// returns `(fee_delta, net_proceeds)` where `net = notional - fee` when
    /// the maker owes, and `notional + |rebate|` when credited.
    #[must_use]
    pub fn maker_settlement(
        tier: &FeeTier,
        notional_quote_minor: u128,
    ) -> Option<(FeeQuote, u128)> {
        let fee = Self::maker_fee(tier, notional_quote_minor)?;
        let net = if fee >= 0 {
            notional_quote_minor.saturating_sub(fee.unsigned_abs())
        } else {
            notional_quote_minor.saturating_add(fee.unsigned_abs())
        };
        Some((fee, net))
    }

    /// Fraction of a notional payable at `bps` (helper for reward pools and
    /// liquidation penalties shared with other crates).
    #[must_use]
    pub fn bps_of(notional_quote_minor: u128, bps: u64) -> Option<u128> {
        mul_div_floor(notional_quote_minor, u128::from(bps), 10_000)
    }

    /// Same as [`FeeCalculator::bps_of`] but rounding up.
    #[must_use]
    pub fn bps_of_ceil(notional_quote_minor: u128, bps: u64) -> Option<u128> {
        poc_core::mul_div(
            notional_quote_minor,
            u128::from(bps),
            10_000,
            Rounding::Ceil,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_resolution_uses_best_qualifying_tier() {
        let s = FeeSchedule::mainnet();
        assert_eq!(s.resolve(0).name, "Base");
        assert_eq!(s.resolve(1_000_000).name, "VIP-1"); // inclusive threshold
        assert_eq!(s.resolve(9_000_000).name, "VIP-1");
        assert_eq!(s.resolve(10_000_000).name, "VIP-2");
        assert_eq!(s.resolve(5_000_000_000).name, "VIP-4");
    }

    #[test]
    fn volume_tracking_moves_tiers() {
        let mut s = FeeSchedule::mainnet();
        assert_eq!(s.tier_for(7).name, "Base");
        s.record_volume(7, 10_000_000);
        assert_eq!(s.tier_for(7).name, "VIP-2");
        // Volume is cumulative within the window.
        s.record_volume(7, 100_000_000 - 10_000_000);
        assert_eq!(s.tier_for(7).name, "VIP-3");
        // Other accounts unaffected.
        assert_eq!(s.tier_for(8).name, "Base");
    }

    #[test]
    fn taker_fees_round_up() {
        let tier = FeeTier {
            name: "T",
            maker_bps: 0,
            taker_bps: 4,
            volume_threshold_quote_minor: 0,
        };
        // 3 minor units * 4bps = 0.0012 -> rounds up to 1.
        assert_eq!(FeeCalculator::taker_fee(&tier, 3), Some(1));
        assert_eq!(FeeCalculator::taker_fee(&tier, 1_000_000), Some(400));
    }

    #[test]
    fn maker_rebate_rounds_down_and_credits() {
        let tier = FeeTier {
            name: "T",
            maker_bps: -1, // 0.01% rebate
            taker_bps: 1,
            volume_threshold_quote_minor: 0,
        };
        // 3 * 1bp = 0.0003 -> rebate floors to 0.
        assert_eq!(FeeCalculator::maker_fee(&tier, 3), Some(0));
        // Rebates are signed negative (credited to the maker).
        assert_eq!(FeeCalculator::maker_fee(&tier, 10_000), Some(-1));
        let (fee, net) = FeeCalculator::maker_settlement(&tier, 10_000).unwrap();
        assert_eq!(fee, -1);
        assert_eq!(net, 10_001, "rebate increases net proceeds");
    }

    #[test]
    fn maker_settlement_owed_deducts() {
        let tier = FeeTier {
            name: "T",
            maker_bps: 1,
            taker_bps: 4,
            volume_threshold_quote_minor: 0,
        };
        let (fee, net) = FeeCalculator::maker_settlement(&tier, 10_000).unwrap();
        assert_eq!(fee, 1);
        assert_eq!(net, 9_999);
    }

    #[test]
    fn custom_ladder_requires_nonempty() {
        assert!(FeeSchedule::mainnet().with_tiers(Vec::new()).is_none());
        let custom = FeeSchedule::mainnet()
            .with_tiers(vec![
                FeeTier {
                    name: "Flat",
                    maker_bps: 0,
                    taker_bps: 2,
                    volume_threshold_quote_minor: 0,
                },
                FeeTier {
                    name: "Pro",
                    maker_bps: 0,
                    taker_bps: 1,
                    volume_threshold_quote_minor: 5_000_000,
                },
            ])
            .unwrap();
        assert_eq!(custom.resolve(4_999_999).name, "Flat");
        assert_eq!(custom.resolve(5_000_000).name, "Pro");
    }

    #[test]
    fn volume_decay_pulls_accounts_back_down() {
        let mut s = FeeSchedule::mainnet();
        s.record_volume(7, 10_000_000);
        assert_eq!(s.tier_for(7).name, "VIP-2");
        // Decay 50% per interval: 10M -> 5M -> 2.5M -> 1.25M -> ... below 1M.
        for _ in 0..4 {
            s.decay_volume(5_000);
        }
        assert_eq!(s.tier_for(7).name, "Base");
        assert_eq!(s.tracked_accounts(), 1);
    }

    #[test]
    fn bps_helpers() {
        assert_eq!(FeeCalculator::bps_of(10_000, 250), Some(250));
        assert_eq!(FeeCalculator::bps_of_ceil(3, 1), Some(1));
        assert_eq!(FeeCalculator::bps_of(3, 1), Some(0));
    }
}
