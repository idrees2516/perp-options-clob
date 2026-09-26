//! Liquidity-provider incentive scoring and reward distribution.
//!
//! ## The cold-start problem and its price
//!
//! A new CLOB has traders but no depth, and no depth means no traders. The
//! industry answer — dYdX v4's LP rewards, Blur's bidding pools, Paradigm's
//! maker rebates — is to *rent* liquidity with an explicit budget rather
//! than wait for it to emerge. This module implements that budget as a
//! transparent, rules-based score:
//!
//! * **Score, not fills.** Makers are paid for *quoting*, not for trading —
//!   paying for fills would incentivize wash trading against yourself.
//! * **Proximity-weighted size.** A quote at the touch is worth more than
//!   one 5% away: `points = size × (1 − spread / max_spread)`.
//! * **Two-sided multiplier.** Two-sided quoting is what takers actually
//!   consume; one-sided resting inventory is worth half.
//! * **Budget-bound.** A fixed pool is distributed pro-rata each interval,
//!   dust carries forward, and nothing is ever minted beyond the budget.
//!
//! The pool is therefore a *controllable, depreciating* marketing expense
//! whose ROI (quoted depth per reward unit) is directly measurable — the
//! same accounting discipline Paradigm applies to its own maker programs.

use std::collections::HashMap;

use poc_core::{mul_div_floor, SubaccountId};

/// The fixed reward budget per settlement interval plus carried dust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewardPool {
    /// Reward budget paid out every interval, quote minor units.
    pub per_interval_quote_minor: u128,
    /// Dust from earlier intervals carried forward (conservation).
    pub carried_quote_minor: u128,
}

impl RewardPool {
    /// A pool paying `per_interval` per interval, starting empty.
    #[must_use]
    pub fn new(per_interval_quote_minor: u128) -> Self {
        Self {
            per_interval_quote_minor,
            carried_quote_minor: 0,
        }
    }

    /// Total spendable at the next settlement.
    #[must_use]
    pub fn available(&self) -> u128 {
        self.per_interval_quote_minor
            .saturating_add(self.carried_quote_minor)
    }
}

/// The result of one reward settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewardSettlement {
    /// `(subaccount, payment)` pairs, deterministic order (ascending id).
    pub payments: Vec<(SubaccountId, u128)>,
    /// Undistributed dust carried to the next interval.
    pub carried_quote_minor: u128,
}

impl RewardSettlement {
    /// Total actually paid out.
    #[must_use]
    pub fn paid_total(&self) -> u128 {
        self.payments.iter().map(|&(_, p)| p).sum()
    }
}

/// Scoring parameters for maker observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncentiveParams {
    /// Quotes further than this spread (bps) from mid score nothing.
    pub max_spread_bps: u64,
    /// Quotes smaller than this size (lots) score nothing.
    pub min_size_lots: u64,
    /// Multiplier applied to two-sided quoting, in "x" units (2 = double).
    pub two_sided_multiplier: u64,
    /// Resting notional at touch below this floor scores nothing — the
    /// uptime-grinding defense (quoting one lot on a dead market all
    /// interval must not farm the reward pool).
    pub min_touch_notional_quote_minor: u128,
    /// Cancel-to-quote ratio above which the interval score is penalized
    /// (see [`LiquidityIncentives::on_quote_cancelled`]). The penalty is
    /// linear: a maker whose quote ratio is at/below this floor is
    /// unaffected; at zero quotes kept the score halves (quote-and-pull
    /// defense).
    pub min_quote_ratio_bps: u64,
    /// Lower bound of the randomized sampling delay, ms.
    pub sample_delay_min_ms: u64,
    /// Span of the randomized sampling delay, ms (delay is uniform over
    /// `[min, min+span)` from a deterministic PRNG — unannounced sample
    /// instants make the score an unbiased estimator of the quoting
    /// time-integral).
    pub sample_delay_span_ms: u64,
}

impl Default for IncentiveParams {
    fn default() -> Self {
        Self {
            max_spread_bps: 50, // 0.5% of mid
            min_size_lots: 1,
            two_sided_multiplier: 2,
            min_touch_notional_quote_minor: 0,
            min_quote_ratio_bps: 5_000, // 50% quotes kept -> penalty-free
            sample_delay_min_ms: 30_000,
            sample_delay_span_ms: 300_000,
        }
    }
}

/// Deterministic splitmix64 — the PRNG behind unannounced scoring
/// instants (no wall-clock entropy anywhere: replay reproduces it).
#[must_use]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The deterministic delay before the next unannounced scoring sample.
///
/// `sample_counter` is the number of samples taken so far (engine-side,
/// advanced only when a sample is taken — so replay of the same tick
/// sequence reproduces the same instants). Returns a delay in
/// `[min, min + span)` ms.
#[must_use]
pub fn sample_delay(sample_counter: u64, params: &IncentiveParams) -> u64 {
    let span = params.sample_delay_span_ms.max(1);
    let r = splitmix64(sample_counter.wrapping_add(0x5DEE_CE66_D000_0001)) % u64::from(span);
    params.sample_delay_min_ms + r
}

/// Accumulates maker scores and settles reward pools pro-rata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidityIncentives {
    params: IncentiveParams,
    /// Running score points per subaccount for the current interval.
    scores: HashMap<SubaccountId, u128>,
    /// Quotes placed this interval (cancel-to-quote ratio numerator).
    quotes_placed: HashMap<SubaccountId, u64>,
    /// Quotes cancelled this interval (quote-and-pull tracking).
    quotes_cancelled: HashMap<SubaccountId, u64>,
    /// Lifetime rewards paid per subaccount (audit trail).
    lifetime_paid: HashMap<SubaccountId, u128>,
}

impl Default for LiquidityIncentives {
    fn default() -> Self {
        Self::new(IncentiveParams::default())
    }
}

impl LiquidityIncentives {
    /// New incentive tracker with the given parameters.
    #[must_use]
    pub fn new(params: IncentiveParams) -> Self {
        Self {
            params,
            scores: HashMap::new(),
            quotes_placed: HashMap::new(),
            quotes_cancelled: HashMap::new(),
            lifetime_paid: HashMap::new(),
        }
    }

    /// Record a quote placement (order resting) for the cancel-to-quote
    /// ratio. Only resting limit placements count — IOC sweeps are not
    /// quoting.
    pub fn on_quote_placed(&mut self, subaccount: SubaccountId) {
        *self.quotes_placed.entry(subaccount).or_insert(0) += 1;
    }

    /// Record a quote cancellation. A maker who repeatedly places and
    /// pulls quotes inside the scoring band still accrues raw score
    /// points; the ratio penalty applied at settle is the counterweight.
    pub fn on_quote_cancelled(&mut self, subaccount: SubaccountId) {
        *self.quotes_cancelled.entry(subaccount).or_insert(0) += 1;
    }

    /// The quote-ratio penalty multiplier for one subaccount, in bps of
    /// the raw score (10_000 = unpenalized). Zero-activity makers are
    /// unpenalized (their score is already zero).
    #[must_use]
    pub fn ratio_penalty_bps(&self, subaccount: SubaccountId) -> u64 {
        let placed = u128::from(self.quotes_placed.get(&subaccount).copied().unwrap_or(0));
        let cancelled = u128::from(self.quotes_cancelled.get(&subaccount).copied().unwrap_or(0));
        let total = placed.checked_add(cancelled).unwrap_or(u128::MAX);
        if total == 0 {
            return 10_000;
        }
        // kept_ratio = placed / total, in bps.
        let kept_bps =
            u64::try_from(mul_div_floor(placed, 10_000, total).unwrap_or(0)).unwrap_or(0);
        // Free below the configured floor; linear to half-score at zero kept.
        if kept_bps >= self.params.min_quote_ratio_bps || self.params.min_quote_ratio_bps == 0 {
            return 10_000;
        }
        let above = u128::from(self.params.min_quote_ratio_bps);
        let below = u128::from(kept_bps);
        // penalty multiplier: 5000 + 5000 * kept / floor  (floor=5000 default)
        let scaled = 5_000_u128.saturating_add(mul_div_floor(below, 5_000, above).unwrap_or(0));
        u64::try_from(scaled.min(10_000)).unwrap_or(10_000)
    }

    /// Record one observation of a maker's quoting state.
    ///
    /// * `size_lots` — resting size within `max_spread_bps` of mid;
    /// * `spread_bps` — the *tighter* side's distance from mid if two-sided;
    /// * `two_sided` — maker was simultaneously resting both sides.
    ///
    /// Returns the score points contributed (0 when the quote qualifies for
    /// nothing), so the engine can surface live scoreboard data.
    pub fn on_observation(
        &mut self,
        subaccount: SubaccountId,
        size_lots: u64,
        spread_bps: u64,
        two_sided: bool,
    ) -> u128 {
        self.on_observation_with_notional(subaccount, size_lots, spread_bps, two_sided, u128::MAX)
    }

    /// [`LiquidityIncentives::on_observation`] with the resting notional at
    /// touch — the uptime-grinding floor applies (`min_touch_notional`).
    pub fn on_observation_with_notional(
        &mut self,
        subaccount: SubaccountId,
        size_lots: u64,
        spread_bps: u64,
        two_sided: bool,
        touch_notional_quote_minor: u128,
    ) -> u128 {
        if size_lots < self.params.min_size_lots
            || spread_bps > self.params.max_spread_bps
            || self.params.max_spread_bps == 0
        {
            return 0;
        }
        if touch_notional_quote_minor < self.params.min_touch_notional_quote_minor {
            return 0;
        }
        // Proximity factor in bps of the maximum: (max - spread)/max.
        let proximity_bps = 10_000_u128
            .saturating_mul(u128::from(self.params.max_spread_bps - spread_bps))
            / u128::from(self.params.max_spread_bps);
        let mut points = u128::from(size_lots).saturating_mul(proximity_bps);
        if two_sided {
            points = points.saturating_mul(u128::from(self.params.two_sided_multiplier));
        }
        let entry = self.scores.entry(subaccount).or_insert(0);
        *entry = entry.saturating_add(points);
        points
    }

    /// Current score of a subaccount (live scoreboard).
    #[must_use]
    pub fn score_of(&self, subaccount: SubaccountId) -> u128 {
        self.scores.get(&subaccount).copied().unwrap_or(0)
    }

    /// All current scores, ascending by subaccount id (deterministic).
    #[must_use]
    pub fn scoreboard(&self) -> Vec<(SubaccountId, u128)> {
        let mut board: Vec<(SubaccountId, u128)> =
            self.scores.iter().map(|(&k, &v)| (k, v)).collect();
        board.sort_unstable();
        board
    }

    /// Settle the pool pro-rata by score.
    ///
    /// * Every participant's payment is floored (users are credited);
    /// * the dust is carried into the next interval, so the pool never
    ///   creates or destroys quote units;
    /// * scores reset for the next interval; an empty pool still resets.
    pub fn settle(&mut self, pool: &mut RewardPool) -> RewardSettlement {
        let budget = pool.available();
        // Penalized scores drive both the numerator and the denominator so
        // pro-rata shares stay normalized (dust, not misallocation, absorbs
        // the penalty).
        let penalized: Vec<(SubaccountId, u128)> = self
            .scores
            .iter()
            .map(|(&id, &raw)| {
                let penalty = u128::from(self.ratio_penalty_bps(id));
                let score = mul_div_floor(raw, penalty, 10_000).unwrap_or(raw);
                (id, score)
            })
            .collect();
        let total_score: u128 = penalized.iter().map(|(_, s)| *s).sum();

        let mut payments = Vec::new();
        if budget > 0 && total_score > 0 {
            let mut distributed: u128 = 0;
            // Deterministic ascending-id order.
            let mut ids: Vec<SubaccountId> = penalized.iter().map(|(id, _)| *id).collect();
            ids.sort_unstable();
            for id in ids {
                let score = penalized
                    .iter()
                    .find(|(i, _)| *i == id)
                    .map_or(0, |(_, s)| *s);
                if let Some(payment) = mul_div_floor(budget, score, total_score) {
                    if payment > 0 {
                        distributed = distributed.saturating_add(payment);
                        let lifetime = self.lifetime_paid.entry(id).or_insert(0);
                        *lifetime = lifetime.saturating_add(payment);
                        payments.push((id, payment));
                    }
                }
            }
            pool.carried_quote_minor = budget - distributed.min(budget);
        } else {
            pool.carried_quote_minor = budget;
        }

        self.scores.clear();
        self.quotes_placed.clear();
        self.quotes_cancelled.clear();
        RewardSettlement {
            payments,
            carried_quote_minor: pool.carried_quote_minor,
        }
    }

    /// Lifetime rewards paid to a subaccount.
    #[must_use]
    pub fn lifetime_paid(&self, subaccount: SubaccountId) -> u128 {
        self.lifetime_paid.get(&subaccount).copied().unwrap_or(0)
    }

    /// Total rewards paid across all subaccounts.
    #[must_use]
    pub fn lifetime_paid_total(&self) -> u128 {
        self.lifetime_paid.values().copied().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> LiquidityIncentives {
        LiquidityIncentives::new(IncentiveParams {
            max_spread_bps: 100,
            min_size_lots: 1,
            two_sided_multiplier: 2,
            ..IncentiveParams::default()
        })
    }

    #[test]
    fn spread_zero_scores_full() {
        let mut t = tracker();
        let pts = t.on_observation(1, 10, 0, false);
        // size 10 * (100-0)/100 * 10000 bps = 10 * 10000 = 100_000
        assert_eq!(pts, 100_000);
        assert_eq!(t.score_of(1), 100_000);
    }

    #[test]
    fn wide_spread_scores_zero() {
        let mut t = tracker();
        assert_eq!(t.on_observation(1, 10, 101, false), 0);
        assert_eq!(t.on_observation(1, 10, 100, false), 0); // exactly at max: zero
        assert_eq!(t.score_of(1), 0);
    }

    #[test]
    fn two_sided_doubles() {
        let mut t = tracker();
        let one_sided = t.on_observation(1, 10, 50, false);
        let two_sided = t.on_observation(2, 10, 50, true);
        assert_eq!(two_sided, one_sided * 2);
    }

    #[test]
    fn dust_size_scores_zero() {
        let mut t = LiquidityIncentives::new(IncentiveParams {
            min_size_lots: 5,
            ..IncentiveParams::default()
        });
        assert_eq!(t.on_observation(1, 4, 0, true), 0);
        assert!(t.on_observation(1, 5, 0, true) > 0);
    }

    #[test]
    fn pro_rata_settlement_conserves() {
        let mut t = tracker();
        // Two makers: scores 3:1.
        t.on_observation(1, 30, 0, false); // 300_000 points
        t.on_observation(2, 10, 0, false); // 100_000 points
        let mut pool = RewardPool::new(10_000);
        let s = t.settle(&mut pool);
        assert_eq!(s.paid_total() + s.carried_quote_minor, 10_000);
        assert_eq!(s.payments.len(), 2);
        // 3:1 split of 10_000 = 7500 / 2500 exactly.
        assert_eq!(s.payments[0], (1, 7_500));
        assert_eq!(s.payments[1], (2, 2_500));
        assert_eq!(pool.carried_quote_minor, 0);
    }

    #[test]
    fn dust_carries_forward() {
        let mut t = tracker();
        // Scores 1:1:1 on a budget of 10 -> 3,3,3 = 9 paid, 1 carried.
        t.on_observation(1, 10, 0, false);
        t.on_observation(2, 10, 0, false);
        t.on_observation(3, 10, 0, false);
        let mut pool = RewardPool::new(10);
        let s = t.settle(&mut pool);
        assert_eq!(s.paid_total(), 9);
        assert_eq!(s.carried_quote_minor, 1);
        assert_eq!(pool.available(), 11, "next interval spends budget + carry");
    }

    #[test]
    fn empty_scores_carry_everything() {
        let mut t = tracker();
        let mut pool = RewardPool::new(500);
        let s = t.settle(&mut pool);
        assert!(s.payments.is_empty());
        assert_eq!(s.carried_quote_minor, 500);
        assert_eq!(t.settle(&mut RewardPool::new(0)).paid_total(), 0);
    }

    #[test]
    fn scores_reset_between_intervals() {
        let mut t = tracker();
        t.on_observation(1, 100, 0, false);
        let mut pool = RewardPool::new(1_000);
        let s = t.settle(&mut pool);
        assert_eq!(s.paid_total(), 1_000);
        assert_eq!(pool.carried_quote_minor, 0);
        assert_eq!(t.score_of(1), 0, "scores reset after settlement");
        // No scores this interval: whole budget carries.
        let s2 = t.settle(&mut pool);
        assert!(s2.payments.is_empty());
        assert_eq!(s2.carried_quote_minor, 1_000);
    }

    #[test]
    fn lifetime_tracking() {
        let mut t = tracker();
        t.on_observation(1, 10, 0, false);
        let mut pool = RewardPool::new(1_000);
        let s = t.settle(&mut pool);
        let paid1 = s.payments[0].1;
        t.on_observation(1, 10, 0, false);
        let s2 = t.settle(&mut pool);
        let paid2 = s2.payments[0].1;
        assert_eq!(t.lifetime_paid(1), paid1 + paid2);
        assert_eq!(t.lifetime_paid_total(), paid1 + paid2);
        assert_eq!(t.lifetime_paid(99), 0);
    }

    #[test]
    fn deterministic_scoreboard_order() {
        let mut t = tracker();
        for id in [9_u64, 3, 7, 1] {
            t.on_observation(id, 5, 0, false);
        }
        let ids: Vec<SubaccountId> = t.scoreboard().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![1, 3, 7, 9]);
    }

    #[test]
    fn quote_and_pull_penalty_binds() {
        let mut t = tracker();
        // Honest maker: 4 quotes placed, 1 cancelled -> 80% kept, above floor.
        for _ in 0..4 {
            t.on_quote_placed(1);
        }
        t.on_quote_cancelled(1);
        t.on_observation(1, 100, 0, true);
        assert_eq!(t.ratio_penalty_bps(1), 10_000);
        // Quote-and-puller: 1 placed, 9 cancelled -> 10% kept, deep penalty.
        t.on_quote_placed(2);
        for _ in 0..9 {
            t.on_quote_cancelled(2);
        }
        t.on_observation(2, 100, 0, true);
        let p2 = t.ratio_penalty_bps(2);
        assert!(p2 < 10_000, "penalized: {p2}");
        assert!(p2 >= 5_000, "halved at most: {p2}");
        // No activity at all: unpenalized.
        assert_eq!(t.ratio_penalty_bps(3), 10_000);
    }

    #[test]
    fn notional_floor_blocks_uptime_grinding() {
        let mut t = LiquidityIncentives::new(IncentiveParams {
            min_touch_notional_quote_minor: 1_000,
            ..IncentiveParams::default()
        });
        assert_eq!(t.on_observation_with_notional(1, 10, 0, true, 500), 0);
        assert!(t.on_observation_with_notional(1, 10, 0, true, 1_500) > 0);
    }

    #[test]
    fn sample_jitter_is_deterministic_and_in_range() {
        let params = IncentiveParams::default();
        let a = sample_delay(7, &params);
        let b = sample_delay(7, &params);
        assert_eq!(a, b, "deterministic");
        assert!(a >= params.sample_delay_min_ms);
        assert!(a < params.sample_delay_min_ms + params.sample_delay_span_ms);
        let c = sample_delay(8, &params);
        // No guarantee of difference, but across 100 counters at least a few
        // distinct values must appear (statistical sanity on the PRNG).
        let distinct: std::collections::HashSet<_> =
            (0..100).map(|i| sample_delay(i, &params)).collect();
        assert!(
            distinct.len() > 10,
            "jitter actually varies: {}",
            distinct.len()
        );
    }
}
