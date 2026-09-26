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
}

impl Default for IncentiveParams {
    fn default() -> Self {
        Self {
            max_spread_bps: 50, // 0.5% of mid
            min_size_lots: 1,
            two_sided_multiplier: 2,
        }
    }
}

/// Accumulates maker scores and settles reward pools pro-rata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidityIncentives {
    params: IncentiveParams,
    /// Running score points per subaccount for the current interval.
    scores: HashMap<SubaccountId, u128>,
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
            lifetime_paid: HashMap::new(),
        }
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
        if size_lots < self.params.min_size_lots
            || spread_bps > self.params.max_spread_bps
            || self.params.max_spread_bps == 0
        {
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
        let total_score: u128 = self.scores.values().copied().sum();

        let mut payments = Vec::new();
        if budget > 0 && total_score > 0 {
            let mut distributed: u128 = 0;
            // Deterministic ascending-id order.
            let mut ids: Vec<SubaccountId> = self.scores.keys().copied().collect();
            ids.sort_unstable();
            for id in ids {
                let score = self.scores[&id];
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
}
