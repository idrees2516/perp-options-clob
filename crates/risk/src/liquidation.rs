//! Liquidation pipeline: partial closures, insurance fund, auto-deleverage.
//!
//! The planner is **pure**: given the account, registry, and marks it
//! produces a [`LiquidationPlan`] of position closures at penalized
//! prices. The engine executes the plan (book first, insurance as buyer of
//! last resort) and journals the events — planning and execution are
//! separate so replays are exact.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};

use poc_core::{mul_div, to_i128, Instrument, Side, SubaccountId, Symbol, TimestampMs};
use poc_margin::{MarginAccount, MarkSet, PortfolioMarginEngine};

/// Parameters of the liquidation cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidationParams {
    /// Penalty charged on liquidated notional, bps — flows to the
    /// insurance fund for being the buyer of last resort.
    pub penalty_bps: u64,
    /// After partial liquidation, restore equity to at least this fraction
    /// over maintenance (bps of maintenance) so the account is not
    /// immediately re-liquidated by the next tick.
    pub restoration_buffer_bps: u64,
    /// Equity below which an account is bankrupt (insurance/ADL territory).
    /// Expresses maintenance ≤ equity < 0 as: equity < 0 → bankrupt.
    /// This field is the buffer above zero equity used to decide early
    /// bankruptcy (0 = only true negative equity triggers it).
    pub bankruptcy_buffer: u128,
}

impl Default for LiquidationParams {
    fn default() -> Self {
        Self {
            penalty_bps: 125,              // 1.25%, Deribit-shaped
            restoration_buffer_bps: 2_000, // restore to maintenance × 1.2
            bankruptcy_buffer: 0,
        }
    }
}

/// One position closure the engine must execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidationAction {
    /// Instrument to close.
    pub symbol: Symbol,
    /// The side the engine *sells* (opposite of the liquidated position).
    pub closing_side: Side,
    /// Lots to close.
    pub lots: u64,
    /// Floor/ceiling price (quote minor per base) the account receives /
    /// pays — mark adjusted by the liquidation penalty.
    pub penalized_price_quote_minor: u128,
}

/// The full plan for one liquidatable account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidationPlan {
    /// The account being liquidated.
    pub subaccount: SubaccountId,
    /// Closures, largest risk contributor first.
    pub actions: Vec<LiquidationAction>,
    /// Projected equity after executing the plan at penalized prices.
    pub projected_equity_after: i128,
    /// Projected maintenance after the plan.
    pub projected_maintenance_after: u128,
    /// The plan empties the account and equity is still below the
    /// bankruptcy line → insurance fund / ADL territory.
    pub bankrupt: bool,
}

/// Deterministic priority queue of accounts pending liquidation, most
/// severe deficit first.
///
/// Severity = `maintenance − equity` (how much capital the account is
/// short). Ties broken by subaccount id — full determinism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidationCandidate {
    /// The account.
    pub subaccount: SubaccountId,
    /// Current equity (signed).
    pub equity_quote_minor: i128,
    /// Current maintenance requirement.
    pub maintenance_quote_minor: u128,
}

impl LiquidationCandidate {
    /// Deficit in quote minor (0 when healthy): how far equity sits below
    /// maintenance, counting negative equity fully.
    #[must_use]
    pub fn deficit(&self) -> u128 {
        (to_i128(self.maintenance_quote_minor) - self.equity_quote_minor)
            .max(0)
            .unsigned_abs()
    }
}

impl Ord for LiquidationCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap: larger deficit sorts greater so it
        // pops first; ties break on the smaller subaccount id.
        self.deficit()
            .cmp(&other.deficit())
            .then_with(|| self.subaccount.cmp(&other.subaccount))
    }
}

impl PartialOrd for LiquidationCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Priority queue wrapper (max-heap on severity).
#[derive(Debug, Default)]
pub struct LiquidationQueue {
    heap: BinaryHeap<LiquidationCandidate>,
    /// Dedup: an account is queued at most once.
    queued: BTreeMap<SubaccountId, ()>,
}

impl LiquidationQueue {
    /// Empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer an account for liquidation. Ignored when healthy or already
    /// queued (the *first* severity snapshot wins — deterministic).
    pub fn offer(&mut self, candidate: LiquidationCandidate) -> bool {
        if candidate.deficit() == 0 || self.queued.contains_key(&candidate.subaccount) {
            return false;
        }
        self.queued.insert(candidate.subaccount, ());
        self.heap.push(candidate);
        true
    }

    /// Pop the most severe case.
    pub fn pop(&mut self) -> Option<LiquidationCandidate> {
        let c = self.heap.pop()?;
        self.queued.remove(&c.subaccount);
        Some(c)
    }

    /// Queue length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Drop an account from the queue (e.g. it self-healed via deposits).
    pub fn remove(&mut self, subaccount: SubaccountId) {
        self.queued.remove(&subaccount);
        self.heap.retain(|c| c.subaccount != subaccount);
    }
}

/// The buyer of last resort.
///
/// The fund collects liquidation penalties (its income) and absorbs
/// bankruptcy shortfalls (its expense). Its balance is the venue's promise
/// that liquidations will not socialize losses while it holds capital.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsuranceFund {
    balance_quote_minor: i128,
    lifetime_penalties: u128,
    lifetime_absorbed: u128,
    /// Total number of bankruptcies absorbed.
    bankruptcies: u64,
    /// Timestamped balance history for audit trails.
    history: Vec<(TimestampMs, i128)>,
}

impl InsuranceFund {
    /// A fund seeded with `seed` quote minor.
    #[must_use]
    pub fn new(seed_quote_minor: u128) -> Self {
        Self {
            balance_quote_minor: to_i128(seed_quote_minor),
            lifetime_penalties: 0,
            lifetime_absorbed: 0,
            bankruptcies: 0,
            history: Vec::new(),
        }
    }

    /// Current balance (may be negative under extreme stress).
    #[must_use]
    pub fn balance(&self) -> i128 {
        self.balance_quote_minor
    }

    /// Credit liquidation penalty income.
    pub fn credit_penalty(&mut self, amount_quote_minor: u128, ts: TimestampMs) {
        if amount_quote_minor == 0 {
            return;
        }
        self.lifetime_penalties = self.lifetime_penalties.saturating_add(amount_quote_minor);
        self.balance_quote_minor = self
            .balance_quote_minor
            .saturating_add(to_i128(amount_quote_minor));
        self.history.push((ts, self.balance_quote_minor));
    }

    /// Absorb a bankruptcy shortfall. Returns `false` when the fund is
    /// exhausted and the shortfall must be socialized (ADL).
    pub fn absorb(&mut self, shortfall_quote_minor: u128, ts: TimestampMs) -> bool {
        self.lifetime_absorbed = self.lifetime_absorbed.saturating_add(shortfall_quote_minor);
        self.balance_quote_minor = self
            .balance_quote_minor
            .saturating_sub(to_i128(shortfall_quote_minor));
        self.bankruptcies = self.bankruptcies.saturating_add(1);
        self.history.push((ts, self.balance_quote_minor));
        self.balance_quote_minor >= 0
    }

    /// Whether the fund can absorb a shortfall without going negative.
    #[must_use]
    pub fn can_absorb(&self, shortfall_quote_minor: u128) -> bool {
        self.balance_quote_minor >= 0 && to_i128(shortfall_quote_minor) <= self.balance_quote_minor
    }

    /// Lifetime statistics.
    #[must_use]
    pub fn stats(&self) -> (u128, u128, u64) {
        (
            self.lifetime_penalties,
            self.lifetime_absorbed,
            self.bankruptcies,
        )
    }
}

/// Produces liquidation plans.
pub struct LiquidationPlanner {
    params: LiquidationParams,
}

impl LiquidationPlanner {
    /// A planner with the given parameters.
    #[must_use]
    pub fn new(params: LiquidationParams) -> Self {
        Self { params }
    }

    /// The planner's parameters.
    #[must_use]
    pub fn params(&self) -> LiquidationParams {
        self.params
    }

    /// Plan the liquidation cascade for an account.
    ///
    /// Strategy (documented, deterministic):
    ///
    /// 1. Rank underlyings by their maintenance contribution — the biggest
    ///    risk source is closed first (SPAN decomposition guides the axe).
    /// 2. Within an underlying, close positions by absolute notional desc.
    /// 3. Close legs fully until projected equity ≥ maintenance ×
    ///    (1 + restoration buffer), then minimize disruption by bisection:
    ///    reopen the smallest fraction of the last leg that still restores
    ///    the target.
    /// 4. Legs are priced at the penalized mark: longs sell at
    ///    `mark × (1 − p)`, shorts buy back at `mark × (1 + p)`.
    /// 5. If the account empties with equity still under the bankruptcy
    ///    line, the plan is flagged bankrupt (insurance / ADL follows).
    #[must_use]
    pub fn plan(
        &self,
        subaccount: SubaccountId,
        account: &MarginAccount,
        instruments: &BTreeMap<Symbol, Instrument>,
        marks: &BTreeMap<String, MarkSet>,
        margin_engine: &PortfolioMarginEngine,
    ) -> Option<LiquidationPlan> {
        let summary = margin_engine.margin_summary(account, instruments, marks)?;
        if summary.equity_quote_minor >= to_i128(summary.maintenance_quote_minor) {
            return None; // healthy: nothing to plan
        }
        let target = restore_target(
            summary.maintenance_quote_minor,
            self.params.restoration_buffer_bps,
        );
        let _ = target; // superseded by per-probe restoration checks

        // ---- Rank underlyings by maintenance contribution -----------------
        let mut underlying_margin: Vec<(String, u128)> = Vec::new();
        for mark_set in marks.values() {
            if let Some(m) = margin_engine.scan_underlying(account, instruments, mark_set) {
                underlying_margin.push((mark_set.base_symbol.clone(), m.maintenance_quote_minor));
            }
        }
        underlying_margin.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        // ---- Flatten legs: underlying order, then notional desc -----------
        let mut legs: Vec<LegClosure> = Vec::new();
        for (base, _) in &underlying_margin {
            let mark_set = marks.get(base)?;
            let mut symbol_legs: Vec<LegClosure> = Vec::new();
            for (symbol, position) in &account.positions {
                if position.signed_lots == 0 {
                    continue;
                }
                let instrument = instruments.get(symbol)?;
                if instrument.base_symbol() != base {
                    continue;
                }
                let mark_price = mark_price_of(mark_set, symbol)?;
                let notional = instrument
                    .position_notional_minor(mark_price, position.signed_lots)
                    .unwrap_or(0);
                symbol_legs.push(LegClosure {
                    symbol: symbol.clone(),
                    signed_lots: position.signed_lots,
                    mark_quote_minor: mark_price,
                    notional,
                });
            }
            symbol_legs.sort_by(|a, b| {
                b.notional
                    .cmp(&a.notional)
                    .then_with(|| a.symbol.cmp(&b.symbol))
            });
            legs.extend(symbol_legs);
        }

        // ---- Simulate closures until restored ------------------------------
        // A state is "restored" when its equity covers its *own* remaining
        // maintenance plus the restoration buffer — evaluated per probe
        // state, never against the pre-liquidation target (closing at
        // penalized prices realizes losses, so both sides of the
        // inequality move).
        let restored = |acct: &MarginAccount| -> bool {
            margin_engine
                .margin_summary(acct, instruments, marks)
                .is_some_and(|s| {
                    s.equity_quote_minor
                        >= to_i128(restore_target(
                            s.maintenance_quote_minor,
                            self.params.restoration_buffer_bps,
                        ))
                })
        };

        let mut sim = account.clone();
        let mut actions: Vec<LiquidationAction> = Vec::new();

        for leg in &legs {
            let instrument = instruments.get(&leg.symbol)?;
            let closing_side = if leg.signed_lots > 0 {
                Side::Ask
            } else {
                Side::Bid
            };
            let penalized = penalize(leg.mark_quote_minor, closing_side, self.params.penalty_bps);
            let lots_abs = leg.signed_lots.unsigned_abs();

            sim.apply_fill(instrument, &leg.symbol, closing_side, lots_abs, penalized);
            actions.push(LiquidationAction {
                symbol: leg.symbol.clone(),
                closing_side,
                lots: lots_abs,
                penalized_price_quote_minor: penalized,
            });

            if restored(&sim) {
                break;
            }
        }

        // ---- Minimize disruption: bisect the smallest lot count of the
        // last closed leg that still restores the account ---------------------
        if let Some(last) = actions.last().cloned() {
            if last.lots > 1 && restored(&sim) {
                if let Some(instrument) = instruments.get(&last.symbol) {
                    let probe = |lots: u64| -> bool {
                        let mut p = account.clone();
                        for a in &actions[..actions.len() - 1] {
                            if let Some(inst) = instruments.get(&a.symbol) {
                                p.apply_fill(
                                    inst,
                                    &a.symbol,
                                    a.closing_side,
                                    a.lots,
                                    a.penalized_price_quote_minor,
                                );
                            }
                        }
                        p.apply_fill(
                            instrument,
                            &last.symbol,
                            last.closing_side,
                            lots,
                            last.penalized_price_quote_minor,
                        );
                        restored(&p)
                    };
                    let full = last.lots;
                    let mut best = full;
                    let mut lo = 1_u64;
                    let mut hi = full.saturating_sub(1);
                    while lo <= hi {
                        let mid = lo + (hi - lo) / 2;
                        if probe(mid) {
                            best = mid;
                            hi = mid - 1;
                        } else {
                            lo = mid + 1;
                        }
                    }
                    if best < full {
                        if let Some(a) = actions.last_mut() {
                            a.lots = best;
                        }
                    }
                }
            }
        }

        // ---- Project the final plan -----------------------------------------
        let mut projected = account.clone();
        for a in &actions {
            if let Some(inst) = instruments.get(&a.symbol) {
                projected.apply_fill(
                    inst,
                    &a.symbol,
                    a.closing_side,
                    a.lots,
                    a.penalized_price_quote_minor,
                );
            }
        }
        let final_summary = margin_engine.margin_summary(&projected, instruments, marks);
        let (equity_after, maint_after) = final_summary.as_ref().map_or(
            (summary.equity_quote_minor, summary.maintenance_quote_minor),
            |s| (s.equity_quote_minor, s.maintenance_quote_minor),
        );
        let bankrupt = actions.len() == legs.len()
            && equity_after < 0
            && projected.positions.values().all(|p| p.is_flat());

        Some(LiquidationPlan {
            subaccount,
            actions,
            projected_equity_after: equity_after,
            projected_maintenance_after: maint_after,
            bankrupt,
        })
    }
}

/// A closure candidate leg.
struct LegClosure {
    symbol: Symbol,
    signed_lots: i64,
    mark_quote_minor: u128,
    notional: u128,
}

/// Mark price of a symbol from the set (per-base, or premium for options).
fn mark_price_of(mark_set: &MarkSet, symbol: &str) -> Option<u128> {
    match mark_set.marks.get(symbol)? {
        poc_margin::Mark::Perp {
            mark_quote_minor_per_base,
        } => Some(*mark_quote_minor_per_base),
        poc_margin::Mark::Option {
            premium_quote_minor_per_base,
            ..
        } => Some(*premium_quote_minor_per_base),
    }
}

/// Restoration target: `maintenance × (1 + buffer_bps/10_000)`.
fn restore_target(maintenance: u128, buffer_bps: u64) -> u128 {
    mul_div(
        maintenance,
        10_000 + u128::from(buffer_bps),
        10_000,
        poc_core::Rounding::Ceil,
    )
    .unwrap_or(maintenance)
}

/// Penalized execution price for a closing side.
///
/// Closing a *long* sells into the penalty: `mark × (1 − p)`.
/// Closing a *short* buys back into it: `mark × (1 + p)`.
fn penalize(mark: u128, closing_side: Side, penalty_bps: u64) -> u128 {
    let scale = match closing_side {
        Side::Ask => 10_000_u128.saturating_sub(u128::from(penalty_bps)),
        Side::Bid => 10_000_u128.saturating_add(u128::from(penalty_bps)),
    };
    mul_div(mark, scale, 10_000, poc_core::Rounding::Ceil).unwrap_or(mark)
}

/// A counterparty eligible for auto-deleveraging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdlCandidate {
    /// The counterparty account.
    pub subaccount: SubaccountId,
    /// Instrument of the position that can offset the bankrupt one.
    pub symbol: Symbol,
    /// Their signed lots.
    pub signed_lots: i64,
    /// Their entry price (quote minor per base).
    pub entry_quote_minor: u128,
    /// Current mark.
    pub mark_quote_minor: u128,
}

impl AdlCandidate {
    /// Profit ratio in bps of entry, signed — the ADL ranking key.
    #[must_use]
    pub fn profit_ratio_bps(&self) -> i128 {
        if self.entry_quote_minor == 0 || self.signed_lots == 0 {
            return 0;
        }
        let entry = to_i128(self.entry_quote_minor);
        let mark = to_i128(self.mark_quote_minor);
        // (mark/entry − 1) × 10_000, sign-adjusted by side: a long profits
        // when mark > entry, a short when mark < entry.
        let raw = (mark - entry) * 10_000 / entry;
        raw * i128::from(self.signed_lots.signum())
    }
}

/// Ranks counterparties for auto-deleveraging: **most profitable first**.
///
/// The ranked accounts are force-closed against the bankrupt position at
/// the bankruptcy price. Taking the winners first is the standard: their
/// profit is unrealized and would otherwise be a claim on a bankrupt
/// estate; closing them costs them the least relative to their position.
pub struct AdlRanking;

impl AdlRanking {
    /// Sort candidates by profit ratio (desc), then by subaccount id for
    /// determinism.
    #[must_use]
    pub fn rank(mut candidates: Vec<AdlCandidate>) -> Vec<AdlCandidate> {
        candidates.sort_by(|a, b| {
            b.profit_ratio_bps()
                .cmp(&a.profit_ratio_bps())
                .then_with(|| a.subaccount.cmp(&b.subaccount))
        });
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poc_core::{OptionMarket, PerpMarket};
    use poc_margin::{Mark, Position};

    fn perp() -> Instrument {
        Instrument::Perp(PerpMarket::default())
    }

    fn option_strike(strike: u128) -> Instrument {
        Instrument::Option(OptionMarket {
            strike_quote_minor: strike,
            ..OptionMarket::default()
        })
    }

    fn registry() -> BTreeMap<Symbol, Instrument> {
        let mut m = BTreeMap::new();
        m.insert("BTC-PERP".into(), perp());
        m.insert("BTC-80000-C".into(), option_strike(8_000_000));
        m
    }

    fn marks(spot: u128) -> BTreeMap<String, MarkSet> {
        let set = MarkSet::new("BTC", spot)
            .with_mark(
                "BTC-PERP",
                Mark::Perp {
                    mark_quote_minor_per_base: spot,
                },
            )
            .with_mark(
                "BTC-80000-C",
                Mark::Option {
                    premium_quote_minor_per_base: 400_000,
                    iv: 0.55,
                    tau_years: 0.25,
                },
            );
        let mut m = BTreeMap::new();
        m.insert("BTC".into(), set);
        m
    }

    fn margin_engine() -> PortfolioMarginEngine {
        PortfolioMarginEngine::new()
    }

    #[test]
    fn queue_orders_by_deficit_then_id() {
        let mut q = LiquidationQueue::new();
        q.offer(LiquidationCandidate {
            subaccount: 7,
            equity_quote_minor: 100,
            maintenance_quote_minor: 500,
        });
        q.offer(LiquidationCandidate {
            subaccount: 3,
            equity_quote_minor: -1_000,
            maintenance_quote_minor: 2_000,
        });
        q.offer(LiquidationCandidate {
            subaccount: 9,
            equity_quote_minor: 4_900,
            maintenance_quote_minor: 5_000,
        });
        assert_eq!(q.pop().unwrap().subaccount, 3, "largest deficit first");
        assert_eq!(q.pop().unwrap().subaccount, 7);
        assert_eq!(q.pop().unwrap().subaccount, 9);
        assert!(q.pop().is_none());
        // Healthy accounts are not queued.
        assert!(!q.offer(LiquidationCandidate {
            subaccount: 1,
            equity_quote_minor: 900,
            maintenance_quote_minor: 500
        }));
        // Dedup works.
        assert!(q.offer(LiquidationCandidate {
            subaccount: 2,
            equity_quote_minor: 0,
            maintenance_quote_minor: 100
        }));
        assert!(!q.offer(LiquidationCandidate {
            subaccount: 2,
            equity_quote_minor: 0,
            maintenance_quote_minor: 500
        }));
        q.remove(2);
        assert!(q.is_empty());
    }

    #[test]
    fn deficit_math() {
        let c = LiquidationCandidate {
            subaccount: 1,
            equity_quote_minor: -250,
            maintenance_quote_minor: 1_000,
        };
        assert_eq!(c.deficit(), 1_250);
        let c = LiquidationCandidate {
            subaccount: 1,
            equity_quote_minor: 1_500,
            maintenance_quote_minor: 1_000,
        };
        assert_eq!(c.deficit(), 0);
    }

    #[test]
    fn insurance_fund_accounting() {
        let mut f = InsuranceFund::new(1_000_000);
        f.credit_penalty(500, 1);
        assert_eq!(f.balance(), 1_000_500);
        assert!(f.can_absorb(400_000));
        // Absorbing more than the balance: the loss is still recorded but
        // the return signals the fund is exhausted (ADL territory).
        assert!(!f.absorb(1_200_000, 2));
        assert!(!f.can_absorb(1), "fund went negative");
        assert_eq!(f.balance(), -199_500);
        let (penalties, absorbed, bankruptcies) = f.stats();
        assert_eq!((penalties, absorbed, bankruptcies), (500, 1_200_000, 1));
        // A healthy absorb returns true.
        let mut g = InsuranceFund::new(1_000);
        assert!(g.absorb(400, 3));
        assert_eq!(g.balance(), 600);
    }

    #[test]
    fn healthy_account_has_no_plan() {
        let account = MarginAccount::new(1, 1_000_000);
        let planner = LiquidationPlanner::new(LiquidationParams::default());
        assert!(planner
            .plan(
                1,
                &account,
                &registry(),
                &marks(8_000_000),
                &margin_engine()
            )
            .is_none());
    }

    #[test]
    fn overleveraged_long_gets_partially_liquidated() {
        // Long 1.0 BTC at $80k, cash just under maintenance.
        let mut account = MarginAccount::new(1, 299_000); // maintenance = 300_000
        let mut pos = Position::flat("BTC-PERP");
        let _ = pos.apply_fill(&perp(), Side::Bid, 1000, 8_000_000);
        account.positions.insert("BTC-PERP".into(), pos);

        let planner = LiquidationPlanner::new(LiquidationParams {
            penalty_bps: 125,
            restoration_buffer_bps: 2_000,
            bankruptcy_buffer: 0,
        });
        let plan = planner
            .plan(
                1,
                &account,
                &registry(),
                &marks(8_000_000),
                &margin_engine(),
            )
            .expect("must plan");

        assert_eq!(plan.actions.len(), 1, "single leg");
        let a = &plan.actions[0];
        assert_eq!(a.closing_side, Side::Ask, "closing a long sells");
        assert!(
            a.penalized_price_quote_minor < 8_000_000,
            "penalized below mark"
        );
        assert!(a.lots < 1000, "partial: {}/1000 closed", a.lots);
        assert!(!plan.bankrupt);
        // Restored above target.
        assert!(
            plan.projected_equity_after >= 0,
            "equity after = {}",
            plan.projected_equity_after
        );
    }

    #[test]
    fn bankrupt_account_flagged_for_adl() {
        // Long 1.0 BTC at $80k with near-zero cash and a crash to $50k:
        // equity ≈ −$30k → bankrupt after full closure.
        let mut account = MarginAccount::new(1, 1_000);
        let mut pos = Position::flat("BTC-PERP");
        let _ = pos.apply_fill(&perp(), Side::Bid, 1000, 8_000_000);
        account.positions.insert("BTC-PERP".into(), pos);

        let planner = LiquidationPlanner::new(LiquidationParams::default());
        let plan = planner
            .plan(
                1,
                &account,
                &registry(),
                &marks(5_000_000),
                &margin_engine(),
            )
            .expect("must plan");
        assert!(plan.bankrupt, "plan: {plan:?}");
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].lots, 1000, "everything closed");
        assert!(plan.projected_equity_after < 0);
    }

    #[test]
    fn penalized_price_directions() {
        assert_eq!(penalize(8_000_000, Side::Ask, 125), 7_900_000);
        assert_eq!(penalize(8_000_000, Side::Bid, 125), 8_100_000);
        assert_eq!(restore_target(300_000, 2_000), 360_000);
    }

    #[test]
    fn adl_ranking_most_profitable_first() {
        let candidates = vec![
            AdlCandidate {
                subaccount: 1,
                symbol: "BTC-PERP".into(),
                signed_lots: 500,
                entry_quote_minor: 7_000_000,
                mark_quote_minor: 8_000_000,
            },
            AdlCandidate {
                subaccount: 2,
                symbol: "BTC-PERP".into(),
                signed_lots: -500,
                entry_quote_minor: 9_000_000,
                mark_quote_minor: 8_000_000,
            },
            AdlCandidate {
                subaccount: 3,
                symbol: "BTC-PERP".into(),
                signed_lots: 100,
                entry_quote_minor: 8_000_000,
                mark_quote_minor: 8_000_000,
            },
        ];
        let ranked = AdlRanking::rank(candidates);
        // Long from 70k→80k (+14.3%) and short from 90k→80k (+11.1%) are
        // both winners; flat entry is 0.
        assert_eq!(ranked[0].subaccount, 1);
        assert_eq!(ranked[1].subaccount, 2);
        assert_eq!(ranked[2].subaccount, 3);
        assert!(ranked[0].profit_ratio_bps() > ranked[1].profit_ratio_bps());
        assert_eq!(ranked[2].profit_ratio_bps(), 0);
    }

    #[test]
    fn liquidation_restores_margin_target() {
        // Multi-leg account: perp + long call; drop the spot so both hurt,
        // but keep enough cash that the account is not bankrupt.
        let mut account = MarginAccount::new(1, 900_000);
        let mut pos = Position::flat("BTC-PERP");
        let _ = pos.apply_fill(&perp(), Side::Bid, 1000, 8_000_000);
        account.positions.insert("BTC-PERP".into(), pos);
        let mut opt = Position::flat("BTC-80000-C");
        let _ = opt.apply_fill(&option_strike(8_000_000), Side::Bid, 100, 400_000);
        account.positions.insert("BTC-80000-C".into(), opt);

        // Spot crashes 10%: perp alone loses $8k = 800_000 minor.
        let planner = LiquidationPlanner::new(LiquidationParams {
            penalty_bps: 125,
            restoration_buffer_bps: 0,
            bankruptcy_buffer: 0,
        });
        let marks_crash = marks(7_200_000);
        let plan = planner
            .plan(1, &account, &registry(), &marks_crash, &margin_engine())
            .expect("must plan");
        assert!(!plan.bankrupt, "plan: {plan:?}");
        // After the plan, remaining maintenance must be coverable by equity.
        assert!(
            plan.projected_equity_after >= to_i128(plan.projected_maintenance_after),
            "equity {} vs maintenance {}",
            plan.projected_equity_after,
            plan.projected_maintenance_after
        );
    }
}
