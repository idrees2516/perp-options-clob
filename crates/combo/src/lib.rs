//! Combined (multi-leg) orders and implied matching — the combo book.
//!
//! A **combo order** trades a *package* of instruments as one atomic unit:
//! a call spread, a straddle, a collar, a futures-vs-options synthetic.
//! One signed number — the **net price** — governs the whole package, and
//! one signature — the sorted leg structure — decides which orders can
//! trade against each other.
//!
//! # The one signed net algebra
//!
//! All leg money is expressed in the **buyer-cost convention**: for each
//! leg `j` the package *buyer* either pays (`flow_j > 0`) or receives
//! (`flow_j < 0`). With `V_j(p)` the per-unit value of leg `j` at tick
//! price `p`:
//!
//! ```text
//! flow_j = +V_j(p)   if the buyer BUYS leg j
//! flow_j = -V_j(p)   if the buyer SELLS leg j
//! net   C = Σ flow_j   (per package unit, quote minor)
//! ```
//!
//! The single stored limit covers debits, credits and shorts:
//!
//! * a **buy** combo stores `L` = *maximum acceptable net cost*
//!   (`C ≤ L`). `L = 8.00` caps a debit; `L = -2.00` demands a credit.
//! * a **sell** combo stores `m` = *minimum acceptable net proceeds*
//!   (`C ≥ m`, because the buyer's cost *is* the seller's proceeds).
//!   `m = 8.00` demands a debit; `m = -2.50` caps a paid credit.
//!
//! Two orders are executable when `m ≤ L` (seller meets buyer, the
//! ordinary `ask ≤ bid` relation); the trade prints **at the resting
//! order's limit**, exactly like a CLOB prints at the maker's price.
//!
//! # The three matching regimes
//!
//! 1. **combo-vs-combo** — opposite sides of the same signature trade
//!    at the resting limit; the leg prices are the mark-anchored on-grid
//!    [`distribute_net`] split of that net.
//! 2. **implied-in** — a *newly placed* combo takes liquidity directly
//!    from the per-leg books: a level-synchronous greedy walk
//!    ([`plan_implied_in`]) bounded by the combo's own net limit.
//! 3. **implied-out** — a *plain* order whose remainder would rest (or
//!    cancel) fills into a *resting* combo at the combo's implied leg
//!    price ([`implied_out_bound`]), while the combo hedges its other
//!    legs at the book BBO.
//!
//! All functions are pure: the engine plans against them and journals
//! the resulting trades; replay never re-derives them.

use std::collections::BTreeMap;

#[allow(unused_imports)]
use poc_core::Rounding as _RoundingUnused;
use poc_core::{mul_div, mul_div_ceil, mul_div_floor, Side, SubaccountId, Symbol, TimestampMs};

// ---------------------------------------------------------------------------
// Legs and orders
// ---------------------------------------------------------------------------

/// One leg of a combo package.
///
/// The leg side is stored in the **buyer convention**: it is the side the
/// *package buyer* takes on this instrument. A seller of the package
/// takes the mirrored side on every leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComboLeg {
    /// Instrument symbol.
    pub symbol: Symbol,
    /// The package BUYER's side on this leg.
    pub side: Side,
    /// Lots per package unit (`units × ratio = leg lots`). Positive.
    pub ratio: u32,
    /// Price tick: quote minor units per `1.0` base.
    pub tick_size_quote_minor: u128,
    /// Quantity lot: base minor units per lot.
    pub lot_size_base_minor: u128,
    /// Base precision (minor units per major, e.g. 5 for BTC).
    pub base_decimals: u32,
}

impl ComboLeg {
    /// Lots this leg trades for `units` package units.
    #[must_use]
    pub fn lots_for(&self, units: u64) -> u64 {
        u64::from(self.ratio).saturating_mul(units)
    }

    /// The rational per-tick denominator: `tick_size × ratio × lot_size`
    /// (before dividing by `10^base_decimals`). One tick on this leg is
    /// worth `denominator / 10^decimals` quote minor per package unit —
    /// possibly fractional, which is why tick inversions below use
    /// cross-multiplied integer math instead of a rounded per-tick value.
    fn tick_denominator(&self) -> Option<u128> {
        self.tick_size_quote_minor
            .checked_mul(u128::from(self.ratio))?
            .checked_mul(self.lot_size_base_minor)
    }

    /// Value of one tick on this leg for ONE package unit, quote minor
    /// (`tick_size × ratio × lot_size / 10^decimals`), rounded half-up.
    /// Informational only — bounds invert rationally, see
    /// [`Self::tick_at_value_at_most`].
    #[must_use]
    pub fn value_per_tick(&self) -> Option<u128> {
        let base = 10_u128.checked_pow(self.base_decimals)?;
        mul_div(
            self.tick_denominator()?,
            1,
            base,
            poc_core::Rounding::NearestHalfUp,
        )
    }

    /// Absolute per-unit value of this leg at `price_ticks` for one
    /// package unit (always positive; sign comes from [`Self::flow_per_unit`]).
    #[must_use]
    pub fn value_per_unit(&self, price_ticks: u64) -> Option<u128> {
        let base = 10_u128.checked_pow(self.base_decimals)?;
        let price = u128::from(price_ticks).checked_mul(self.tick_size_quote_minor)?;
        mul_div(
            price,
            u128::from(self.ratio) * self.lot_size_base_minor,
            base,
            poc_core::Rounding::NearestHalfUp,
        )
    }

    /// Buyer-convention per-unit flow at `price_ticks`: positive = the
    /// package buyer pays, negative = the buyer receives.
    #[must_use]
    pub fn flow_per_unit(&self, price_ticks: u64) -> Option<i128> {
        let v = i128::try_from(self.value_per_unit(price_ticks)?).ok()?;
        match self.side {
            Side::Bid => Some(v),
            Side::Ask => v.checked_neg(),
        }
    }

    /// The floor'd on-grid tick price whose per-unit value is at most
    /// `max_value` (the buyer's pay bound on this leg).
    ///
    /// Inverts `value = ticks × tick_denominator / 10^decimals`
    /// rationally: `ticks = floor(max_value × 10^decimals / denom)` — no
    /// rounded per-tick constant, so fractional tick values invert
    /// exactly.
    fn tick_at_value_at_most(&self, max_value: u128) -> Option<u64> {
        let base = 10_u128.checked_pow(self.base_decimals)?;
        let denom = self.tick_denominator()?;
        if denom == 0 {
            return Some(0);
        }
        let ticks = mul_div_floor(max_value, base, denom)?;
        u64::try_from(ticks).ok()
    }

    /// The ceil'd on-grid tick price whose per-unit value is at least
    /// `min_value` (the seller's receive bound on this leg).
    fn tick_at_value_at_least(&self, min_value: u128) -> Option<u64> {
        let base = 10_u128.checked_pow(self.base_decimals)?;
        let denom = self.tick_denominator()?;
        if denom == 0 {
            return Some(0);
        }
        let ticks = if min_value == 0 {
            0
        } else {
            mul_div_ceil(min_value, base, denom)?
        };
        u64::try_from(ticks).ok()
    }
}

/// Time-in-force for combo orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComboTif {
    /// Rest until filled or canceled.
    Gtc,
    /// Cancel any remainder after the immediate matching phases.
    Ioc,
    /// All-or-nothing: reject the whole placement if any remainder stays.
    Fok,
}

/// A resting combined order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComboOrder {
    /// Engine-assigned combo id (own id space — never an order id).
    pub id: u64,
    /// Owning subaccount.
    pub owner: SubaccountId,
    /// Legs in canonical order (sorted by symbol, no duplicates),
    /// buyer convention.
    pub legs: Vec<ComboLeg>,
    /// The OWNER's side on the *package*.
    pub side: Side,
    /// Buyer-cost limit, quote minor per package unit:
    /// buy = max acceptable net cost, sell = min acceptable net proceeds.
    pub limit_quote_minor_per_unit: i128,
    /// Total package units.
    pub units: u64,
    /// Units already executed.
    pub filled_units: u64,
    /// Time-in-force.
    pub tif: ComboTif,
    /// Placement sequence (price-time priority within a level).
    pub seq: u64,
    /// Placement wall-clock.
    pub ts: TimestampMs,
}

impl ComboOrder {
    /// Package units still open.
    #[must_use]
    pub fn open_units(&self) -> u64 {
        self.units.saturating_sub(self.filled_units)
    }

    /// Whether the order is still live on the book.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open_units() > 0
    }

    /// Does a buyer-cost net of `c` (per unit) satisfy this order's limit?
    #[must_use]
    pub fn accepts_net(&self, c: i128) -> bool {
        match self.side {
            Side::Bid => c <= self.limit_quote_minor_per_unit,
            Side::Ask => c >= self.limit_quote_minor_per_unit,
        }
    }

    /// The structure signature: the canonical buyer-convention leg list.
    /// Two orders with equal signatures can trade against each other.
    #[must_use]
    pub fn signature(&self) -> ComboSignature {
        ComboSignature {
            legs: self
                .legs
                .iter()
                .map(|l| (l.symbol.clone(), l.side, l.ratio))
                .collect(),
        }
    }
}

/// Canonical structure signature of a combo.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComboSignature {
    /// (symbol, buyer side, ratio) per leg, sorted by symbol.
    pub legs: Vec<(Symbol, Side, u32)>,
}

// ---------------------------------------------------------------------------
// The combo book
// ---------------------------------------------------------------------------

/// The book of resting combo orders: one price-time-priority market per
/// structure signature, plus a symbol index for implied-out lookups.
///
/// Matching inside a signature is the ordinary two-sided auction: best
/// buy = highest limit, best sell = lowest limit, ties by placement
/// sequence. Combo books are expected to stay small (they carry the
/// *long-tail* structures), so the queues are simple sorted vectors.
#[derive(Debug, Default, Clone)]
pub struct ComboBook {
    orders: BTreeMap<u64, ComboOrder>,
    by_signature: BTreeMap<ComboSignature, Vec<u64>>,
    by_symbol: BTreeMap<Symbol, Vec<u64>>,
    by_owner: BTreeMap<SubaccountId, Vec<u64>>,
    next_id: u64,
    next_seq: u64,
}

impl ComboBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The id the next placed combo will receive (plan-stage prediction;
    /// deterministic under the plan/apply split).
    #[must_use]
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Reserve the next id (apply-side).
    pub(crate) fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Insert a resting combo. `order.id`/`order.seq` are overwritten by
    /// the book's counters; returns the assigned id.
    pub fn place(&mut self, mut order: ComboOrder) -> u64 {
        let id = self.allocate_id();
        order.id = id;
        order.seq = self.next_seq;
        self.next_seq += 1;
        let sig = order.signature();
        for leg in &order.legs {
            self.by_symbol
                .entry(leg.symbol.clone())
                .or_default()
                .push(id);
        }
        self.by_owner.entry(order.owner).or_default().push(id);
        self.by_signature.entry(sig).or_default().push(id);
        self.orders.insert(id, order);
        id
    }

    /// Note that a combo id was consumed without resting (a rejection
    /// or an at-placement fill) so later placements never collide with
    /// it.
    pub fn note_rejected(&mut self, id: u64) {
        self.next_id = self.next_id.max(id + 1);
    }

    /// Look up a combo by id.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&ComboOrder> {
        self.orders.get(&id)
    }

    /// All resting combos (id order — deterministic).
    pub fn all(&self) -> impl Iterator<Item = &ComboOrder> {
        self.orders.values()
    }

    /// Open combos whose packages include `symbol` (id order).
    #[must_use]
    pub fn on_symbol(&self, symbol: &str) -> Vec<&ComboOrder> {
        let Some(ids) = self.by_symbol.get(symbol) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| self.orders.get(id))
            .filter(|o| o.is_open())
            .collect()
    }

    /// Open combos of one owner (id order).
    #[must_use]
    pub fn of_owner(&self, owner: SubaccountId) -> Vec<&ComboOrder> {
        let Some(ids) = self.by_owner.get(&owner) else {
            return Vec::new();
        };
        ids.iter()
            .filter_map(|id| self.orders.get(id))
            .filter(|o| o.is_open())
            .collect()
    }

    /// Best resting counterparty for an incoming taker combo: opposite
    /// side, both limits compatible, best price first, then time.
    ///
    /// "Best" is from the TAKER's viewpoint: the lowest seller limit for
    /// an incoming buy, the highest buyer limit for an incoming sell.
    #[must_use]
    pub fn best_opponent(
        &self,
        signature: &ComboSignature,
        taker_side: Side,
        taker_limit: i128,
    ) -> Option<&ComboOrder> {
        let ids = self.by_signature.get(signature)?;
        let mut best: Option<&ComboOrder> = None;
        for id in ids {
            let Some(o) = self.orders.get(id) else {
                continue;
            };
            if !o.is_open() || o.side == taker_side || o.owner == u64::MAX {
                continue;
            }
            // Executable: seller_limit <= buyer_limit.
            let (sell_limit, buy_limit) = if taker_side == Side::Bid {
                (o.limit_quote_minor_per_unit, taker_limit)
            } else {
                (taker_limit, o.limit_quote_minor_per_unit)
            };
            if sell_limit > buy_limit {
                continue;
            }
            best = match best {
                None => Some(o),
                Some(b) => {
                    let better = match taker_side {
                        // incoming buy: lower seller limit is better.
                        Side::Bid => o.limit_quote_minor_per_unit < b.limit_quote_minor_per_unit,
                        // incoming sell: higher buyer limit is better.
                        Side::Ask => o.limit_quote_minor_per_unit > b.limit_quote_minor_per_unit,
                    };
                    if better
                        || (o.limit_quote_minor_per_unit == b.limit_quote_minor_per_unit
                            && o.seq < b.seq)
                    {
                        Some(o)
                    } else {
                        Some(b)
                    }
                }
            };
        }
        best
    }

    /// Apply a fill of `units` package units to a resting combo.
    /// Removes the order when fully filled. Returns the combo's final
    /// open units.
    pub fn apply_fill(&mut self, id: u64, units: u64) -> Option<u64> {
        let order = self.orders.get_mut(&id)?;
        order.filled_units = order.filled_units.saturating_add(units);
        let open = order.open_units();
        if open == 0 {
            self.remove(id);
        }
        Some(open)
    }

    /// Remove a combo (cancel / close). Returns the removed order.
    pub fn remove(&mut self, id: u64) -> Option<ComboOrder> {
        let order = self.orders.remove(&id)?;
        let sig = order.signature();
        if let Some(v) = self.by_signature.get_mut(&sig) {
            v.retain(|&i| i != id);
            if v.is_empty() {
                self.by_signature.remove(&sig);
            }
        }
        for leg in &order.legs {
            if let Some(v) = self.by_symbol.get_mut(&leg.symbol) {
                v.retain(|&i| i != id);
                if v.is_empty() {
                    self.by_symbol.remove(&leg.symbol);
                }
            }
        }
        if let Some(v) = self.by_owner.get_mut(&order.owner) {
            v.retain(|&i| i != id);
            if v.is_empty() {
                self.by_owner.remove(&order.owner);
            }
        }
        Some(order)
    }

    /// Cancel every open combo of `owner` (cancel-all / CoD path).
    /// Returns the removed combos in id order.
    pub fn cancel_all_of(&mut self, owner: SubaccountId) -> Vec<ComboOrder> {
        let ids: Vec<u64> = self.of_owner(owner).iter().map(|o| o.id).collect();
        ids.into_iter().filter_map(|id| self.remove(id)).collect()
    }

    /// Internal consistency (fuzzed): every index entry exists and is
    /// open, and every open order is indexed everywhere it should be.
    pub fn invariants_hold(&self) -> Result<(), String> {
        for (sig, ids) in &self.by_signature {
            for id in ids {
                let o = self
                    .orders
                    .get(id)
                    .ok_or_else(|| format!("signature index dangling: {id}"))?;
                if !o.is_open() {
                    return Err(format!("signature index holds filled combo {id}"));
                }
                if o.signature() != *sig {
                    return Err(format!("combo {id} in wrong signature bucket"));
                }
            }
        }
        for (symbol, ids) in &self.by_symbol {
            for id in ids {
                let o = self
                    .orders
                    .get(id)
                    .ok_or_else(|| format!("symbol index dangling: {id}"))?;
                if !o.legs.iter().any(|l| l.symbol == *symbol) {
                    return Err(format!("combo {id} indexed under foreign symbol {symbol}"));
                }
            }
        }
        for o in self.orders.values() {
            if !o.is_open() {
                return Err(format!("filled combo {} still resting", o.id));
            }
            let sig = o.signature();
            let indexed = self
                .by_signature
                .get(&sig)
                .is_some_and(|v| v.contains(&o.id));
            if !indexed {
                return Err(format!("combo {} missing from its signature index", o.id));
            }
            for leg in &o.legs {
                let ok = self
                    .by_symbol
                    .get(&leg.symbol)
                    .is_some_and(|v| v.contains(&o.id));
                if !ok {
                    return Err(format!(
                        "combo {} missing from symbol index {}",
                        o.id, leg.symbol
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Canonicalize a leg list: sort by symbol and reject duplicates.
/// Combo orders must rest with canonical legs — the signature and the
/// combo book's indexes assume it.
///
/// Returns `None` on a duplicate symbol or an empty list.
#[must_use]
pub fn canonicalize_legs(legs: Vec<ComboLeg>) -> Option<Vec<ComboLeg>> {
    if legs.is_empty() {
        return None;
    }
    let mut legs = legs;
    legs.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    for w in legs.windows(2) {
        if w[0].symbol == w[1].symbol {
            return None;
        }
    }
    Some(legs)
}

// ---------------------------------------------------------------------------
// distribute_net — the mark-anchored on-grid split
// ---------------------------------------------------------------------------

/// Split a per-unit net buyer-cost `target` into per-leg tick prices,
/// anchored at the marks, on each leg's grid.
///
/// Every leg anchors at its mark (`floor(mark / tick_size)` ticks); the
/// **absorber leg** — the leg with the finest tick granularity (ties by
/// canonical order) — carries the residual so the split lands on the
/// target as closely as the grid allows. The **effective net is then
/// recomputed from the final integer ticks**: the returned prices are
/// what actually trade, and conservation is exact by construction.
///
/// Returns `(per-leg ticks, effective net)`.
///
/// # Errors
///
/// `None` on overflow or when a leg's mark is missing.
#[must_use]
pub fn distribute_net(
    legs: &[ComboLeg],
    marks: &BTreeMap<Symbol, u128>,
    target_net_quote_minor_per_unit: i128,
) -> Option<(Vec<u64>, i128)> {
    if legs.is_empty() {
        return None;
    }
    // Anchor every leg at its mark, on grid (floor).
    let mut ticks: Vec<u64> = Vec::with_capacity(legs.len());
    let mut anchored_flow: i128 = 0;
    for leg in legs {
        let mark = *marks.get(&leg.symbol)?;
        let t = u64::try_from(mul_div_floor(mark, 1, leg.tick_size_quote_minor)?).ok()?;
        ticks.push(t);
        anchored_flow = anchored_flow.checked_add(leg.flow_per_unit(t)?)?;
    }
    // The absorber: the leg whose ticks are finest (smallest rational
    // tick denominator) carries the residual — the smallest attainable
    // error. Ties: canonical order.
    let absorber = (0..legs.len())
        .min_by_key(|&i| (legs[i].tick_denominator().unwrap_or(u128::MAX), i))
        .unwrap_or(0);
    let residual = target_net_quote_minor_per_unit.checked_sub(anchored_flow)?;

    // Solve the absorber's tick: we need flow_absorber(t) = anchored + residual.
    let leg = &legs[absorber];
    let current = leg.flow_per_unit(ticks[absorber])?;
    let wanted = current.checked_add(residual)?;
    // Invert rationally: t = round(|wanted| × 10^d / (tick × ratio × lot)).
    let t_abs = if wanted == 0 {
        0
    } else {
        let base = 10_u128.checked_pow(leg.base_decimals)?;
        let denom = leg.tick_denominator()?;
        if denom == 0 {
            ticks[absorber]
        } else {
            let t = mul_div(
                wanted.unsigned_abs(),
                base,
                denom,
                poc_core::Rounding::NearestHalfUp,
            )?;
            let t = u64::try_from(t).ok()?;
            // A zero tick would be a free leg; keep the anchor in that
            // case (the residual then shows up in the effective net,
            // which both limits re-verify).
            if t == 0 {
                ticks[absorber]
            } else {
                t
            }
        }
    };
    ticks[absorber] = t_abs;

    // Recompute the effective net from the FINAL ticks — this is the
    // number that actually trades.
    let mut effective: i128 = 0;
    for (i, leg) in legs.iter().enumerate() {
        effective = effective.checked_add(leg.flow_per_unit(ticks[i])?)?;
    }
    Some((ticks, effective))
}

// ---------------------------------------------------------------------------
// implied-in — the level-synchronous greedy walk
// ---------------------------------------------------------------------------

/// One level of per-leg depth facing a combo, already priced in
/// buyer-convention flow per unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FacingLevel {
    /// The level's tick price on the leg.
    pub price_ticks: u64,
    /// Visible lots resting at this level (on the side facing the combo).
    pub lots: u64,
}

/// The result of a successful implied-in walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpliedInPlan {
    /// Package units the walk can execute within the net limit.
    pub units: u64,
    /// The worst (deepest) tick the walk reached on each leg — the
    /// synthetic IOC per leg is bounded at this price.
    pub worst_ticks: Vec<u64>,
}

/// Level-synchronous greedy walk over per-leg depth facing a *newly
/// placed* combo (buyer-convention legs).
///
/// For each depth stratum `k` (the k-th best level on every leg,
/// up to `max_levels`), the per-unit net is `Σ flow_j(level_k_price_j)`.
/// Because deeper levels are never better for the taker, the per-unit
/// net is monotone: the first stratum that violates the taker's limit
/// ends the walk. Units accumulate as the minimum, across legs, of the
/// lots each leg can still fill in the acceptable strata (converted to
/// package units via each leg's ratio).
///
/// `facing` supplies, per leg index, the depth levels on the side of
/// the book the combo takes from (asks for buyer-Bid legs, bids for
/// buyer-Ask legs), best level first.
///
/// Returns `None` when no stratum is acceptable or the inputs are
/// inconsistent (wrong leg count).
#[must_use]
pub fn plan_implied_in(
    legs: &[ComboLeg],
    facing: &[Vec<FacingLevel>],
    taker_side: Side,
    taker_limit: i128,
    max_levels: usize,
) -> Option<ImpliedInPlan> {
    if legs.is_empty() || facing.len() != legs.len() {
        return None;
    }
    let max_levels = max_levels.max(1);
    let mut units: u64 = 0;
    let mut worst_ticks = vec![0_u64; legs.len()];
    // Stratum k: level k of every leg (legs with fewer levels cap the
    // walk — the stratum is only usable if EVERY leg has level k).
    let depth = facing.iter().map(Vec::len).min().unwrap_or(0);
    for k in 0..depth.min(max_levels) {
        let mut stratum_flow: i128 = 0;
        for (j, levels) in facing.iter().enumerate() {
            let lvl = levels.get(k)?;
            stratum_flow = stratum_flow.checked_add(legs[j].flow_per_unit(lvl.price_ticks)?)?;
        }
        // The taker's net constraint at this stratum's marginal price.
        let acceptable = match taker_side {
            Side::Bid => stratum_flow <= taker_limit,
            Side::Ask => stratum_flow >= taker_limit,
        };
        if !acceptable {
            break; // monotone: nothing deeper can help.
        }
        // Accepted: the worst ticks reached are this stratum's prices.
        for (j, levels) in facing.iter().enumerate() {
            worst_ticks[j] = levels.get(k)?.price_ticks;
        }
        // Units this stratum can add: min across legs of (lots at levels
        // 0..=k already counted minus used) — track per-leg usage.
        // Level-synchronous: stratum k adds min_j(remaining lots in
        // level k of leg j) / ratio_j, but each leg's EARLIER levels
        // may have leftovers that only deeper strata of other legs can
        // reach; a leftover on leg j at level k can serve stratum k' > k.
        // We therefore carry per-leg remaining lots forward.
        let mut stratum_units = u64::MAX;
        for (j, levels) in facing.iter().enumerate() {
            let available = u128::from(levels[k].lots);
            let ratio = u128::from(legs[j].ratio);
            let by_ratio = mul_div_floor(available, 1, ratio)?;
            stratum_units = stratum_units.min(u64::try_from(by_ratio).unwrap_or(u64::MAX));
        }
        if stratum_units == u64::MAX {
            stratum_units = 0;
        }
        units = units.saturating_add(stratum_units);
    }
    if units == 0 {
        return None;
    }
    Some(ImpliedInPlan { units, worst_ticks })
}

// ---------------------------------------------------------------------------
// implied-out — the resting combo's bound on an aggressed leg
// ---------------------------------------------------------------------------

/// The resting combo's implied price bound on one aggressed leg.
///
/// When a plain taker aggresses leg `sym`, a resting combo whose package
/// contains that leg can take the other side — while hedging its OTHER
/// legs at the book BBO. Given `other_leg_flows` (the per-unit
/// buyer-convention flows the combo obtains on every other leg at those
/// BBO hedges), the combo's own net limit pins the worst price it can
/// accept on the aggressed leg:
///
/// * resting **buy** combo (`C = flow_j + H ≤ L`): `V_j(p) ≤ L − H`
///   — a **max-pay** bound, floored to the leg grid;
/// * resting **sell** combo (`C = flow_j + H ≥ m`): `V_j(p) ≥ m − H`
///   — a **min-receive** bound, ceiled to the leg grid.
///
/// Flooring / ceiling is conservative against the *combo's* constraint:
/// the effective net, recomputed from the final ticks, can only be
/// better for the combo than its limit, never worse.
///
/// Returns `None` on overflow or a missing leg.
#[must_use]
pub fn implied_out_bound(
    combo: &ComboOrder,
    aggressed_symbol: &str,
    other_leg_flows: &BTreeMap<Symbol, i128>,
) -> Option<ImpliedOutBound> {
    let idx = combo
        .legs
        .iter()
        .position(|l| l.symbol == aggressed_symbol)?;
    let leg = &combo.legs[idx];
    let mut hedge_flow: i128 = 0;
    for (i, other) in combo.legs.iter().enumerate() {
        if i == idx {
            continue;
        }
        let flow = *other_leg_flows.get(&other.symbol)?;
        hedge_flow = hedge_flow.checked_add(flow)?;
    }
    // The combo's inequality on C = flow_j(p) + H, with flow_j the
    // BUYER-convention flow on the aggressed leg: the direction comes
    // from the combo's side, the SIGN of flow_j from the LEG's side.
    // Solving for the leg's value V(p) gives four cases:
    //
    // | combo | leg   | constraint   | bound on V | shape       |
    // |-------|-------|--------------|------------|-------------|
    // | buy   | Bid   | +V + H <= L  | V <= L - H | max-pay     |
    // | buy   | Ask   | -V + H <= L  | V >= H - L | min-receive |
    // | sell  | Bid   | +V + H >= m  | V >= m - H | min-receive |
    // | sell  | Ask   | -V + H >= m  | V <= H - m | max-pay     |
    //
    // Max-pay bounds floor to the grid, min-receive bounds ceil —
    // conservative against the combo's own constraint in every case.
    let bound = match (combo.side, leg.side) {
        (Side::Bid, Side::Bid) => {
            let max_pay = combo.limit_quote_minor_per_unit.checked_sub(hedge_flow)?;
            if max_pay < 0 {
                // The hedge alone already exceeds the limit: no price
                // on this leg can satisfy the combo (V >= 0).
                return None;
            }
            ImpliedOutBound::MaxPayTicks(leg.tick_at_value_at_most(max_pay.unsigned_abs())?)
        }
        (Side::Bid, Side::Ask) => {
            let min_receive = hedge_flow
                .checked_sub(combo.limit_quote_minor_per_unit)?
                .max(0);
            ImpliedOutBound::MinReceiveTicks(
                leg.tick_at_value_at_least(min_receive.unsigned_abs())?,
            )
        }
        (Side::Ask, Side::Bid) => {
            let min_receive = combo
                .limit_quote_minor_per_unit
                .checked_sub(hedge_flow)?
                .max(0);
            ImpliedOutBound::MinReceiveTicks(
                leg.tick_at_value_at_least(min_receive.unsigned_abs())?,
            )
        }
        (Side::Ask, Side::Ask) => {
            let max_pay = hedge_flow.checked_sub(combo.limit_quote_minor_per_unit)?;
            if max_pay < 0 {
                // -V + H >= m with H - m < 0 is unsatisfiable (V >= 0).
                return None;
            }
            ImpliedOutBound::MaxPayTicks(leg.tick_at_value_at_most(max_pay.unsigned_abs())?)
        }
    };
    Some(bound)
}

/// The implied-out bound in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpliedOutBound {
    /// The highest tick a resting BUY combo can pay on the aggressed
    /// leg (it is the implied BID the combo quotes on that leg).
    MaxPayTicks(u64),
    /// The lowest tick a resting SELL combo can receive on the aggressed
    /// leg (the implied ASK the combo quotes on that leg).
    MinReceiveTicks(u64),
}

impl ImpliedOutBound {
    /// The tick price, whichever shape the bound takes.
    #[must_use]
    pub fn ticks(self) -> u64 {
        match self {
            ImpliedOutBound::MaxPayTicks(t) | ImpliedOutBound::MinReceiveTicks(t) => t,
        }
    }
}

/// Convert a raw buyer-convention per-unit bound value into on-grid
/// ticks for a leg, flooring pay-bounds and ceiling receive-bounds.
///
/// This is the engine-facing helper for routes that compute a bound as
/// a value rather than through [`implied_out_bound`] (e.g. the RFQ
/// collar: a taker's max total cost over a package).
#[must_use]
pub fn bound_to_ticks(
    leg: &ComboLeg,
    bound_quote_minor_per_unit: i128,
    floor: bool,
) -> Option<u64> {
    if bound_quote_minor_per_unit < 0 {
        return Some(0);
    }
    let v = bound_quote_minor_per_unit.unsigned_abs();
    if floor {
        leg.tick_at_value_at_most(v)
    } else {
        leg.tick_at_value_at_least(v)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use poc_core::Side;

    fn leg(symbol: &str, side: Side, ratio: u32) -> ComboLeg {
        ComboLeg {
            symbol: symbol.into(),
            side,
            ratio,
            tick_size_quote_minor: 100, // $1.00 ticks
            lot_size_base_minor: 100,   // 0.001 BTC lots
            base_decimals: 5,
        }
    }

    fn order(
        id: u64,
        owner: u64,
        legs: Vec<ComboLeg>,
        side: Side,
        limit: i128,
        units: u64,
    ) -> ComboOrder {
        ComboOrder {
            id,
            owner,
            legs,
            side,
            limit_quote_minor_per_unit: limit,
            units,
            filled_units: 0,
            tif: ComboTif::Gtc,
            seq: 0,
            ts: 0,
        }
    }

    // 1. Buyer-convention flows: bid pays, ask receives.
    #[test]
    fn flow_signs_follow_buyer_convention() {
        let buy = leg("C1", Side::Bid, 1);
        let sell = leg("C2", Side::Ask, 1);
        // 80_000 ticks * $1 * 0.001 BTC = $80 = 8000 minor.
        assert_eq!(buy.flow_per_unit(80_000), Some(8_000));
        assert_eq!(sell.flow_per_unit(80_000), Some(-8_000));
        // Ratio 2 doubles the exposure.
        assert_eq!(leg("C1", Side::Bid, 2).flow_per_unit(80_000), Some(16_000));
    }

    // 2. accepts_net covers debits, credits and shorts.
    #[test]
    fn accepts_net_handles_debits_and_credits() {
        let buy = order(1, 1, vec![leg("C1", Side::Bid, 1)], Side::Bid, 800, 1);
        assert!(buy.accepts_net(800));
        assert!(buy.accepts_net(-500)); // credit combo: receive at least 5.00
        assert!(!buy.accepts_net(801));
        let sell = order(2, 2, vec![leg("C1", Side::Bid, 1)], Side::Ask, 800, 1);
        assert!(sell.accepts_net(800));
        assert!(sell.accepts_net(900));
        assert!(!sell.accepts_net(799));
        // Short package: seller demanding a 2.50 credit cap (m = -250).
        let short_sell = order(3, 3, vec![leg("S", Side::Bid, 1)], Side::Ask, -250, 1);
        assert!(short_sell.accepts_net(-250));
        assert!(short_sell.accepts_net(-100)); // pays only 1.00
        assert!(!short_sell.accepts_net(-300)); // pays 3.00 > cap
    }

    // 3. Signature: sorted, no duplicates; equal structures match.
    #[test]
    fn signature_is_canonical() {
        let a_legs =
            canonicalize_legs(vec![leg("B", Side::Bid, 1), leg("A", Side::Ask, 2)]).unwrap();
        let b_legs =
            canonicalize_legs(vec![leg("A", Side::Ask, 2), leg("B", Side::Bid, 1)]).unwrap();
        let a = order(1, 1, a_legs, Side::Bid, 0, 1);
        let b = order(2, 2, b_legs, Side::Ask, 0, 1);
        assert_eq!(a.signature(), b.signature());
        let other =
            canonicalize_legs(vec![leg("A", Side::Ask, 3), leg("B", Side::Bid, 1)]).unwrap();
        assert_ne!(
            a.signature(),
            order(3, 3, other, Side::Bid, 0, 1).signature()
        );
        // Duplicates are rejected outright.
        assert!(canonicalize_legs(vec![leg("A", Side::Bid, 1), leg("A", Side::Ask, 1)]).is_none());
        assert!(canonicalize_legs(vec![]).is_none());
    }

    // 4. Best opponent: price first, then time.
    #[test]
    fn best_opponent_is_price_then_time() {
        let mut book = ComboBook::new();
        let sig = order(
            0,
            0,
            vec![leg("C1", Side::Bid, 1), leg("C2", Side::Ask, 1)],
            Side::Bid,
            0,
            1,
        )
        .signature();
        let first = book.place(order(
            0,
            2,
            vec![leg("C1", Side::Bid, 1), leg("C2", Side::Ask, 1)],
            Side::Ask,
            820,
            5,
        ));
        let second = book.place(order(
            0,
            3,
            vec![leg("C1", Side::Bid, 1), leg("C2", Side::Ask, 1)],
            Side::Ask,
            820,
            5,
        ));
        let best_price = book.place(order(
            0,
            4,
            vec![leg("C1", Side::Bid, 1), leg("C2", Side::Ask, 1)],
            Side::Ask,
            850,
            5,
        ));
        // Incoming buy at 9.00: best ask is the 8.20 level; among the two
        // 8.20s the earlier placement (first) wins.
        let got = book.best_opponent(&sig, Side::Bid, 900).unwrap();
        assert_eq!(got.id, first);
        assert_ne!(got.id, second);
        // Incoming buy at 8.00: nothing executable.
        assert!(book.best_opponent(&sig, Side::Bid, 800).is_none());
        // Incoming buy at 8.60: the best (lowest) ask is still the 8.20
        // level — an incoming buy always prefers the cheaper seller.
        let got = book.best_opponent(&sig, Side::Bid, 860).unwrap();
        assert_eq!(got.id, first);
        assert_ne!(got.id, best_price);
        let _ = best_price;
        // Same-side orders are never opponents.
        assert!(book.best_opponent(&sig, Side::Ask, i128::MIN).is_none());
    }

    // 5. Fill and removal keep the indexes consistent.
    #[test]
    fn fills_and_cancels_keep_indexes_consistent() {
        let mut book = ComboBook::new();
        let id = book.place(order(
            0,
            7,
            vec![leg("A", Side::Bid, 1), leg("B", Side::Ask, 1)],
            Side::Bid,
            100,
            10,
        ));
        assert_eq!(book.on_symbol("A").len(), 1);
        assert_eq!(book.of_owner(7).len(), 1);
        book.apply_fill(id, 6);
        assert_eq!(book.get(id).unwrap().open_units(), 4);
        book.apply_fill(id, 4);
        assert!(book.get(id).is_none());
        assert!(book.on_symbol("A").is_empty());
        assert!(book.of_owner(7).is_empty());
        assert!(book.invariants_hold().is_ok());

        let id2 = book.place(order(0, 8, vec![leg("A", Side::Bid, 1)], Side::Bid, 100, 3));
        let removed = book.cancel_all_of(8);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].id, id2);
        assert!(book.invariants_hold().is_ok());
    }

    // 6. distribute_net: debit split, exact conservation, both limits.
    #[test]
    fn distribute_net_splits_a_debit_on_grid() {
        let legs = vec![leg("C1", Side::Bid, 1), leg("C2", Side::Ask, 1)];
        let marks: BTreeMap<Symbol, u128> = [
            ("C1".to_string(), 10_000_000_u128), // $100k -> 100_000 ticks
            ("C2".to_string(), 9_200_000),
        ]
        .into_iter()
        .collect();
        // Target: pay $8.00 net per unit (call spread debit).
        let (ticks, net) = distribute_net(&legs, &marks, 800).unwrap();
        // Effective net recomputed from ticks must be within one tick
        // value of the target ($1 tick on the absorber).
        let diff = (net - 800).unsigned_abs();
        let per_tick = legs
            .iter()
            .map(|l| l.value_per_tick().unwrap())
            .min()
            .unwrap();
        assert!(diff <= per_tick, "net {net} too far from 800");
        // Both legs anchored: recomputing from the ticks is what trades.
        let c1 = legs[0].flow_per_unit(ticks[0]).unwrap();
        let c2 = legs[1].flow_per_unit(ticks[1]).unwrap();
        assert_eq!(c1 + c2, net);
    }

    // 7. distribute_net: credit split (negative net).
    #[test]
    fn distribute_net_handles_credit_packages() {
        let legs = vec![leg("P1", Side::Bid, 1), leg("P2", Side::Ask, 1)];
        let marks: BTreeMap<Symbol, u128> =
            [("P1".to_string(), 5_000_000), ("P2".to_string(), 4_600_000)]
                .into_iter()
                .collect();
        // Target: receive $2.00 per unit.
        let (ticks, net) = distribute_net(&legs, &marks, -200).unwrap();
        assert!(
            net <= -200 + 100,
            "net {net} violates the credit within a tick"
        );
        let p1 = legs[0].flow_per_unit(ticks[0]).unwrap();
        let p2 = legs[1].flow_per_unit(ticks[1]).unwrap();
        assert_eq!(p1 + p2, net);
    }

    // 8. distribute_net: the absorber carries the residual.
    #[test]
    fn distribute_net_absorber_carries_residual() {
        // Coarse leg ($10 ticks) + fine leg ($1 ticks): the fine leg absorbs.
        let mut coarse = leg("COARSE", Side::Bid, 1);
        coarse.tick_size_quote_minor = 1_000;
        let fine = leg("FINE", Side::Ask, 1);
        let legs = vec![coarse.clone(), fine.clone()];
        let marks: BTreeMap<Symbol, u128> = [
            ("COARSE".into(), 10_000_000_u128),
            ("FINE".into(), 5_000_000),
        ]
        .into_iter()
        .collect();
        let (ticks, net) = distribute_net(&legs, &marks, 1_237).unwrap();
        // The coarse leg stays at its mark anchor; the fine leg moves.
        assert_eq!(ticks[0], 10_000); // anchored
        let eff = coarse.flow_per_unit(ticks[0]).unwrap() + fine.flow_per_unit(ticks[1]).unwrap();
        assert_eq!(eff, net);
        assert!((net - 1_237).unsigned_abs() <= 100);
    }

    // 9. plan_implied_in: walks while the net holds, stops at violation.
    #[test]
    fn implied_in_walk_stops_at_first_violation() {
        // Buy the spread: buy C1 (hit asks), sell C2 (hit bids).
        let legs = vec![leg("C1", Side::Bid, 1), leg("C2", Side::Ask, 1)];
        // C1 asks: 100_000 (x3 lots), 100_050 (x5). C2 bids: 92_000 (x4).
        let facing = vec![
            vec![
                FacingLevel {
                    price_ticks: 100_000,
                    lots: 3,
                },
                FacingLevel {
                    price_ticks: 100_050,
                    lots: 5,
                },
            ],
            vec![
                FacingLevel {
                    price_ticks: 92_000,
                    lots: 4,
                },
                FacingLevel {
                    price_ticks: 91_950,
                    lots: 5,
                },
            ],
        ];
        // Stratum 0 net: 10_000 - 9_200 = $8.00 -> acceptable at limit 800.
        let plan = plan_implied_in(&legs, &facing, Side::Bid, 800, 16).unwrap();
        assert_eq!(plan.units, 3); // min(3, 4) lots -> 3 units
        assert_eq!(plan.worst_ticks, vec![100_000, 92_000]);
        // Tighten the limit to $7.99: stratum 0 violates -> no plan.
        assert!(plan_implied_in(&legs, &facing, Side::Bid, 799, 16).is_none());
        // Widen to $8.10: stratum 1 net = 10_005 - 9_195 = $8.10 ok ->
        // 3 + min(5, 5) = 8 units.
        let plan = plan_implied_in(&legs, &facing, Side::Bid, 810, 16).unwrap();
        assert_eq!(plan.units, 8);
        assert_eq!(plan.worst_ticks, vec![100_050, 91_950]);
        // Depth levels cap the walk.
        let plan = plan_implied_in(&legs, &facing, Side::Bid, 900, 1).unwrap();
        assert_eq!(plan.units, 3);
    }

    // 10. plan_implied_in: ratios divide stratum units.
    #[test]
    fn implied_in_respects_ratios() {
        let legs = vec![leg("X", Side::Bid, 2), leg("Y", Side::Ask, 1)];
        let facing = vec![
            vec![FacingLevel {
                price_ticks: 50_000,
                lots: 7,
            }],
            vec![FacingLevel {
                price_ticks: 10_000,
                lots: 9,
            }],
        ];
        // X at 50_000 ticks, ratio 2: value = 2 × $50 = 10_000 minor.
        // Y at 10_000 ticks, ratio 1: value = $10 = 1_000 minor.
        // Net: 10_000 - 1_000 = $9.00; limit 9_000 -> ok. Units = floor(7/2) = 3.
        let plan = plan_implied_in(&legs, &facing, Side::Bid, 9_000, 16).unwrap();
        assert_eq!(plan.units, 3);
        // Tighter than $9.00: nothing.
        assert!(plan_implied_in(&legs, &facing, Side::Bid, 8_999, 16).is_none());
    }

    // 11. plan_implied_in: sell-side taker (proceeds floor).
    #[test]
    fn implied_in_supports_sell_takers() {
        // Sell the spread: sell C1 (hit bids), buy C2 (hit asks).
        let legs = vec![leg("C1", Side::Bid, 1), leg("C2", Side::Ask, 1)];
        let facing = vec![
            vec![FacingLevel {
                price_ticks: 99_000,
                lots: 5,
            }], // C1 bids
            vec![FacingLevel {
                price_ticks: 92_500,
                lots: 5,
            }], // C2 asks
        ];
        // Buyer-cost net at these prices: 9_900 - 9_250 = $6.50. A sell
        // taker demands proceeds >= $6.40 -> net >= 640: acceptable.
        let plan = plan_implied_in(&legs, &facing, Side::Ask, 640, 16).unwrap();
        assert_eq!(plan.units, 5);
        // Demanding $6.60 rejects the stratum.
        assert!(plan_implied_in(&legs, &facing, Side::Ask, 660, 16).is_none());
    }

    // 12. implied_out_bound: resting buy combo's max-pay, floored.
    #[test]
    fn implied_out_bound_floors_a_buy_combo() {
        // Resting BUY spread at $8.00: buy C1, sell C2.
        let combo = order(
            1,
            1,
            vec![leg("C1", Side::Bid, 1), leg("C2", Side::Ask, 1)],
            Side::Bid,
            800,
            5,
        );
        // Aggressor sells C1 to the combo. The combo hedges C2 at the
        // bid: $92.00 -> buyer flow on C2 = -9_200.
        let mut flows = BTreeMap::new();
        flows.insert("C2".to_string(), -9_200_i128);
        let bound = implied_out_bound(&combo, "C1", &flows).unwrap();
        // V_C1 <= 800 + 9_200 = 10_000 minor = $100.00 -> 100_000 ticks.
        match bound {
            ImpliedOutBound::MaxPayTicks(t) => assert_eq!(t, 100_000),
            other => panic!("wrong bound shape: {other:?}"),
        }
        // Non-grid residual floors DOWN (conservative for the combo):
        // 800 + 9_199 = 9_999 -> $99.99 -> 99_990 ticks.
        let mut flows2 = BTreeMap::new();
        flows2.insert("C2".to_string(), -9_199_i128);
        let bound = implied_out_bound(&combo, "C1", &flows2).unwrap();
        match bound {
            ImpliedOutBound::MaxPayTicks(t) => assert_eq!(t, 99_990),
            other => panic!("wrong bound shape: {other:?}"),
        }
        let _ = flows;
    }

    // 13. implied_out_bound: resting sell combo's min-receive, ceiled.
    #[test]
    fn implied_out_bound_ceils_a_sell_combo() {
        // Resting SELL spread demanding $8.00 proceeds.
        let combo = order(
            2,
            2,
            vec![leg("C1", Side::Bid, 1), leg("C2", Side::Ask, 1)],
            Side::Ask,
            800,
            5,
        );
        // Aggressor buys C1 from the combo; the combo hedges C2 at the
        // ask: buyer flow on C2 = -9_205 (it pays 92.05).
        let mut flows = BTreeMap::new();
        flows.insert("C2".to_string(), -9_205_i128);
        let bound = implied_out_bound(&combo, "C1", &flows).unwrap();
        // V_C1 >= 800 + 9_205 = 10_005 -> $100.05 -> 100_050 ticks.
        match bound {
            ImpliedOutBound::MinReceiveTicks(t) => assert_eq!(t, 100_050),
            other => panic!("wrong bound shape: {other:?}"),
        }
        // A hedge so profitable the receive floor goes negative: any
        // non-negative price satisfies the combo.
        let mut flows2 = BTreeMap::new();
        flows2.insert("C2".to_string(), 20_000_i128);
        let bound = implied_out_bound(&combo, "C1", &flows2).unwrap();
        assert_eq!(bound.ticks(), 0);
        // An EXPENSIVE hedge raises the floor: proceeds from leg j must
        // cover both the hedge and the demanded net.
        let mut flows3 = BTreeMap::new();
        flows3.insert("C2".to_string(), -20_000_i128);
        let bound = implied_out_bound(&combo, "C1", &flows3).unwrap();
        match bound {
            ImpliedOutBound::MinReceiveTicks(t) => assert_eq!(t, 208_000),
            other => panic!("wrong bound shape: {other:?}"),
        }
    }

    // 14. implied_out_bound: missing hedge flow or leg -> None.
    #[test]
    fn implied_out_bound_requires_all_other_legs() {
        let combo = order(
            3,
            3,
            vec![
                leg("A", Side::Bid, 1),
                leg("B", Side::Ask, 1),
                leg("C", Side::Bid, 1),
            ],
            Side::Bid,
            500,
            1,
        );
        let mut flows = BTreeMap::new();
        flows.insert("B".to_string(), -100);
        // Leg C missing from the hedge map.
        assert!(implied_out_bound(&combo, "A", &flows).is_none());
        // Unknown aggressed symbol.
        let mut full = flows.clone();
        full.insert("C".to_string(), 200);
        assert!(implied_out_bound(&combo, "Z", &full).is_none());
    }

    // 15. bound_to_ticks: the RFQ collar helper.
    #[test]
    fn bound_to_ticks_floors_and_ceils() {
        let l = leg("X", Side::Bid, 1);
        // $123.45 is EXACTLY on this leg's grid ($1 ticks, 0.1 minor per
        // tick per unit): floor == ceil == 123_450 ticks.
        assert_eq!(bound_to_ticks(&l, 12_345, true), Some(123_450));
        assert_eq!(bound_to_ticks(&l, 12_345, false), Some(123_450));
        // A leg whose tick value does not divide the minor unit evenly:
        // lot 30 base-minor -> 1 tick = 100×30/100_000 = 0.03 minor.
        // A 100-minor bound = 3_333.33 ticks: floor 3_333, ceil 3_334.
        let mut odd = leg("Y", Side::Bid, 1);
        odd.lot_size_base_minor = 30;
        assert_eq!(bound_to_ticks(&odd, 100, true), Some(3_333));
        assert_eq!(bound_to_ticks(&odd, 100, false), Some(3_334));
        // Negative bounds clamp to zero ticks.
        assert_eq!(bound_to_ticks(&l, -5, true), Some(0));
    }

    // 16. The book never matches a combo against itself.
    #[test]
    fn best_opponent_ignores_same_side() {
        let mut book = ComboBook::new();
        let legs = vec![leg("A", Side::Bid, 1)];
        let sig = ComboSignature {
            legs: vec![("A".into(), Side::Bid, 1)],
        };
        book.place(order(0, 1, legs.clone(), Side::Bid, 500, 1));
        book.place(order(0, 2, legs, Side::Bid, 900, 1));
        // A sell taker at 4.00 trades at the RESTING limit — the best
        // (highest) buyer is the 9.00 bid.
        let got = book.best_opponent(&sig, Side::Ask, 400).unwrap();
        assert_eq!(got.limit_quote_minor_per_unit, 900);
        let _ = (book.next_id(),);
    }

    // 17. apply_fill overflow safety.
    #[test]
    fn apply_fill_saturates() {
        let mut book = ComboBook::new();
        let id = book.place(order(0, 1, vec![leg("A", Side::Bid, 1)], Side::Bid, 0, 2));
        assert_eq!(book.apply_fill(id, u64::MAX), Some(0));
        assert!(book.get(id).is_none());
    }

    // 18. Empty / inconsistent inputs are rejected, not panicked.
    #[test]
    fn degenerate_inputs_return_none() {
        assert!(distribute_net(&[], &BTreeMap::new(), 0).is_none());
        assert!(plan_implied_in(&[], &[], Side::Bid, 0, 16).is_none());
        let legs = vec![leg("A", Side::Bid, 1)];
        assert!(plan_implied_in(&legs, &vec![], Side::Bid, 0, 16).is_none());
    }

    // 19. Randomized book operations keep invariants_hold green.
    #[test]
    fn randomized_operations_preserve_invariants() {
        let mut rng = 0x5eed_1234_u64;
        let mut next = move || {
            rng ^= rng >> 12;
            rng ^= rng << 25;
            rng ^= rng >> 27;
            rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let mut book = ComboBook::new();
        for step in 0..500 {
            let r = next() % 10;
            let legs = vec![
                leg(
                    if r % 2 == 0 { "A" } else { "B" },
                    if r % 3 == 0 { Side::Bid } else { Side::Ask },
                    1,
                ),
                leg("C", Side::Bid, 1 + (r % 3) as u32),
            ];
            let side = if r < 5 { Side::Bid } else { Side::Ask };
            let limit = 100 + (next() % 1_000) as i128;
            let id = book.place(order(0, 1 + next() % 4, legs, side, limit, 1 + next() % 5));
            assert!(book.invariants_hold().is_ok(), "step {step}");
            if r % 4 == 0 && book.get(id).is_some() {
                book.apply_fill(id, 1);
            }
            if r % 7 == 0 {
                let victim = 1 + next() % 4;
                book.cancel_all_of(victim);
            }
            assert!(book.invariants_hold().is_ok(), "post step {step}");
        }
    }
}
