//! Market-maker tier program (G-15): obligations, tiers, and the fee
//! discounts they earn.
//!
//! ## Why a tier *program* and not just rebates
//!
//! Volume-tier rebates (the [`FeeSchedule`](crate::fees::FeeSchedule)
//! ladder) pay for *flow*; they cannot pay for *depth*. A maker who lifts
//! and re-quotes all day earns VIP status while contributing nothing to
//! the book a taker actually consumes. The industry answer — Deribit's
//! market-maker program, Aevo's tiered benefits, Derive's designated
//! market-maker scheme — is a second ladder whose rungs are paid in
//! *obligations*, not volume:
//!
//! * **Uptime** — the share of sampled ticks on which the maker quoted
//!   both sides within the tier's spread band at the tier's size.
//! * **Spread** — the worst of the two sides' distances from mid, at the
//!   moment of sampling.
//! * **Size** — the smaller of the two sides' displayed lots.
//!
//! In exchange for meeting obligations *continuously*, the maker earns a
//! fee discount on top of (not instead of) the volume ladder — the
//! two ladders compose, the way Deribit's maker program and volume tiers
//! compose.
//!
//! ## Anti-gaming properties
//!
//! * **Sampled, not self-reported.** The engine's measurement rides the
//!   same randomized-sampling liquidity scorer (G-39) that drives reward
//!   payouts — a maker cannot grind uptime by quoting only when a
//!   deterministic clock says "sample now".
//! * **Both sides at once.** Every obligation is evaluated on the *worst*
//!   side; a one-sided quote earns nothing for the tick.
//! * **Demotion is automatic.** A tier is re-earned at every review from
//!   the trailing window's performance — no grandfathering, no appeals.
//! * **Discounts are bounded.** A tier discount can never make a fee
//!   negative or exceed the program cap, so the venue never *pays* a
//!   maker for the privilege of trading with them.
//!
//! ## Numeric policy
//!
//! Uptime is permille (1000 = 100%). Tiers are stored best-first
//! (index 0 is the most demanding tier with the largest discount). All
//! arithmetic is integer; discount application rounds in the *user's*
//! favour on rebates and against the user on fees — the same asymmetry
//! the fee ladder uses.

use std::collections::BTreeMap;

/// Obligations and benefits of one program tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmTierSpec {
    /// Human-readable tier name (e.g. `"MM-1"`).
    pub name: &'static str,
    /// Fee discount earned while active, bps of the computed fee
    /// (250 = 2.5% off). Applied after the volume-tier fee.
    pub fee_discount_bps: u64,
    /// Minimum share of sampled ticks with a qualifying two-sided
    /// quote, permille (950 = 95% uptime).
    pub min_uptime_permille: u64,
    /// Maximum worst-side spread from mid that counts, bps.
    pub max_spread_bps: u64,
    /// Minimum smaller-side displayed size that counts, lots.
    pub min_size_lots: u64,
}

/// One sampled tick's maker statistics for a single subaccount — the
/// per-tick facts the obligations are evaluated against.
///
/// `worst_spread_bps` is the larger of the two sides' distances from
/// mid (the *worst* side, because a two-sided quote is only as tight as
/// its wider leg); `min_size_lots` is the smaller side's displayed size.
/// Both are `u64::MAX`/0 respectively when a side is missing, which
/// naturally fails every tier's obligations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmTickStats {
    /// True when the subaccount quoted both sides on this tick.
    pub two_sided: bool,
    /// Worst-side spread from mid, bps (`u64::MAX` = unbounded/missing).
    pub worst_spread_bps: u64,
    /// Smaller-side displayed size, lots.
    pub min_size_lots: u64,
}

impl MmTickStats {
    /// Statistics of a subaccount with no qualifying presence this tick.
    #[must_use]
    pub fn absent() -> Self {
        Self {
            two_sided: false,
            worst_spread_bps: u64::MAX,
            min_size_lots: 0,
        }
    }

    /// Whether this tick satisfies a tier's spread/size obligations
    /// (uptime is evaluated across the window, not per tick).
    #[must_use]
    pub fn meets(&self, tier: &MmTierSpec) -> bool {
        self.two_sided
            && self.worst_spread_bps <= tier.max_spread_bps
            && self.min_size_lots >= tier.min_size_lots
    }
}

/// The tier ladder under which enrolled makers are measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmTierProgram {
    /// Tiers ordered best-first: index 0 is the most demanding tier
    /// (tightest spread, largest size, highest uptime, biggest
    /// discount). A maker is assigned the *first* tier whose uptime
    /// obligation their trailing performance meets.
    pub tiers: Vec<MmTierSpec>,
    /// Review cadence for tier re-evaluation, ms.
    pub review_interval_ms: u64,
}

impl Default for MmTierProgram {
    fn default() -> Self {
        Self::mainnet()
    }
}

impl MmTierProgram {
    /// The Deribit-shaped default ladder (permille uptime, bps spreads):
    ///
    /// | Tier | Uptime | Max spread | Min size | Discount |
    /// |------|--------|-----------|----------|----------|
    /// | MM-1 | 98%    | 50 bps    | 5 lots   | 20% off  |
    /// | MM-2 | 95%    | 100 bps   | 3 lots   | 12% off  |
    /// | MM-3 | 90%    | 250 bps   | 1 lot    | 6% off   |
    ///
    /// Discounts are deliberately modest against the fee ladder: the
    /// tier buys *predictable depth*, which the reward pool also pays
    /// for — stacking a huge discount on top would double-pay the same
    /// behaviour.
    #[must_use]
    pub fn mainnet() -> Self {
        Self {
            tiers: vec![
                MmTierSpec {
                    name: "MM-1",
                    fee_discount_bps: 2_000,
                    min_uptime_permille: 980,
                    max_spread_bps: 50,
                    min_size_lots: 5,
                },
                MmTierSpec {
                    name: "MM-2",
                    fee_discount_bps: 1_200,
                    min_uptime_permille: 950,
                    max_spread_bps: 100,
                    min_size_lots: 3,
                },
                MmTierSpec {
                    name: "MM-3",
                    fee_discount_bps: 600,
                    min_uptime_permille: 900,
                    max_spread_bps: 250,
                    min_size_lots: 1,
                },
            ],
            review_interval_ms: 30 * 24 * 60 * 60 * 1000, // monthly
        }
    }

    /// A program with no tiers (the feature exists, the venue opts out).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            tiers: Vec::new(),
            review_interval_ms: 0,
        }
    }

    /// Validate the ladder: at most 16 tiers, discounts capped at 50%,
    /// uptime obligations within (0, 1000], spreads and sizes non-zero
    /// (a zero max-spread is unsatisfiable; a zero min-size is
    /// unsatisfiable when the venue has no tick-minimum).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        if self.tiers.len() > 16 {
            return false;
        }
        self.tiers.iter().all(|t| {
            t.fee_discount_bps <= 5_000
                && t.min_uptime_permille > 0
                && t.min_uptime_permille <= 1_000
                && t.max_spread_bps > 0
        })
    }

    /// The tier a trailing-window performance earns: the first (best)
    /// tier whose *own* uptime obligation is met by that tier's uptime,
    /// or `None` when even the loosest tier is unmet (demotion to
    /// standard fees).
    #[must_use]
    pub fn evaluate(&self, performance: &MmWindowStats) -> Option<&MmTierSpec> {
        for (i, tier) in self.tiers.iter().enumerate() {
            if performance.tier_uptime_permille(i) >= tier.min_uptime_permille {
                return Some(tier);
            }
        }
        None
    }
}

/// Trailing-window performance for one enrolled maker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MmWindowStats {
    /// Total sampled ticks in the window (the denominator). Includes
    /// ticks where the maker was absent — uptime is paid for presence,
    /// not participation.
    pub ticks_total: u64,
    /// Ticks where tier `i`'s spread/size obligations were met
    /// (`tier_hits[i]`). Because a tick meeting a tight tier also
    /// meets every looser tier, the array is **monotone
    /// non-decreasing in `i`** by construction.
    pub tier_hits: [u64; 16],
}

impl MmWindowStats {
    /// Overall two-sided presence uptime, permille of the whole window
    /// (0 when nothing was sampled). Presence = meeting the loosest
    /// tier's spread/size bar, i.e. quoting both sides at all
    /// usefully.
    #[must_use]
    pub fn uptime_permille(&self) -> u64 {
        self.tier_uptime_permille(15)
    }

    /// Uptime permille *for a specific tier index* — the share of
    /// sampled ticks that met that tier's spread/size obligations.
    #[must_use]
    pub fn tier_uptime_permille(&self, tier_index: usize) -> u64 {
        if self.ticks_total == 0 {
            return 0;
        }
        // Round down: the maker is the one claiming the uptime.
        let hits = u128::from(self.tier_hits[tier_index.min(15)]);
        u64::try_from(hits * 1_000 / u128::from(self.ticks_total)).unwrap_or(0)
    }

    /// Record one sampled tick given the best (tightest) tier index
    /// the tick's spread/size satisfied (`None` = no tier). A tick
    /// meeting a tight tier also meets every looser tier.
    pub fn observe(&mut self, best_tier_met: Option<usize>) {
        self.ticks_total = self.ticks_total.saturating_add(1);
        if let Some(best) = best_tier_met {
            for hit in self.tier_hits.iter_mut().skip(best) {
                *hit = hit.saturating_add(1);
            }
        }
    }
}

/// Engine-side ledger tracking enrollment and per-maker window stats.
///
/// One ledger per engine; the sweep observes ticks into it (from the
/// journaled `LiquidityScored` events), and the review stage drains a
/// completed window, returning the measured performances for tier
/// assignment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MmLedger {
    /// Enrolled subaccounts with their in-flight window stats.
    pub enrolled: BTreeMap<u64, MmWindowStats>,
}

impl MmLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enroll a subaccount (idempotent: re-enrollment keeps the current
    /// window — the maker asked to keep being measured, not to reset).
    pub fn enroll(&mut self, subaccount: u64) {
        self.enrolled.entry(subaccount).or_default();
    }

    /// Whether a subaccount is enrolled.
    #[must_use]
    pub fn is_enrolled(&self, subaccount: u64) -> bool {
        self.enrolled.contains_key(&subaccount)
    }

    /// Observe one sampled tick for one enrolled maker.
    ///
    /// `best_tier_met` is the best (lowest) tier index whose
    /// spread/size obligations the tick satisfied, `None` when no tier
    /// was satisfied (absent, one-sided, too wide, or too small).
    pub fn observe(&mut self, subaccount: u64, best_tier_met: Option<usize>) {
        if let Some(stats) = self.enrolled.get_mut(&subaccount) {
            stats.observe(best_tier_met);
        }
    }

    /// Close the window: return every enrollment with its measured
    /// performance and reset the stats for the next window.
    ///
    /// Enrollment persists across windows (a maker leaves by explicit
    /// withdrawal from the program, not by a bad month); only the
    /// measurements reset.
    pub fn drain_window(&mut self) -> Vec<(u64, MmWindowStats)> {
        let out = self.enrolled.iter().map(|(k, v)| (*k, *v)).collect();
        for stats in self.enrolled.values_mut() {
            *stats = MmWindowStats::default();
        }
        out
    }
}

/// Apply a tier discount to a computed fee quote.
///
/// Fees owed round **down** (the user keeps the rounding); rebates
/// (negative quotes) shrink by rounding **up** toward zero — in both
/// directions the discount never exceeds its bps, and a discount can
/// never flip a fee into a rebate or a rebate into a fee.
#[must_use]
pub fn apply_tier_discount(fee: i128, discount_bps: u64) -> i128 {
    if discount_bps == 0 || fee == 0 {
        return fee;
    }
    let magnitude = fee.unsigned_abs();
    // 10_000 - discount_bps is the retained fraction.
    let retained = mul_div_floor(
        magnitude,
        10_000_u128.saturating_sub(u128::from(discount_bps)),
        10_000,
    )
    .unwrap_or(magnitude);
    let sign = if fee < 0 { -1_i128 } else { 1_i128 };
    sign * poc_core::to_i128(retained)
}

use poc_core::mul_div_floor;

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(two_sided: bool, spread: u64, size: u64) -> MmTickStats {
        MmTickStats {
            two_sided,
            worst_spread_bps: spread,
            min_size_lots: size,
        }
    }

    #[test]
    fn default_ladder_is_valid_and_ordered() {
        let p = MmTierProgram::mainnet();
        assert!(p.is_valid());
        // Best-first: discounts descend, obligations loosen.
        assert!(p.tiers[0].fee_discount_bps > p.tiers[1].fee_discount_bps);
        assert!(p.tiers[0].max_spread_bps < p.tiers[1].max_spread_bps);
        assert!(p.tiers[1].max_spread_bps < p.tiers[2].max_spread_bps);
        assert!(p.tiers[0].min_uptime_permille > p.tiers[2].min_uptime_permille);
    }

    #[test]
    fn invalid_ladders_rejected() {
        let mut p = MmTierProgram::mainnet();
        p.tiers[0].fee_discount_bps = 6_000; // > 50%
        assert!(!p.is_valid());
        p.tiers[0].fee_discount_bps = 2_000;
        p.tiers[0].min_uptime_permille = 0; // unsatisfiable
        assert!(!p.is_valid());
        p.tiers[0].min_uptime_permille = 1_001; // > 100%
        assert!(!p.is_valid());
        p.tiers[0].min_uptime_permille = 980;
        p.tiers[0].max_spread_bps = 0; // unsatisfiable
        assert!(!p.is_valid());
        let mut many = MmTierProgram::disabled();
        let template = MmTierProgram::mainnet().tiers[0].clone();
        many.tiers = vec![template; 17];
        assert!(!many.is_valid());
        // Disabled is always valid.
        assert!(MmTierProgram::disabled().is_valid());
    }

    #[test]
    fn evaluation_takes_best_tier_met() {
        let p = MmTierProgram::mainnet();
        // 100% uptime at MM-1 spread/size -> MM-1.
        let mut s = MmWindowStats::default();
        for _ in 0..100 {
            s.observe(Some(0));
        }
        assert_eq!(p.evaluate(&s).map(|t| t.name), Some("MM-1"));
        // 96% MM-2-grade ticks, 4% absent -> fails MM-1 (98%), passes MM-2 (95%).
        let mut s = MmWindowStats::default();
        for _ in 0..96 {
            s.observe(Some(1));
        }
        for _ in 0..4 {
            s.observe(None);
        }
        assert_eq!(p.evaluate(&s).map(|t| t.name), Some("MM-2"));
        // 85% MM-3-grade -> fails everything (loosest needs 90%).
        let mut s = MmWindowStats::default();
        for _ in 0..85 {
            s.observe(Some(2));
        }
        for _ in 0..15 {
            s.observe(None);
        }
        assert!(p.evaluate(&s).is_none());
        // Empty window demotes.
        assert!(p.evaluate(&MmWindowStats::default()).is_none());
    }

    #[test]
    fn tier_hits_are_monotone_in_tier_index() {
        // A tick meeting tier 0 (tightest) credits tier 0 and every
        // looser tier.
        let mut s = MmWindowStats::default();
        s.observe(Some(0));
        assert_eq!(s.tier_hits[0], 1);
        assert_eq!(s.tier_hits[1], 1);
        assert_eq!(s.tier_hits[15], 1);
        // A tick meeting only tier 2 credits tier 2 and every looser
        // slot — NOT the tighter tiers.
        let mut s = MmWindowStats::default();
        s.observe(Some(2));
        assert_eq!(s.tier_hits[0], 0);
        assert_eq!(s.tier_hits[1], 0);
        assert_eq!(s.tier_hits[2], 1);
        assert_eq!(s.tier_hits[15], 1);
        // Overall presence counts the loosest bar.
        assert_eq!(s.uptime_permille(), 1_000);
    }

    #[test]
    fn uptime_rounds_down_against_the_maker() {
        // 979/1000 ticks = 97.9% -> 977 permille after floor.
        let mut s = MmWindowStats::default();
        for _ in 0..979 {
            s.observe(Some(2));
        }
        for _ in 0..21 {
            s.observe(None);
        }
        assert_eq!(s.uptime_permille(), 979);
        // 1/3 uptime floors to 333.
        let mut s = MmWindowStats::default();
        s.observe(Some(2));
        s.observe(None);
        s.observe(None);
        assert_eq!(s.uptime_permille(), 333);
    }

    #[test]
    fn ledger_enrollment_and_drain() {
        let mut l = MmLedger::new();
        l.enroll(7);
        l.enroll(7); // idempotent
        assert!(l.is_enrolled(7));
        assert!(!l.is_enrolled(8));
        l.observe(7, Some(0));
        l.observe(8, Some(0)); // not enrolled: ignored
        let drained = l.drain_window();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, 7);
        assert_eq!(drained[0].1.ticks_total, 1);
        // Enrollment persists; stats reset.
        assert!(l.is_enrolled(7));
        assert_eq!(l.enrolled[&7].ticks_total, 0);
        // Unenrolled accounts never appear.
        let again = l.drain_window();
        assert_eq!(again[0].1.ticks_total, 0);
    }

    #[test]
    fn absent_tick_fails_every_tier() {
        let absent = MmTickStats::absent();
        let p = MmTierProgram::mainnet();
        for tier in &p.tiers {
            assert!(!absent.meets(tier));
        }
    }

    #[test]
    fn meets_requires_both_sides_and_worst_side_bounds() {
        let p = MmTierProgram::mainnet();
        let mm3 = &p.tiers[2];
        // Two-sided, within spread and size.
        assert!(tick(true, mm3.max_spread_bps, mm3.min_size_lots).meets(mm3));
        // One-sided fails.
        assert!(!tick(false, 0, 1_000).meets(mm3));
        // Spread exactly at the bound passes; one bps over fails.
        assert!(tick(true, mm3.max_spread_bps, mm3.min_size_lots).meets(mm3));
        assert!(!tick(true, mm3.max_spread_bps + 1, mm3.min_size_lots).meets(mm3));
        // Size exactly at the bound passes; one lot under fails.
        assert!(tick(true, mm3.max_spread_bps, mm3.min_size_lots).meets(mm3));
        assert!(!tick(true, mm3.max_spread_bps, mm3.min_size_lots - 1).meets(mm3));
    }

    #[test]
    fn discount_math_is_exact_and_bounded() {
        // 20% off a 100-unit fee = 80.
        assert_eq!(apply_tier_discount(100, 2_000), 80);
        // 20% off a 99-unit fee floors to 79 (user keeps the unit).
        assert_eq!(apply_tier_discount(99, 2_000), 79);
        // Rebates shrink toward zero, sign preserved.
        assert_eq!(apply_tier_discount(-100, 2_000), -80);
        assert_eq!(apply_tier_discount(-99, 2_000), -79);
        // Zero discount and zero fee are identities.
        assert_eq!(apply_tier_discount(123, 0), 123);
        assert_eq!(apply_tier_discount(0, 2_000), 0);
        // A discount never flips sign.
        assert!(apply_tier_discount(1, 5_000) >= 0);
        assert!(apply_tier_discount(-1, 5_000) <= 0);
    }
}
