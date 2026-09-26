//! Volume-tiered maker/taker fee schedule with the options premium cap.
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
//!
//! ## The options premium cap (Deribit / Derive rule)
//!
//! An uncapped notional rate is economically broken at the option wings: a
//! far-OTM put priced at 0.1% of notional would pay ~4.5 bps of notional in
//! taker fees — 4.5x the entire premium. Both Deribit and Derive V3 cap the
//! option fee at a fraction of premium:
//!
//! ```text
//! option_fee = min( bps_rate × underlying_notional , cap_pct × premium )
//! ```
//!
//! We adopt taker cap = 12.5% of premium (the shared industry number) and a
//! tighter maker cap = 2.5% so maker economics can never exceed the premium
//! fractions that keep two-sided wing quoting viable. Fees ceil; rebates
//! floor — the same rounding asymmetry as the plain ladder.

use poc_core::{apply_bps, mul_div_floor, to_i128, Rounding};
use std::collections::BTreeMap;

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

/// Options fee-cap policy (see module docs). Fraction of premium that is
/// the maximum fee, in bps of premium.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionFeeCaps {
    /// Taker fee never exceeds this fraction of the option premium
    /// (12.5% = 1250 bps — the Deribit/Derive number).
    pub taker_cap_bps_of_premium: u64,
    /// Maker fee never exceeds this fraction of the premium
    /// (2.5% = 250 bps — tighter, so wing quoting stays paid to quote).
    pub maker_cap_bps_of_premium: u64,
}

impl Default for OptionFeeCaps {
    fn default() -> Self {
        Self {
            taker_cap_bps_of_premium: 1_250,
            maker_cap_bps_of_premium: 250,
        }
    }
}

/// A fee computation result. Signed: positive = owed by the user, negative =
/// rebate credited to the user.
pub type FeeQuote = i128;

/// Day-bucketed trailing volume for one subaccount: the exact 30-day
/// sliding window (dYdX v4 pattern — a dated ledger, not a decay
/// approximation).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct VolumeWindow {
    /// epoch-day -> volume booked that day (quote minor).
    buckets: BTreeMap<u64, u128>,
}

impl VolumeWindow {
    fn add(&mut self, day: u64, amount: u128, horizon_days: u64) {
        let entry = self.buckets.entry(day).or_insert(0);
        *entry = entry.saturating_add(amount);
        while let Some(first) = self.buckets.keys().next().copied() {
            if first + horizon_days <= day {
                self.buckets.remove(&first);
            } else {
                break;
            }
        }
    }

    fn total(&self) -> u128 {
        self.buckets
            .values()
            .copied()
            .fold(0_u128, u128::saturating_add)
    }
}

/// The ladder of fee tiers for one venue configuration.
///
/// `resolve` picks the *highest* tier whose threshold the trailing 30-day
/// volume meets — dYdX v4 semantics (best tier wins, no exclusivity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeSchedule {
    /// Tiers sorted ascending by volume threshold. Index 0 is the base tier.
    tiers: Vec<FeeTier>,
    /// Trailing-window volume ledger per subaccount (day buckets).
    volumes: std::collections::HashMap<u64, VolumeWindow>,
    /// Sliding-window horizon in days.
    window_days: u64,
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
            window_days: 30,
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
        let volume = self.trailing_volume(subaccount);
        self.resolve(volume)
    }

    /// Trailing-window volume of one subaccount (quote minor).
    #[must_use]
    pub fn trailing_volume(&self, subaccount: u64) -> u128 {
        self.volumes.get(&subaccount).map_or(0, VolumeWindow::total)
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

    /// Record traded notional into a subaccount's trailing volume figure
    /// at a specific engine time — the exact-window bookkeeping the engine
    /// calls on every fill (bucketed by epoch day, pruned past the horizon).
    ///
    /// A production system calls this on every fill; volume ages out of the
    /// window exactly when its day bucket leaves the horizon — no decay
    /// approximation, no interval-boundary drift (dYdX v4 semantics).
    pub fn record_volume_at(&mut self, subaccount: u64, notional_quote_minor: u128, ts_ms: u64) {
        let day = ts_ms / 86_400_000;
        let horizon = self.window_days;
        self.volumes
            .entry(subaccount)
            .or_default()
            .add(day, notional_quote_minor, horizon);
    }

    /// Record traded notional with no timestamp context (legacy path:
    /// bucketed at day 0 — callers that care about exact windows use
    /// [`FeeSchedule::record_volume_at`]).
    pub fn record_volume(&mut self, subaccount: u64, notional_quote_minor: u128) {
        self.record_volume_at(subaccount, notional_quote_minor, 0);
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

    /// Option taker fee with the premium cap (F-2): the fee is the lesser
    /// of the tier's notional rate and `cap_bps_of_premium × premium`.
    ///
    /// `premium_quote_minor` is the total premium value of the fill
    /// (price × qty). Both branches round up, so the minimum rounds up too.
    #[must_use]
    pub fn option_taker_fee(
        tier: &FeeTier,
        caps: &OptionFeeCaps,
        notional_quote_minor: u128,
        premium_quote_minor: u128,
    ) -> Option<FeeQuote> {
        let rate_fee = apply_bps(notional_quote_minor, tier.taker_bps)?;
        // Fees ceil (house is owed) — the cap branch rounds up too.
        let cap_fee = poc_core::mul_div(
            premium_quote_minor,
            u128::from(caps.taker_cap_bps_of_premium),
            10_000,
            Rounding::Ceil,
        )?;
        Some(to_i128(rate_fee.min(cap_fee)))
    }

    /// Option maker fee with the premium cap: the lesser (most negative)
    /// of the tier's maker economics and the cap magnitude. Rebates floor.
    #[must_use]
    pub fn option_maker_fee(
        tier: &FeeTier,
        caps: &OptionFeeCaps,
        notional_quote_minor: u128,
        premium_quote_minor: u128,
    ) -> Option<FeeQuote> {
        let plain = Self::maker_fee(tier, notional_quote_minor)?;
        let cap_floor = mul_div_floor(
            premium_quote_minor,
            u128::from(caps.maker_cap_bps_of_premium),
            10_000,
        )?;
        // Owing fees clamp to at most cap; rebates clamp to at most cap too.
        let clamped = match plain {
            p if p >= 0 => p.min(to_i128(cap_floor)),
            p => p.max(-(to_i128(cap_floor))),
        };
        Some(clamped)
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
    fn volume_ages_out_of_the_exact_window() {
        let mut s = FeeSchedule::mainnet();
        let day = 86_400_000_u64;
        s.record_volume_at(7, 10_000_000, 5 * day);
        assert_eq!(s.tier_for(7).name, "VIP-2");
        // Still inside the 30-day window 25 days later.
        s.record_volume_at(7, 1, 30 * day);
        assert_eq!(s.tier_for(7).name, "VIP-2");
        // The burst has left the window 31 days after it booked.
        s.record_volume_at(7, 1, 36 * day);
        assert_eq!(s.tier_for(7).name, "Base");
        assert_eq!(s.trailing_volume(7), 2, "only the two 1-unit prints remain");
    }

    #[test]
    fn option_fee_cap_binds_at_the_wings() {
        let tier = FeeTier {
            name: "T",
            maker_bps: 1,
            taker_bps: 4,
            volume_threshold_quote_minor: 0,
        };
        let caps = OptionFeeCaps::default();
        // Deep-OTM wing: notional 100_000, premium 100 (0.1% of notional).
        // Rate fee: 4 bps of 100_000 = 40. Cap: 12.5% of 100 = 12.5 -> 13.
        let fee = FeeCalculator::option_taker_fee(&tier, &caps, 100_000, 100).unwrap();
        assert_eq!(fee, 13, "cap binds, not the 4x-premium notional rate");
        // ITM-ish: premium 5_000, rate fee 40 vs cap 625 -> rate binds.
        let fee = FeeCalculator::option_taker_fee(&tier, &caps, 100_000, 5_000).unwrap();
        assert_eq!(fee, 40, "notional rate binds when premium is rich");
        // Maker rebate capped at 2.5% of premium.
        let rebate_tier = FeeTier {
            name: "R",
            maker_bps: -1,
            taker_bps: 4,
            volume_threshold_quote_minor: 0,
        };
        let fee = FeeCalculator::option_maker_fee(&rebate_tier, &caps, 100_000, 100).unwrap();
        assert_eq!(fee, -2, "rebate floors to 2.5% of premium");
        let fee = FeeCalculator::option_maker_fee(&rebate_tier, &caps, 1_000_000, 100_000).unwrap();
        assert_eq!(fee, -100, "full -1bp rebate when premium is rich");
    }

    #[test]
    fn bps_helpers() {
        assert_eq!(FeeCalculator::bps_of(10_000, 250), Some(250));
        assert_eq!(FeeCalculator::bps_of_ceil(3, 1), Some(1));
        assert_eq!(FeeCalculator::bps_of(3, 1), Some(0));
    }
}
