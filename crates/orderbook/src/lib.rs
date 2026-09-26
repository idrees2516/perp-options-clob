//! # poc-orderbook
//!
//! A central limit order book with **price-time priority** matching, the
//! standard for crypto CLOBs (Paradigm's network, Derive V3, dYdX v4,
//! Hyperliquid).
//!
//! ## Design: pure matching, deterministic apply
//!
//! The single most important architectural decision in this crate:
//!
//! * [`LimitOrderBook::match_taker`] is **pure** — it reads book state and
//!   returns a [`MatchOutcome`] describing fills and self-trade effects, but
//!   never mutates.
//! * The engine then *applies* those results (or replays the corresponding
//!   events) through [`LimitOrderBook::apply_fill`] and friends.
//!
//! This guarantees that live trading and event-log replay perform *exactly*
//! the same state transitions — the dYdX v4 / Derive V3 determinism model —
//! and makes the matching core trivially property-testable.
//!
//! ## Microstructure choices
//!
//! * Price levels are stored in `BTreeMap`s keyed by price (inverted for
//!   bids) with FIFO `VecDeque` order queues — the [flood](https://github.com/paradigmxyz/flood-rs)
//!   layout: O(log n) best-quote maintenance, no empty-level scanning.
//! * Fills always execute at the **maker's** price (takers get price
//!   improvement), the universal CLOB convention.
//! * Four self-trade-prevention modes; `CancelNewest` is the engine default.

use std::collections::{BTreeMap, VecDeque};

use poc_core::{Order, OrderId, SelfTradePrevention, Side, SubaccountId, Symbol};

/// A resting order plus its queue-time metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestingOrder {
    /// The order (with live fill state).
    pub order: Order,
    /// Resting price in ticks (copied for fast matching).
    pub price_ticks: u64,
}

/// A trade produced by matching, priced at the maker's limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fill {
    /// Taker (incoming) order id.
    pub taker_order_id: OrderId,
    /// Maker (resting) order id.
    pub maker_order_id: OrderId,
    /// Taker's subaccount.
    pub taker_subaccount: SubaccountId,
    /// Maker's subaccount.
    pub maker_subaccount: SubaccountId,
    /// Side of the *maker* (resting order).
    pub maker_side: Side,
    /// Execution price in ticks.
    pub price_ticks: u64,
    /// Execution quantity in lots.
    pub qty_lots: u64,
}

/// Self-trade handling outcome embedded in matching results.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StpEffects {
    /// Resting orders the STP policy canceled.
    pub canceled_makers: Vec<OrderId>,
    /// Resting orders the STP policy reduced without a trade:
    /// `(maker_id, reduced_lots)`.
    pub decrements: Vec<(OrderId, u64)>,
    /// Taker quantity consumed by `DecrementAndCancel` (no trade generated).
    pub taker_consumed_lots: u64,
    /// The STP policy canceled the taker itself.
    pub taker_canceled: bool,
}

/// Result of matching one taker against the book (pure — no mutation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchOutcome {
    /// Trades generated, in execution order.
    pub fills: Vec<Fill>,
    /// Self-trade-prevention effects.
    pub stp: StpEffects,
    /// Taker quantity still unfilled after fills and STP consumption.
    pub taker_remaining_lots: u64,
}

impl MatchOutcome {
    /// Total quantity filled by trades.
    #[must_use]
    pub fn filled_lots(&self) -> u64 {
        self.fills.iter().map(|f| f.qty_lots).sum()
    }
}

/// Aggregated depth level for market-data snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelSnapshot {
    /// Level price in ticks.
    pub price_ticks: u64,
    /// Total resting quantity in lots.
    pub total_qty_lots: u64,
    /// Number of resting orders.
    pub order_count: usize,
}

/// The limit order book for one instrument.
///
/// Invariant (enforced by the engine's place-then-rest flow): **the book is
/// never crossed** — `best_bid < best_ask` at all times, because any incoming
/// order that crosses is matched immediately rather than rested.
pub struct LimitOrderBook {
    symbol: Symbol,
    /// Ask levels: price ascending (first = best ask).
    asks: BTreeMap<u64, VecDeque<OrderId>>,
    /// Bid levels keyed by `INVERT - price` so the *first* entry is the best
    /// (highest) bid. Avoids `range(..).rev()` iteration cost on every match.
    bids_inverted: BTreeMap<u64, VecDeque<OrderId>>,
    /// All resting orders (deterministic iteration by id).
    orders: BTreeMap<OrderId, RestingOrder>,
}

const INVERT: u64 = u64::MAX;

fn invert_bid(price_ticks: u64) -> u64 {
    INVERT - price_ticks
}

impl LimitOrderBook {
    /// Create an empty book for `symbol`.
    #[must_use]
    pub fn new(symbol: Symbol) -> Self {
        Self {
            symbol,
            asks: BTreeMap::new(),
            bids_inverted: BTreeMap::new(),
            orders: BTreeMap::new(),
        }
    }

    /// Instrument symbol.
    #[must_use]
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// Number of resting orders.
    #[must_use]
    pub fn open_order_count(&self) -> usize {
        self.orders.len()
    }

    /// Look up a resting order.
    #[must_use]
    pub fn get(&self, id: OrderId) -> Option<&RestingOrder> {
        self.orders.get(&id)
    }

    /// All resting orders, ascending by order id (deterministic sweeps).
    pub fn resting_orders(&self) -> impl Iterator<Item = &RestingOrder> {
        self.orders.values()
    }

    /// Best (highest) bid price in ticks, if any.
    #[must_use]
    pub fn best_bid(&self) -> Option<u64> {
        self.bids_inverted.keys().next().map(|k| INVERT - k)
    }

    /// Best (lowest) ask price in ticks, if any.
    #[must_use]
    pub fn best_ask(&self) -> Option<u64> {
        self.asks.keys().next().copied()
    }

    /// Whether an incoming order with `side` and `price_ticks` would cross
    /// the opposite side of the book (used for post-only enforcement).
    #[must_use]
    pub fn would_cross(&self, side: Side, price_ticks: u64) -> bool {
        match side {
            Side::Bid => self.best_ask().is_some_and(|ask| price_ticks >= ask),
            Side::Ask => self.best_bid().is_some_and(|bid| price_ticks <= bid),
        }
    }

    /// Resting size at the best bid and ask (lots) — the surface's
    /// touch-size gate (`(0, 0)` when a side is empty).
    #[must_use]
    pub fn best_touch_sizes(&self) -> (u64, u64) {
        let bid = self.best_bid();
        let ask = self.best_ask();
        let mut sizes = (0_u64, 0_u64);
        for resting in self.resting_orders() {
            if let Some(p) = resting.order.price_ticks {
                if Some(p) == bid {
                    sizes.0 = sizes.0.saturating_add(resting.order.open_qty());
                }
                if Some(p) == ask {
                    sizes.1 = sizes.1.saturating_add(resting.order.open_qty());
                }
            }
        }
        sizes
    }

    /// Top-of-book snapshot `(bid, ask)` in ticks.
    #[must_use]
    pub fn bbo(&self) -> (Option<u64>, Option<u64>) {
        (self.best_bid(), self.best_ask())
    }

    /// Rest a (checked, non-crossing) order on the book.
    ///
    /// The engine calls this only for the unfilled remainder of a limit
    /// order, after matching. Inserting an order that crosses the book
    /// breaks the no-cross invariant and is a programming error; callers
    /// must match first. Returns `false` if the order would cross.
    pub fn insert_resting(&mut self, order: Order, price_ticks: u64) -> bool {
        if order.open_qty() == 0 || price_ticks == 0 {
            return false;
        }
        if self.would_cross(order.side, price_ticks) {
            return false;
        }
        let side_levels = match order.side {
            Side::Bid => &mut self.bids_inverted,
            Side::Ask => &mut self.asks,
        };
        let key = match order.side {
            Side::Bid => invert_bid(price_ticks),
            Side::Ask => price_ticks,
        };
        side_levels.entry(key).or_default().push_back(order.id);
        self.orders
            .insert(order.id, RestingOrder { order, price_ticks });
        true
    }

    /// Cancel a resting order. Returns the canceled order.
    pub fn cancel(&mut self, id: OrderId) -> Option<Order> {
        let resting = self.orders.remove(&id)?;
        let (side_levels, key) = match resting.order.side {
            Side::Bid => (&mut self.bids_inverted, invert_bid(resting.price_ticks)),
            Side::Ask => (&mut self.asks, resting.price_ticks),
        };
        if let Some(queue) = side_levels.get_mut(&key) {
            queue.retain(|&qid| qid != id);
            if queue.is_empty() {
                side_levels.remove(&key);
            }
        }
        Some(resting.order)
    }

    /// Reduce a resting order by `lots` without canceling it.
    /// Cancels the order if it reaches zero. Returns the order if present.
    pub fn reduce(&mut self, id: OrderId, lots: u64) -> Option<Order> {
        let resting = self.orders.get_mut(&id)?;
        resting.order.filled_lots = resting.order.filled_lots.saturating_add(lots);
        if !resting.order.is_open() {
            self.cancel(id)
        } else {
            self.orders.get(&id).map(|r| r.order.clone())
        }
    }

    /// Apply one fill: reduce the maker by `fill.qty_lots`.
    ///
    /// This is the *only* mutation path for matching, and it is exactly what
    /// event replay performs — the two can never diverge.
    pub fn apply_fill(&mut self, fill: &Fill) {
        self.reduce(fill.maker_order_id, fill.qty_lots);
    }

    /// Apply a batch of STP effects (cancellations + decrements).
    pub fn apply_stp(&mut self, stp: &StpEffects) {
        for id in &stp.canceled_makers {
            self.cancel(*id);
        }
        for &(id, lots) in &stp.decrements {
            self.reduce(id, lots);
        }
    }

    /// Depth snapshot: up to `levels` per side, bids descending, asks ascending.
    #[must_use]
    pub fn depth(&self, levels: usize) -> (Vec<LevelSnapshot>, Vec<LevelSnapshot>) {
        let bids = self
            .bids_inverted
            .iter()
            .take(levels)
            .map(|(&inv, queue)| {
                let total: u64 = queue
                    .iter()
                    .filter_map(|id| self.orders.get(id))
                    .map(|r| r.order.open_qty())
                    .sum();
                LevelSnapshot {
                    price_ticks: INVERT - inv,
                    total_qty_lots: total,
                    order_count: queue.len(),
                }
            })
            .collect();
        let asks = self
            .asks
            .iter()
            .take(levels)
            .map(|(&price, queue)| {
                let total: u64 = queue
                    .iter()
                    .filter_map(|id| self.orders.get(id))
                    .map(|r| r.order.open_qty())
                    .sum();
                LevelSnapshot {
                    price_ticks: price,
                    total_qty_lots: total,
                    order_count: queue.len(),
                }
            })
            .collect();
        (bids, asks)
    }

    /// Total resting quantity within `price_limit` on the opposite side of
    /// `side` (the volume a limit taker could reach).
    #[must_use]
    pub fn available_within(&self, side: Side, price_limit: Option<u64>) -> u64 {
        let mut total = 0_u64;
        match side {
            Side::Bid => {
                for (&ask_price, queue) in &self.asks {
                    if let Some(limit) = price_limit {
                        if ask_price > limit {
                            break;
                        }
                    }
                    total = total.saturating_add(
                        queue
                            .iter()
                            .filter_map(|id| self.orders.get(id))
                            .map(|r| r.order.open_qty())
                            .sum::<u64>(),
                    );
                }
            }
            Side::Ask => {
                for (&inv, queue) in &self.bids_inverted {
                    let bid_price = INVERT - inv;
                    if let Some(limit) = price_limit {
                        if bid_price < limit {
                            break;
                        }
                    }
                    total = total.saturating_add(
                        queue
                            .iter()
                            .filter_map(|id| self.orders.get(id))
                            .map(|r| r.order.open_qty())
                            .sum::<u64>(),
                    );
                }
            }
        }
        total
    }

    /// Match an incoming taker order against the book. **Pure** — no state
    /// is modified; apply the returned [`MatchOutcome`] afterwards.
    ///
    /// * Limit takers pass `price_ticks` as their price limit.
    /// * Market takers pass `None` for an unbounded sweep (the engine's
    ///   price bands bound slippage instead).
    /// * `fok` requests all-or-nothing semantics: if the reachable volume
    ///   (accounting for STP consumption) cannot fill the order, no fills
    ///   are produced.
    #[must_use]
    pub fn match_taker(
        &self,
        taker: &Order,
        price_limit: Option<u64>,
        fok: bool,
        stp: SelfTradePrevention,
    ) -> MatchOutcome {
        let mut outcome = MatchOutcome {
            fills: Vec::new(),
            stp: StpEffects::default(),
            taker_remaining_lots: taker.open_qty(),
        };

        // Collect candidate maker ids in execution order.
        let candidates: Vec<(OrderId, u64)> = self.execution_queue(taker.side, price_limit);

        // FOK feasibility: volume reachable within the price limit.
        if fok {
            let reachable: u64 = candidates
                .iter()
                .filter_map(|&(id, _)| self.orders.get(&id))
                .map(|r| r.order.open_qty())
                .sum();
            if reachable < taker.open_qty() {
                return outcome; // nothing fills, taker cancels with full remainder
            }
        }

        let mut taker_remaining = taker.open_qty();
        'levels: for (maker_id, _maker_price) in candidates {
            if taker_remaining == 0 || outcome.stp.taker_canceled {
                break;
            }
            // The maker may have been canceled by an earlier STP decision in
            // this same match — `candidates` ids are unique per resting order
            // so at most one STP action applies per id.
            let (maker_open, maker_price) = match self.orders.get(&maker_id) {
                Some(r) => (r.order.open_qty(), r.price_ticks),
                None => continue,
            };

            if self.orders[&maker_id].order.subaccount == taker.subaccount {
                match stp {
                    SelfTradePrevention::CancelNewest => {
                        // Taker dies; maker stays resting.
                        outcome.stp.taker_canceled = true;
                        break 'levels;
                    }
                    SelfTradePrevention::CancelOldest => {
                        outcome.stp.canceled_makers.push(maker_id);
                        continue;
                    }
                    SelfTradePrevention::CancelBoth => {
                        outcome.stp.canceled_makers.push(maker_id);
                        outcome.stp.taker_canceled = true;
                        break 'levels;
                    }
                    SelfTradePrevention::DecrementAndCancel => {
                        let overlap = taker_remaining.min(maker_open);
                        outcome.stp.decrements.push((maker_id, overlap));
                        outcome.stp.taker_consumed_lots =
                            outcome.stp.taker_consumed_lots.saturating_add(overlap);
                        taker_remaining = taker_remaining.saturating_sub(overlap);
                        continue;
                    }
                }
            }

            let fill_qty = taker_remaining.min(maker_open);
            if fill_qty == 0 {
                continue;
            }
            outcome.fills.push(Fill {
                taker_order_id: taker.id,
                maker_order_id: maker_id,
                taker_subaccount: taker.subaccount,
                maker_subaccount: self.orders[&maker_id].order.subaccount,
                maker_side: taker.side.opposite(),
                price_ticks: maker_price,
                qty_lots: fill_qty,
            });
            taker_remaining -= fill_qty;
        }

        outcome.taker_remaining_lots = taker_remaining;
        outcome
    }

    /// Maker ids in price-time execution order reachable from `side`.
    fn execution_queue(&self, side: Side, price_limit: Option<u64>) -> Vec<(OrderId, u64)> {
        let mut out = Vec::new();
        match side {
            Side::Bid => {
                for (&ask_price, queue) in &self.asks {
                    if let Some(limit) = price_limit {
                        if ask_price > limit {
                            break;
                        }
                    }
                    for &id in queue {
                        out.push((id, ask_price));
                    }
                }
            }
            Side::Ask => {
                for (&inv, queue) in &self.bids_inverted {
                    let bid_price = INVERT - inv;
                    if let Some(limit) = price_limit {
                        if bid_price < limit {
                            break;
                        }
                    }
                    for &id in queue {
                        out.push((id, bid_price));
                    }
                }
            }
        }
        out
    }

    /// Assert structural invariants (used in tests and debug builds).
    ///
    /// * no crossed book;
    /// * no empty level buckets;
    /// * every queued id exists in `orders` and sits at its level price;
    /// * resting orders have positive open quantity.
    pub fn invariants_hold(&self) -> Result<(), String> {
        if let (Some(bid), Some(ask)) = (self.best_bid(), self.best_ask()) {
            if bid >= ask {
                return Err(format!("crossed book: bid {bid} >= ask {ask}"));
            }
        }
        for (levels, side) in [(&self.bids_inverted, Side::Bid), (&self.asks, Side::Ask)] {
            for (key, queue) in levels {
                if queue.is_empty() {
                    return Err("empty level bucket".into());
                }
                for id in queue {
                    match self.orders.get(id) {
                        None => return Err(format!("dangling queue id {id}")),
                        Some(r) => {
                            if r.order.side != side {
                                return Err(format!("side mismatch for order {id}"));
                            }
                            let expected = match side {
                                Side::Bid => invert_bid(r.price_ticks),
                                Side::Ask => r.price_ticks,
                            };
                            if *key != expected {
                                return Err(format!("order {id} queued at wrong level"));
                            }
                            if r.order.open_qty() == 0 {
                                return Err(format!("zero-qty resting order {id}"));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poc_core::{OrderType, TimeInForce};

    fn order(id: OrderId, sub: SubaccountId, side: Side, price: u64, qty: u64) -> Order {
        Order {
            id,
            subaccount: sub,
            symbol: "BTC-PERP".into(),
            side,
            order_type: OrderType::Limit,
            price_ticks: Some(price),
            qty_lots: qty,
            filled_lots: 0,
            tif: TimeInForce::Gtc,
            post_only: false,
            reduce_only: false,
            stp: SelfTradePrevention::CancelNewest,
            client_ts: 0,
            engine_ts: 0,
        }
    }

    fn book_with_liquidity() -> LimitOrderBook {
        let mut b = LimitOrderBook::new("BTC-PERP".into());
        // Asks: 101 (x5), 102 (x3), 103 (x2)
        b.insert_resting(order(1, 10, Side::Ask, 101, 5), 101);
        b.insert_resting(order(2, 10, Side::Ask, 102, 3), 102);
        b.insert_resting(order(3, 11, Side::Ask, 103, 2), 103);
        // Bids: 99 (x4), 98 (x6)
        b.insert_resting(order(4, 12, Side::Bid, 99, 4), 99);
        b.insert_resting(order(5, 12, Side::Bid, 98, 6), 98);
        b
    }

    #[test]
    fn bbo_and_depth() {
        let b = book_with_liquidity();
        assert_eq!(b.bbo(), (Some(99), Some(101)));
        let (bids, asks) = b.depth(2);
        assert_eq!(
            bids[0],
            LevelSnapshot {
                price_ticks: 99,
                total_qty_lots: 4,
                order_count: 1
            }
        );
        assert_eq!(
            asks[0],
            LevelSnapshot {
                price_ticks: 101,
                total_qty_lots: 5,
                order_count: 1
            }
        );
        assert_eq!(asks[1].price_ticks, 102);
        assert_eq!(asks.len(), 2, "depth(2) truncates");
    }

    #[test]
    fn price_time_priority_fifo() {
        let mut b = LimitOrderBook::new("X".into());
        b.insert_resting(order(1, 10, Side::Ask, 100, 2), 100);
        b.insert_resting(order(2, 11, Side::Ask, 100, 2), 100);
        b.insert_resting(order(3, 12, Side::Ask, 101, 5), 101);
        // Taker buys 5: 2 from #1, 2 from #2 (same price, FIFO), then 1 from #3.
        let taker = order(9, 99, Side::Bid, 101, 5);
        let out = b.match_taker(&taker, Some(101), false, SelfTradePrevention::CancelNewest);
        let ids: Vec<OrderId> = out.fills.iter().map(|f| f.maker_order_id).collect();
        let qtys: Vec<u64> = out.fills.iter().map(|f| f.qty_lots).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(qtys, vec![2, 2, 1]);
        assert_eq!(out.taker_remaining_lots, 0);
        assert!(out
            .fills
            .iter()
            .all(|f| f.price_ticks == 100 || f.price_ticks == 101));
        // Apply and verify.
        for f in &out.fills {
            b.apply_fill(f);
        }
        assert!(b.get(1).is_none() && b.get(2).is_none());
        assert_eq!(b.get(3).map(|r| r.order.open_qty()), Some(4));
        assert!(b.invariants_hold().is_ok());
    }

    #[test]
    fn taker_gets_price_improvement() {
        let b = book_with_liquidity();
        // Bid limit 103 sweeps 101 and 102 levels, both priced at maker price.
        let taker = order(9, 99, Side::Bid, 103, 7);
        let out = b.match_taker(&taker, Some(103), false, SelfTradePrevention::CancelNewest);
        assert_eq!(out.fills.len(), 2);
        assert_eq!(out.fills[0].price_ticks, 101);
        assert_eq!(out.fills[1].price_ticks, 102);
        assert_eq!(out.filled_lots(), 7);
    }

    #[test]
    fn fok_zero_or_all() {
        let b = book_with_liquidity();
        // Reachable ask volume within 101 = 5 lots; ask 8 -> infeasible.
        let taker = order(9, 99, Side::Bid, 101, 8);
        let out = b.match_taker(&taker, Some(101), true, SelfTradePrevention::CancelNewest);
        assert!(out.fills.is_empty());
        assert_eq!(out.taker_remaining_lots, 8);
        // Exactly feasible -> fills.
        let taker = order(10, 99, Side::Bid, 101, 5);
        let out = b.match_taker(&taker, Some(101), true, SelfTradePrevention::CancelNewest);
        assert_eq!(out.filled_lots(), 5);
    }

    #[test]
    fn limit_respected() {
        let b = book_with_liquidity();
        // Bid at 100 cannot lift the 101 ask.
        let taker = order(9, 99, Side::Bid, 100, 5);
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        assert!(out.fills.is_empty());
        assert_eq!(out.taker_remaining_lots, 5);
        // Sell limit 100 cannot hit the 99 bid.
        let taker = order(10, 99, Side::Ask, 100, 4);
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        assert!(out.fills.is_empty());
    }

    #[test]
    fn stp_cancel_newest_kills_taker() {
        let mut b = LimitOrderBook::new("X".into());
        b.insert_resting(order(1, 7, Side::Ask, 100, 5), 100);
        let taker = order(2, 7, Side::Bid, 100, 3); // same subaccount
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        assert!(out.fills.is_empty());
        assert!(out.stp.taker_canceled);
        assert_eq!(out.taker_remaining_lots, 3);
        // Maker untouched.
        assert!(b.get(1).is_some());
    }

    #[test]
    fn stp_cancel_oldest_and_both() {
        let mut b = LimitOrderBook::new("X".into());
        b.insert_resting(order(1, 7, Side::Ask, 100, 5), 100);
        b.insert_resting(order(2, 8, Side::Ask, 100, 5), 100);
        let taker = order(3, 7, Side::Bid, 100, 3);

        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelOldest);
        assert_eq!(out.stp.canceled_makers, vec![1]);
        assert!(!out.stp.taker_canceled);
        // Taker continues to next maker (different sub).
        assert_eq!(out.filled_lots(), 3);
        assert_eq!(out.fills[0].maker_order_id, 2);

        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelBoth);
        assert_eq!(out.stp.canceled_makers, vec![1]);
        assert!(out.stp.taker_canceled);
        assert!(out.fills.is_empty());
    }

    #[test]
    fn stp_decrement_and_cancel() {
        let mut b = LimitOrderBook::new("X".into());
        b.insert_resting(order(1, 7, Side::Ask, 100, 5), 100);
        b.insert_resting(order(2, 8, Side::Ask, 100, 5), 100);
        let taker = order(3, 7, Side::Bid, 100, 3);
        let out = b.match_taker(
            &taker,
            Some(100),
            false,
            SelfTradePrevention::DecrementAndCancel,
        );
        // Overlap of 3 decrements maker 1 and consumes the taker fully.
        assert!(out.fills.is_empty());
        assert_eq!(out.stp.decrements, vec![(1, 3)]);
        assert_eq!(out.stp.taker_consumed_lots, 3);
        assert_eq!(out.taker_remaining_lots, 0);
        b.apply_stp(&out.stp);
        assert_eq!(b.get(1).map(|r| r.order.open_qty()), Some(2));
        assert!(b.invariants_hold().is_ok());
    }

    #[test]
    fn fok_accounts_for_stp_consumption() {
        let mut b = LimitOrderBook::new("X".into());
        b.insert_resting(order(1, 7, Side::Ask, 100, 5), 100);
        b.insert_resting(order(2, 8, Side::Ask, 101, 5), 101);
        // Taker from sub 7 wants 10; 5 via STP decrement + 5 traded with maker 2.
        let taker = order(3, 7, Side::Bid, 101, 10);
        let out = b.match_taker(
            &taker,
            Some(101),
            true,
            SelfTradePrevention::DecrementAndCancel,
        );
        assert_eq!(out.stp.decrements, vec![(1, 5)]);
        assert_eq!(out.stp.taker_consumed_lots, 5);
        assert_eq!(
            out.filled_lots(),
            5,
            "remainder fills against the other maker"
        );
        assert_eq!(out.fills[0].maker_order_id, 2);
        assert_eq!(
            out.taker_remaining_lots, 0,
            "FOK fully satisfied via fills + STP"
        );
    }

    #[test]
    fn partial_fill_then_reduce_and_cancel() {
        let mut b = book_with_liquidity();
        assert_eq!(b.reduce(4, 1).map(|o| o.open_qty()), Some(3));
        assert_eq!(b.reduce(4, 3).map(|o| o.open_qty()), Some(0));
        assert!(b.get(4).is_none());
        assert!(b.invariants_hold().is_ok());
    }

    /// Deterministic pseudo-random session: apply arbitrary matches/cancels,
    /// then assert the book's structural invariants.
    #[test]
    fn random_session_invariants() {
        struct XorShift(u64);
        impl XorShift {
            fn next(&mut self) -> u64 {
                let mut x = self.0;
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                self.0 = x;
                x
            }
        }
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let mut b = LimitOrderBook::new("X".into());
        let mut next_id = 1_u64;

        for round in 0..2000 {
            let sub = 1 + (rng.next() % 5);
            let side = if rng.next() % 2 == 0 {
                Side::Bid
            } else {
                Side::Ask
            };
            let price = 100 + (rng.next() % 20);
            let qty = 1 + (rng.next() % 7);

            if b.would_cross(side, price) {
                continue; // engine would match first; keep this unit pure
            }
            let o = order(next_id, sub, side, price, qty);
            next_id += 1;
            b.insert_resting(o, price);

            // Randomly sweep as taker.
            if round % 3 == 0 {
                let tside = if rng.next() % 2 == 0 {
                    Side::Bid
                } else {
                    Side::Ask
                };
                let tqty = 1 + (rng.next() % 10);
                let taker = order(next_id, 9, tside, price, tqty);
                next_id += 1;
                let limit = if rng.next() % 2 == 0 {
                    None
                } else {
                    Some(105 + rng.next() % 15)
                };
                let stp_mode = match rng.next() % 4 {
                    0 => SelfTradePrevention::CancelNewest,
                    1 => SelfTradePrevention::CancelOldest,
                    2 => SelfTradePrevention::CancelBoth,
                    _ => SelfTradePrevention::DecrementAndCancel,
                };
                let out = b.match_taker(&taker, limit, false, stp_mode);
                for f in &out.fills {
                    b.apply_fill(f);
                }
                b.apply_stp(&out.stp);
            }
            assert!(b.invariants_hold().is_ok(), "round {round}");
        }
    }
}
