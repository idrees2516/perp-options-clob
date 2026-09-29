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
//! ## Microstructure: zkLighter-style channels
//!
//! Price levels are stored in `BTreeMap`s keyed by price (inverted for
//! bids) — O(log n) best-quote maintenance, no empty-level scanning — and
//! each level's FIFO queue is a chain of **channels**: fixed-capacity
//! groups of eight resting orders with a cached aggregate of their
//! visible quantity. This is the zkLighter orderbook layout:
//!
//! * **Bounded work per match step** — matching walks whole channels;
//!   a taker that exhausts a level touches only that level's channels,
//!   and the per-step cost is structurally capped (8 slot probes),
//!   which is what makes zk-provable matching and per-block work
//!   budgeting tractable in production rollups.
//! * **O(1) aggregate queries** — every channel caches its live visible
//!   quantity and live order count, and every level caches the sum, so
//!   [`best_touch_sizes`] and [`depth`] answer from cached numbers
//!   instead of scanning orders. (The vol-surface observation path, which
//!   runs per option market per tick, went from *O(orders)* to *O(1)*.)
//! * **Lazy matching with early termination** — [`match_taker`] pulls
//!   candidates level-by-level through the channel chain and stops the
//!   moment the taker is filled, instead of materializing the full
//!   reachable order list up front.
//! * **Tombstone cancels** — canceling tombstones the order's slot in
//!   its channel and decrements the cached aggregates; fully-dead
//!   channels are dropped from the level.
//!
//! ## Microstructure conventions
//!
//! * Fills always execute at the **maker's** price (takers get price
//!   improvement), the universal CLOB convention.
//! * Four self-trade-prevention modes; `CancelNewest` is the engine default.
//! * The uniform-price call auction ([`uncross`]) clears with prefix-sum
//!   supply/demand curves in O(L log L), not the naive O(L²) candidate
//!   scan.

use std::collections::{BTreeMap, VecDeque};

use poc_core::{Order, OrderId, SelfTradePrevention, Side, SubaccountId, Symbol};

/// Number of resting order slots per channel (zkLighter layout).
pub const CHANNEL_CAPACITY: usize = 8;

/// A resting order plus its queue-time metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestingOrder {
    /// The order (with live fill state).
    pub order: Order,
    /// Resting price in ticks (copied for fast matching).
    pub price_ticks: u64,
    /// Lots currently *displayed* at the level: the full open quantity for
    /// plain orders, the visible iceberg slice for hidden-quantity orders
    /// (G-06). Matching, depth, and touch-size views only ever see this.
    pub visible_lots: u64,
}

impl RestingOrder {
    /// The matching-relevant size of this resting order: its displayed
    /// slice, never its hidden remainder.
    #[must_use]
    pub fn visible_qty(&self) -> u64 {
        self.visible_lots
    }
}

/// One channel: a fixed-capacity group of resting order ids at a single
/// price level, with cached aggregates.
///
/// Slot `0` is the tombstone sentinel (engine order ids start at 1). A
/// channel's `visible_total` is the sum of its live orders' visible lots;
/// keeping it cached is what turns touch-size and depth queries from
/// per-order scans into O(1) lookups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    slots: [OrderId; CHANNEL_CAPACITY],
    live: u8,
    visible_total: u64,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            slots: [0; CHANNEL_CAPACITY],
            live: 0,
            visible_total: 0,
        }
    }
}

impl Channel {
    /// Append `id` strictly **after the last live slot** — the first
    /// free slot beyond every live order. Cancels tombstone slots in the
    /// middle; a new (or re-queued, iceberg-resliced) arrival must land
    /// behind every currently-live order or price-time priority breaks.
    /// Returns `false` when the channel is full past its live prefix.
    fn push(&mut self, id: OrderId, visible: u64) -> bool {
        let mut target = 0_usize;
        for (i, &s) in self.slots.iter().enumerate() {
            if s != 0 {
                target = i + 1;
            }
        }
        if target >= CHANNEL_CAPACITY {
            return false;
        }
        self.slots[target] = id;
        self.live += 1;
        self.visible_total = self.visible_total.saturating_add(visible);
        true
    }

    /// Tombstone `id`, subtracting its visible quantity.
    fn remove(&mut self, id: OrderId, visible: u64) {
        for slot in &mut self.slots {
            if *slot == id {
                *slot = 0;
                self.live = self.live.saturating_sub(1);
                self.visible_total = self.visible_total.saturating_sub(visible);
                return;
            }
        }
    }

    /// Live order ids in arrival (price-time) order.
    fn iter_live(&self) -> impl Iterator<Item = OrderId> + '_ {
        self.slots.iter().filter(|&&id| id != 0).copied()
    }

    /// Number of live orders.
    fn live_count(&self) -> usize {
        usize::from(self.live)
    }
}

/// A price level's FIFO queue: a chain of channels plus the level's
/// cached aggregates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelQueue {
    channels: VecDeque<Channel>,
    total_visible: u64,
    live_orders: u64,
}

impl ChannelQueue {
    /// Append an order (to the tail channel if it has room, else a new
    /// channel — O(1) amortized).
    fn push(&mut self, id: OrderId, visible: u64) {
        if let Some(back) = self.channels.back_mut() {
            if back.push(id, visible) {
                self.total_visible = self.total_visible.saturating_add(visible);
                self.live_orders += 1;
                return;
            }
        }
        let mut ch = Channel::default();
        ch.push(id, visible);
        self.channels.push_back(ch);
        self.total_visible = self.total_visible.saturating_add(visible);
        self.live_orders += 1;
    }

    /// Tombstone an order and drop channels it empties.
    fn remove(&mut self, id: OrderId, visible: u64) {
        for ch in &mut self.channels {
            let before = ch.live_count();
            ch.remove(id, visible);
            if ch.live_count() != before {
                self.total_visible = self.total_visible.saturating_sub(visible);
                self.live_orders = self.live_orders.saturating_sub(1);
                if ch.live_count() == 0 {
                    self.channels.retain(|c| c.live_count() > 0);
                }
                return;
            }
        }
    }

    /// Adjust one order's cached visible contribution by `delta` (the
    /// order stays in its slot — partial fills and iceberg slicing only
    /// shrink the displayed quantity, never the queue position).
    fn adjust_visible(&mut self, id: OrderId, delta: u64) {
        if delta == 0 {
            return;
        }
        for ch in &mut self.channels {
            if ch.slots.contains(&id) {
                ch.visible_total = ch.visible_total.saturating_sub(delta);
                self.total_visible = self.total_visible.saturating_sub(delta);
                return;
            }
        }
    }

    /// Live order ids in price-time order (lazily pulls through the
    /// channel chain, skipping tombstones).
    fn iter_live(&self) -> impl Iterator<Item = OrderId> + '_ {
        self.channels.iter().flat_map(Channel::iter_live)
    }

    /// Cached total visible quantity at the level.
    fn total_visible(&self) -> u64 {
        self.total_visible
    }

    /// Cached live order count.
    fn live_orders(&self) -> u64 {
        self.live_orders
    }
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

/// Result of uncrossing an auction book at a uniform price (G-12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuctionOutcome {
    /// The uniform clearing price (`None` when the book did not cross).
    pub clearing_price_ticks: Option<u64>,
    /// Fills at the clearing price, in price-time priority order.
    pub fills: Vec<Fill>,
    /// Total matched quantity in lots.
    pub matched_lots: u64,
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
/// order that crosses is matched immediately rather than rested. The one
/// exception is **auction mode** (G-12): while an opening/periodic auction
/// accumulates orders, crossing is allowed (nothing matches until the
/// uncross) and the invariant is re-checked by the uncross itself.
pub struct LimitOrderBook {
    symbol: Symbol,
    /// Ask levels: price ascending (first = best ask).
    asks: BTreeMap<u64, ChannelQueue>,
    /// Bid levels keyed by `INVERT - price` so the *first* entry is the best
    /// (highest) bid. Avoids `range(..).rev()` iteration cost on every match.
    bids_inverted: BTreeMap<u64, ChannelQueue>,
    /// All resting orders (deterministic iteration by id).
    orders: BTreeMap<OrderId, RestingOrder>,
    /// Auction mode (G-12): orders accumulate without matching and may
    /// cross; `uncross` clears them at a uniform price.
    auction: bool,
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
            auction: false,
        }
    }

    /// Instrument symbol.
    #[must_use]
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    /// Whether the book accumulates auction orders without matching (G-12).
    #[must_use]
    pub fn auction_mode(&self) -> bool {
        self.auction
    }

    /// Toggle auction mode. Turning it on allows crossing accumulation;
    /// turning it off restores the continuous no-cross invariant (the
    /// uncross is expected to have removed any crossing overlap first).
    pub fn set_auction_mode(&mut self, on: bool) {
        self.auction = on;
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
    /// touch-size gate (`(0, 0)` when a side is empty). Displays the
    /// *visible* slice only (G-06).
    ///
    /// O(1): the level's channel chain caches the visible aggregate, so
    /// the vol-surface per-tick observation never scans orders.
    #[must_use]
    pub fn best_touch_sizes(&self) -> (u64, u64) {
        let bid = self
            .bids_inverted
            .keys()
            .next()
            .and_then(|k| self.bids_inverted.get(k))
            .map_or(0, ChannelQueue::total_visible);
        let ask = self
            .asks
            .keys()
            .next()
            .and_then(|k| self.asks.get(k))
            .map_or(0, ChannelQueue::total_visible);
        (bid, ask)
    }

    /// Top-of-book snapshot `(bid, ask)` in ticks.
    #[must_use]
    pub fn bbo(&self) -> (Option<u64>, Option<u64>) {
        (self.best_bid(), self.best_ask())
    }

    /// Rest a (checked, non-crossing) order on the book.
    ///
    /// The engine calls this only for the unfilled remainder of a limit
    /// order, after matching (or when accumulating auction orders — G-12 —
    /// where crossing is allowed). Inserting a crossing order in continuous
    /// mode breaks the no-cross invariant and is a programming error;
    /// callers must match first. Returns `false` if the order would cross.
    pub fn insert_resting(&mut self, order: Order, price_ticks: u64) -> bool {
        if order.open_qty() == 0 || price_ticks == 0 {
            return false;
        }
        if !self.auction && self.would_cross(order.side, price_ticks) {
            return false;
        }
        let visible = match order.display_lots {
            Some(display) if display > 0 => order.open_qty().min(display),
            _ => order.open_qty(),
        };
        let side_levels = match order.side {
            Side::Bid => &mut self.bids_inverted,
            Side::Ask => &mut self.asks,
        };
        let key = match order.side {
            Side::Bid => invert_bid(price_ticks),
            Side::Ask => price_ticks,
        };
        side_levels.entry(key).or_default().push(order.id, visible);
        self.orders.insert(
            order.id,
            RestingOrder {
                order,
                price_ticks,
                visible_lots: visible,
            },
        );
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
            queue.remove(id, resting.visible_lots);
            if queue.live_orders() == 0 {
                side_levels.remove(&key);
            }
        }
        Some(resting.order)
    }

    /// Reduce a resting order by `lots` without canceling it.
    /// Cancels the order if it reaches zero. Returns the order if present.
    ///
    /// Iceberg reslicing (G-06): when the reduction consumes the visible
    /// slice but the order still has hidden quantity, a fresh slice is
    /// revealed and the order re-queues at the **back** of its price level
    /// (the Deribit rule: revealing new size is joining the queue anew).
    /// The reslice is a pure function of the book state and the fill, so
    /// event replay reproduces queue positions exactly.
    pub fn reduce(&mut self, id: OrderId, lots: u64) -> Option<Order> {
        let (side, level_key, was_visible, display) = {
            let resting = self.orders.get(&id)?;
            (
                resting.order.side,
                match resting.order.side {
                    Side::Bid => invert_bid(resting.price_ticks),
                    Side::Ask => resting.price_ticks,
                },
                resting.visible_lots,
                resting.order.display_lots,
            )
        };
        let resting = self.orders.get_mut(&id)?;
        resting.order.filled_lots = resting.order.filled_lots.saturating_add(lots);
        if !resting.order.is_open() {
            return self.cancel(id);
        }
        let open = resting.order.open_qty();
        let side_levels = match side {
            Side::Bid => &mut self.bids_inverted,
            Side::Ask => &mut self.asks,
        };
        match display {
            Some(display) if display > 0 => {
                let slice_left = was_visible.saturating_sub(lots);
                if slice_left == 0 {
                    // Slice exhausted: reveal the next slice and re-queue at
                    // the back of the level (Deribit iceberg rule).
                    let new_visible = open.min(display).max(1);
                    resting.visible_lots = new_visible;
                    if let Some(queue) = side_levels.get_mut(&level_key) {
                        queue.remove(id, was_visible);
                        queue.push(id, new_visible);
                    }
                } else {
                    // Partial slice consumption: shrink the aggregate in
                    // place — queue position is untouched.
                    resting.visible_lots = slice_left;
                    if let Some(queue) = side_levels.get_mut(&level_key) {
                        queue.adjust_visible(id, was_visible - slice_left);
                    }
                }
            }
            _ => {
                // Plain order: visible always equals the open quantity.
                let new_visible = open;
                resting.visible_lots = new_visible;
                if let Some(queue) = side_levels.get_mut(&level_key) {
                    queue.adjust_visible(id, was_visible.saturating_sub(new_visible));
                }
            }
        }
        self.orders.get(&id).map(|r| r.order.clone())
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

    /// Depth snapshot: up to `levels` per side, bids descending, asks
    /// ascending. Aggregates the *visible* slice only (G-06).
    ///
    /// O(levels): each level answers from its cached channel aggregate.
    #[must_use]
    pub fn depth(&self, levels: usize) -> (Vec<LevelSnapshot>, Vec<LevelSnapshot>) {
        let bids = self
            .bids_inverted
            .iter()
            .take(levels)
            .map(|(&inv, queue)| LevelSnapshot {
                price_ticks: INVERT - inv,
                total_qty_lots: queue.total_visible(),
                order_count: usize::try_from(queue.live_orders()).unwrap_or(usize::MAX),
            })
            .collect();
        let asks = self
            .asks
            .iter()
            .take(levels)
            .map(|(&price, queue)| LevelSnapshot {
                price_ticks: price,
                total_qty_lots: queue.total_visible(),
                order_count: usize::try_from(queue.live_orders()).unwrap_or(usize::MAX),
            })
            .collect();
        (bids, asks)
    }

    /// Total *visible* quantity within `price_limit` on the opposite side
    /// of `side` (the volume a limit taker could reach).
    ///
    /// O(levels within the limit): level aggregates, no per-order scan.
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
                    total = total.saturating_add(queue.total_visible());
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
                    total = total.saturating_add(queue.total_visible());
                }
            }
        }
        total
    }
}

impl LimitOrderBook {
    /// Uncross an auction book at a uniform clearing price (G-12).
    ///
    /// The algorithm is the classic uniform-price call auction (NYSE /
    /// Deutsche Börse opening-auction shape, the one Derive V3 opens new
    /// markets with), cleared through **prefix-sum supply and demand
    /// curves** so each candidate price evaluates in O(log L):
    ///
    /// 1. Candidate prices are every resting bid and ask level.
    /// 2. At each candidate `P`, the executable volume is
    ///    `min(cum_bids_at_or_above(P), cum_asks_at_or_below(P))` —
    ///    the full open quantity participates, including iceberg
    ///    remainder (display hiding is a continuous-trading concern;
    ///    auctions print everything at one fair price).
    /// 3. The clearing price maximizes executable volume; ties are
    ///    broken toward the indicative band midpoint
    ///    `(best_bid + best_ask) / 2`, then toward the lower price.
    /// 4. Fills pair price-time-priority bids (descending) with asks
    ///    (ascending) until the clearing volume is exhausted; every fill
    ///    prints at the clearing price. The later-arriving order is the
    ///    reported taker (it joined after the price was already improving).
    ///
    /// Pure: returns the outcome; the engine applies the fills.
    #[must_use]
    pub fn uncross(&self) -> AuctionOutcome {
        let mut outcome = AuctionOutcome {
            clearing_price_ticks: None,
            fills: Vec::new(),
            matched_lots: 0,
        };
        let (Some(best_bid), Some(best_ask)) = self.bbo() else {
            return outcome;
        };
        if best_bid < best_ask {
            return outcome; // nothing crosses: no auction print
        }

        // Eligible orders in price-time order (full open quantity).
        let mut bids: Vec<(OrderId, SubaccountId, u64, u64)> = Vec::new();
        for (&inv, queue) in self.bids_inverted.iter() {
            for id in queue.iter_live() {
                if let Some(r) = self.orders.get(&id) {
                    bids.push((id, r.order.subaccount, INVERT - inv, r.order.open_qty()));
                }
            }
        }
        let mut asks: Vec<(OrderId, SubaccountId, u64, u64)> = Vec::new();
        for (&price, queue) in &self.asks {
            for id in queue.iter_live() {
                if let Some(r) = self.orders.get(&id) {
                    asks.push((id, r.order.subaccount, price, r.order.open_qty()));
                }
            }
        }

        // Prefix-sum curves. Bid prices are already descending (bids_inverted
        // ascending key = descending price); asks ascending. cum[i] = total
        // quantity at prices at-or-better-than level i.
        let bid_prices: Vec<u64> = bids.iter().map(|b| b.2).collect();
        let ask_prices: Vec<u64> = asks.iter().map(|a| a.2).collect();
        let mut bid_prefix: Vec<u64> = Vec::with_capacity(bids.len());
        {
            let mut acc = 0_u64;
            for &(_, _, _, q) in &bids {
                acc = acc.saturating_add(q);
                bid_prefix.push(acc);
            }
        }
        let mut ask_prefix: Vec<u64> = Vec::with_capacity(asks.len());
        {
            let mut acc = 0_u64;
            for &(_, _, _, q) in &asks {
                acc = acc.saturating_add(q);
                ask_prefix.push(acc);
            }
        }

        // Quantity of bids at price >= p (binary search on the descending
        // price vector) and asks at price <= p (ascending vector).
        let bid_qty_at = |p: u64| -> u64 {
            // prices descending: first index with price < p is the cutoff.
            let n = bid_prices.partition_point(|&x| x >= p);
            if n == 0 {
                0
            } else {
                bid_prefix[n - 1]
            }
        };
        let ask_qty_at = |p: u64| -> u64 {
            // prices ascending: first index with price > p is the cutoff.
            let n = ask_prices.partition_point(|&x| x <= p);
            if n == 0 {
                0
            } else {
                ask_prefix[n - 1]
            }
        };

        let mut candidates: Vec<u64> = bid_prices.clone();
        candidates.extend_from_slice(&ask_prices);
        candidates.sort_unstable();
        candidates.dedup();
        let mid = (best_bid + best_ask) / 2;
        let mut best_price = 0_u64;
        let mut best_volume = 0_u64;
        for &p in &candidates {
            let volume = bid_qty_at(p).min(ask_qty_at(p));
            let better = volume > best_volume
                || (volume == best_volume
                    && volume > 0
                    && (p.abs_diff(mid), p) < (best_price.abs_diff(mid), best_price));
            if better {
                best_price = p;
                best_volume = volume;
            }
        }
        if best_volume == 0 {
            return outcome;
        }

        // Pair price-time priority at the uniform price, tracking each
        // order's unmatched remainder.
        let mut bid_rem: Vec<(OrderId, SubaccountId, u64)> = bids
            .iter()
            .map(|&(id, sub, _px, qty)| (id, sub, qty))
            .collect();
        let mut ask_rem: Vec<(OrderId, SubaccountId, u64)> = asks
            .iter()
            .map(|&(id, sub, _px, qty)| (id, sub, qty))
            .collect();
        let mut bi = 0_usize;
        let mut ai = 0_usize;
        let mut matched = 0_u64;
        while bi < bid_rem.len() && ai < ask_rem.len() && matched < best_volume {
            let (bid_id, bid_sub, bq) = bid_rem[bi];
            let (ask_id, ask_sub, aq) = ask_rem[ai];
            let qty = bq.min(aq).min(best_volume - matched);
            if qty == 0 {
                break;
            }
            // Later-arriving order reports as the taker.
            let (taker_id, taker_sub, maker_id, maker_sub, maker_side) = if ask_id > bid_id {
                (ask_id, ask_sub, bid_id, bid_sub, Side::Bid)
            } else {
                (bid_id, bid_sub, ask_id, ask_sub, Side::Ask)
            };
            outcome.fills.push(Fill {
                taker_order_id: taker_id,
                maker_order_id: maker_id,
                taker_subaccount: taker_sub,
                maker_subaccount: maker_sub,
                maker_side,
                price_ticks: best_price,
                qty_lots: qty,
            });
            matched = matched.saturating_add(qty);
            bid_rem[bi].2 = bq - qty;
            ask_rem[ai].2 = aq - qty;
            if bid_rem[bi].2 == 0 {
                bi += 1;
            }
            if ask_rem[ai].2 == 0 {
                ai += 1;
            }
        }
        outcome.clearing_price_ticks = Some(best_price);
        outcome.matched_lots = matched;
        outcome
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
    ///
    /// zkLighter-style laziness: candidates are pulled level-by-level
    /// through the channel chains and the walk **stops the moment the
    /// taker is filled** — a top-of-book fill never pays for the deep
    /// levels behind it. The reachable volume for FOK is summed from the
    /// levels' cached aggregates, not from per-order scans.
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

        // FOK feasibility from level aggregates (O(levels within limit)).
        if fok {
            let reachable = self.available_within(taker.side, price_limit);
            if reachable < taker.open_qty() {
                return outcome; // nothing fills, taker cancels with full remainder
            }
        }

        // Level iteration in execution order.
        macro_rules! walk_side {
            ($levels:expr, $price_of:expr, $within_limit:expr) => {{
                let mut levels = $levels.iter();
                'levels: while let Some((&key, queue)) = levels.next() {
                    let price = $price_of(key);
                    if !$within_limit(price) {
                        break 'levels;
                    }
                    // Pull orders through the channel chain; skip
                    // tombstoned slots without touching them.
                    let mut channel_iter = queue.iter_live();
                    while let Some(maker_id) = channel_iter.next() {
                        if outcome.stp.taker_canceled {
                            break 'levels;
                        }
                        if outcome.taker_remaining_lots == 0 {
                            break 'levels;
                        }
                        let (maker_open, maker_price, maker_sub) = match self.orders.get(&maker_id)
                        {
                            Some(r) => (r.visible_lots, r.price_ticks, r.order.subaccount),
                            None => continue, // stale slot: skipped lazily
                        };
                        let _ = maker_price;

                        if maker_sub == taker.subaccount {
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
                                    let overlap = outcome.taker_remaining_lots.min(maker_open);
                                    outcome.stp.decrements.push((maker_id, overlap));
                                    outcome.stp.taker_consumed_lots =
                                        outcome.stp.taker_consumed_lots.saturating_add(overlap);
                                    outcome.taker_remaining_lots =
                                        outcome.taker_remaining_lots.saturating_sub(overlap);
                                    continue;
                                }
                            }
                        }

                        let fill_qty = outcome.taker_remaining_lots.min(maker_open);
                        if fill_qty == 0 {
                            continue;
                        }
                        outcome.fills.push(Fill {
                            taker_order_id: taker.id,
                            maker_order_id: maker_id,
                            taker_subaccount: taker.subaccount,
                            maker_subaccount: maker_sub,
                            maker_side: taker.side.opposite(),
                            price_ticks: maker_price,
                            qty_lots: fill_qty,
                        });
                        outcome.taker_remaining_lots -= fill_qty;
                    }
                }
            }};
        }

        match taker.side {
            Side::Bid => walk_side!(self.asks, |key: u64| key, |price: u64| price_limit
                .map_or(true, |limit| price <= limit)),
            Side::Ask => walk_side!(self.bids_inverted, |key: u64| INVERT - key, |price: u64| {
                price_limit.map_or(true, |limit| price >= limit)
            }),
        }

        outcome
    }

    /// Assert structural invariants (used in tests and debug builds).
    ///
    /// * no crossed book (unless auction mode is accumulating, G-12);
    /// * no empty level buckets;
    /// * every live channel slot references an order in `orders`, at the
    ///   right level and side, with positive open quantity and a visible
    ///   slice that never exceeds it (G-06);
    /// * every channel's cached aggregate equals the sum of its live
    ///   orders' visible lots, and every level's cached aggregate equals
    ///   the sum of its channels' (the zkLighter channel contract).
    pub fn invariants_hold(&self) -> Result<(), String> {
        if let (Some(bid), Some(ask)) = (self.best_bid(), self.best_ask()) {
            if bid >= ask && !self.auction {
                return Err(format!("crossed book: bid {bid} >= ask {ask}"));
            }
        }
        for (levels, side) in [(&self.bids_inverted, Side::Bid), (&self.asks, Side::Ask)] {
            for (key, queue) in levels {
                if queue.channels.is_empty() || queue.live_orders() == 0 {
                    return Err("empty level bucket".into());
                }
                let mut live_seen = 0_u64;
                let mut visible_seen = 0_u64;
                for ch in &queue.channels {
                    if ch.live_count() > CHANNEL_CAPACITY {
                        return Err("channel over capacity".into());
                    }
                    let mut ch_live = 0_u64;
                    let mut ch_visible = 0_u64;
                    for &id in ch.slots.iter().filter(|&&i| i != 0) {
                        match self.orders.get(&id) {
                            None => return Err(format!("dangling channel slot {id}")),
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
                                if r.visible_lots == 0 || r.visible_lots > r.order.open_qty() {
                                    return Err(format!("bad visible slice for order {id}"));
                                }
                                ch_live += 1;
                                ch_visible = ch_visible.saturating_add(r.visible_lots);
                            }
                        }
                    }
                    if ch_live != u64::from(ch.live) {
                        return Err("channel live-count aggregate drifted".into());
                    }
                    if ch_visible != ch.visible_total {
                        return Err("channel visible aggregate drifted".into());
                    }
                    live_seen = live_seen.saturating_add(ch_live);
                    visible_seen = visible_seen.saturating_add(ch_visible);
                }
                if live_seen != queue.live_orders() {
                    return Err("level live-count aggregate drifted".into());
                }
                if visible_seen != queue.total_visible() {
                    return Err("level visible aggregate drifted".into());
                }
            }
        }
        // Every resting order must appear in exactly one channel slot.
        let mut slots_seen = std::collections::BTreeMap::new();
        for (levels, _) in [(&self.bids_inverted, Side::Bid), (&self.asks, Side::Ask)] {
            for queue in levels.values() {
                for ch in &queue.channels {
                    for &id in ch.slots.iter().filter(|&&i| i != 0) {
                        if slots_seen.insert(id, ()).is_some() {
                            return Err(format!("order {id} occupies two slots"));
                        }
                    }
                }
            }
        }
        if slots_seen.len() != self.orders.len() {
            return Err(format!(
                "orders without a slot: {} resting, {} slotted",
                self.orders.len(),
                slots_seen.len()
            ));
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
            display_lots: None,
            trailing_extreme_quote_minor: None,
            oco_group: None,
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

    #[test]
    fn iceberg_hides_and_reslices() {
        let mut b = LimitOrderBook::new("X".into());
        let mut iceberg = order(1, 10, Side::Ask, 100, 10);
        iceberg.display_lots = Some(3);
        b.insert_resting(iceberg, 100);
        // Depth shows the slice, not the total.
        let (_, asks) = b.depth(5);
        assert_eq!(asks[0].total_qty_lots, 3);
        assert_eq!(b.best_touch_sizes(), (0, 3));
        assert_eq!(b.get(1).map(|r| r.visible_lots), Some(3));
        // Matching sees only the visible slice.
        let taker = order(9, 99, Side::Bid, 100, 10);
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        assert_eq!(out.filled_lots(), 3);
        assert_eq!(out.taker_remaining_lots, 7);
        for f in &out.fills {
            b.apply_fill(f);
        }
        // Order still rests with 7 open; a fresh slice of 3 is visible.
        assert_eq!(b.get(1).map(|r| r.order.open_qty()), Some(7));
        assert_eq!(b.get(1).map(|r| r.visible_lots), Some(3));
        assert!(b.invariants_hold().is_ok());
        // Second slice...
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        assert_eq!(out.filled_lots(), 3);
        for f in &out.fills {
            b.apply_fill(f);
        }
        assert_eq!(b.get(1).map(|r| r.order.open_qty()), Some(4));
        // Third match consumes another slice of 3 (one slice per taker —
        // the pure-match design never trades hidden quantity by surprise).
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        assert_eq!(out.filled_lots(), 3);
        for f in &out.fills {
            b.apply_fill(f);
        }
        assert_eq!(b.get(1).map(|r| r.order.open_qty()), Some(1));
        // ...and the final revealed 1 prints in full.
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        assert_eq!(out.filled_lots(), 1);
        for f in &out.fills {
            b.apply_fill(f);
        }
        assert!(b.get(1).is_none(), "fully consumed iceberg leaves");
        assert!(b.invariants_hold().is_ok());
    }

    #[test]
    fn iceberg_reslice_requeues_at_back() {
        let mut b = LimitOrderBook::new("X".into());
        let mut front = order(1, 10, Side::Ask, 100, 5);
        front.display_lots = Some(2);
        let behind = order(2, 11, Side::Ask, 100, 5);
        b.insert_resting(front, 100);
        b.insert_resting(behind, 100);
        // Fill the front order's first slice fully.
        let taker = order(9, 99, Side::Bid, 100, 2);
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        for f in &out.fills {
            b.apply_fill(f);
        }
        // Order 1 re-queued behind order 2 at the same level.
        let (_, asks) = b.depth(1);
        assert_eq!(asks[0].order_count, 2);
        assert_eq!(asks[0].total_qty_lots, 2 + 5, "1's fresh slice + 2's size");
        // The next taker now hits order 2 first (queue priority lost).
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        assert_eq!(out.fills[0].maker_order_id, 2);
        assert!(b.invariants_hold().is_ok());
    }

    #[test]
    fn uncross_uniform_price_and_volume_max() {
        let mut b = LimitOrderBook::new("X".into());
        b.set_auction_mode(true);
        b.insert_resting(order(1, 10, Side::Bid, 105, 10), 105); // crossed!
        b.insert_resting(order(2, 11, Side::Bid, 101, 5), 101);
        b.insert_resting(order(3, 12, Side::Ask, 100, 6), 100);
        b.insert_resting(order(4, 13, Side::Ask, 104, 2), 104);
        b.insert_resting(order(5, 14, Side::Ask, 108, 5), 108);
        assert!(
            b.invariants_hold().is_ok(),
            "auction mode tolerates crossing"
        );
        let out = b.uncross();
        // Max volume: min(bids>=104 = 10, asks<=104 = 8) = 8 at P in
        // {104, 105}; midpoint of (105, 100) = 102 ties toward 104.
        assert_eq!(out.clearing_price_ticks, Some(104));
        assert_eq!(out.matched_lots, 8);
        assert!(out.fills.iter().all(|f| f.price_ticks == 104));
        // Price-time: the 105x10 bid pairs against 100x6 then 104x2.
        let qtys: Vec<u64> = out.fills.iter().map(|f| f.qty_lots).collect();
        assert_eq!(qtys, vec![6, 2]);
        assert_eq!(out.fills[0].maker_order_id, 1); // bid arrived first
        assert_eq!(out.fills[0].taker_order_id, 3); // later ask is taker
    }

    #[test]
    fn uncross_partial_fills_pair_across_sizes() {
        let mut b = LimitOrderBook::new("X".into());
        b.set_auction_mode(true);
        b.insert_resting(order(1, 10, Side::Bid, 100, 4), 100);
        b.insert_resting(order(2, 11, Side::Bid, 100, 6), 100);
        b.insert_resting(order(3, 12, Side::Ask, 100, 5), 100);
        b.insert_resting(order(4, 13, Side::Ask, 100, 5), 100);
        let out = b.uncross();
        assert_eq!(out.clearing_price_ticks, Some(100));
        assert_eq!(out.matched_lots, 10);
        let qtys: Vec<u64> = out.fills.iter().map(|f| f.qty_lots).collect();
        assert_eq!(qtys, vec![4, 1, 5]);
    }

    #[test]
    fn uncross_no_cross_no_print() {
        let mut b = LimitOrderBook::new("X".into());
        b.set_auction_mode(true);
        b.insert_resting(order(1, 10, Side::Bid, 99, 5), 99);
        b.insert_resting(order(2, 11, Side::Ask, 101, 5), 101);
        let out = b.uncross();
        assert_eq!(out.clearing_price_ticks, None);
        assert_eq!(out.matched_lots, 0);
        assert!(out.fills.is_empty());
    }

    #[test]
    fn continuous_mode_still_rejects_crossing() {
        let mut b = LimitOrderBook::new("X".into());
        b.insert_resting(order(1, 10, Side::Ask, 101, 5), 101);
        assert!(!b.insert_resting(order(2, 11, Side::Bid, 101, 5), 101));
        b.set_auction_mode(true);
        assert!(b.insert_resting(order(3, 12, Side::Bid, 102, 5), 102));
    }
}

#[cfg(test)]
mod channel_tests {
    //! zkLighter channel-layout tests: aggregate caching, tombstone
    //! cancels, bounded channel capacity, and lazy matching behavior.
    use super::*;

    fn order(id: OrderId, sub: SubaccountId, side: Side, price: u64, qty: u64) -> Order {
        use poc_core::{OrderType, TimeInForce};
        Order {
            id,
            subaccount: sub,
            symbol: "X".into(),
            side,
            order_type: OrderType::Limit,
            price_ticks: Some(price),
            qty_lots: qty,
            filled_lots: 0,
            tif: TimeInForce::Gtc,
            post_only: false,
            reduce_only: false,
            stp: SelfTradePrevention::CancelNewest,
            display_lots: None,
            trailing_extreme_quote_minor: None,
            oco_group: None,
            client_ts: 0,
            engine_ts: 0,
        }
    }

    /// Fill one price level with `n` orders.
    fn level_with(n: u64) -> LimitOrderBook {
        let mut b = LimitOrderBook::new("X".into());
        for i in 1..=n {
            b.insert_resting(order(i, i, Side::Ask, 100, 2), 100);
        }
        b
    }

    #[test]
    fn channels_pack_eight_deep() {
        let b = level_with(20);
        let queue = b.asks.get(&100).expect("level exists");
        // 20 orders pack into ceil(20/8) = 3 channels.
        assert_eq!(queue.channels.len(), 3);
        // Cached aggregates answer without scanning.
        assert_eq!(queue.total_visible(), 40);
        assert_eq!(queue.live_orders(), 20);
        assert!(b.invariants_hold().is_ok());
    }

    #[test]
    fn tombstone_cancel_keeps_aggregates_honest() {
        let mut b = level_with(10);
        assert!(b.cancel(4).is_some());
        let queue = b.asks.get(&100).expect("level remains");
        assert_eq!(queue.live_orders(), 9);
        assert_eq!(queue.total_visible(), 18);
        // FIFO order of the survivors is untouched.
        let ids: Vec<OrderId> = queue.iter_live().collect();
        assert_eq!(ids, vec![1, 2, 3, 5, 6, 7, 8, 9, 10]);
        assert!(b.invariants_hold().is_ok());
        // Canceling everything drops the level entirely.
        for i in [1_u64, 2, 3, 5, 6, 7, 8, 9, 10] {
            assert!(b.cancel(i).is_some());
        }
        assert!(b.best_ask().is_none());
        assert!(b.invariants_hold().is_ok());
    }

    #[test]
    fn touch_sizes_are_constant_time_and_correct() {
        let mut b = level_with(9);
        b.insert_resting(order(50, 9, Side::Bid, 99, 7), 99);
        // Best ask level aggregates 9 x 2.
        assert_eq!(b.best_touch_sizes(), (7, 18));
        b.cancel(5);
        assert_eq!(b.best_touch_sizes(), (7, 16));
        assert!(b.invariants_hold().is_ok());
    }

    #[test]
    fn lazy_match_stops_at_first_level() {
        // A taker that fills entirely at level 100 must not walk 101/102.
        let mut b = LimitOrderBook::new("X".into());
        b.insert_resting(order(1, 10, Side::Ask, 100, 5), 100);
        b.insert_resting(order(2, 11, Side::Ask, 101, 5), 101);
        b.insert_resting(order(3, 12, Side::Ask, 102, 5), 102);
        let taker = order(9, 99, Side::Bid, 100, 5);
        let out = b.match_taker(&taker, Some(102), false, SelfTradePrevention::CancelNewest);
        assert_eq!(out.fills.len(), 1);
        assert_eq!(out.fills[0].maker_order_id, 1);
        assert_eq!(out.fills[0].qty_lots, 5);
        assert_eq!(out.taker_remaining_lots, 0);
    }

    #[test]
    fn partial_fill_shrinks_aggregate_in_place() {
        let mut b = level_with(4);
        let taker = order(9, 99, Side::Bid, 100, 3);
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        for f in &out.fills {
            b.apply_fill(f);
        }
        let queue = b.asks.get(&100).expect("level remains");
        // 8 lots across 4 orders, 3 filled: order 1 fully filled and
        // canceled, order 2 half-filled. 5 visible, 3 live.
        assert_eq!(queue.total_visible(), 5);
        assert_eq!(queue.live_orders(), 3);
        let ids: Vec<OrderId> = queue.iter_live().collect();
        assert_eq!(
            ids,
            vec![2, 3, 4],
            "partial fills never requeue plain orders"
        );
        assert!(b.invariants_hold().is_ok());
    }

    #[test]
    fn depth_uses_cached_aggregates() {
        let mut b = LimitOrderBook::new("X".into());
        for i in 0..10_u64 {
            b.insert_resting(order(i + 1, i + 1, Side::Ask, 100 + i, 3), 100 + i);
        }
        let (_, asks) = b.depth(4);
        assert_eq!(asks.len(), 4);
        assert!(asks
            .iter()
            .all(|l| l.total_qty_lots == 3 && l.order_count == 1));
        assert_eq!(asks[0].price_ticks, 100);
        assert_eq!(asks[3].price_ticks, 103);
    }

    #[test]
    fn available_within_sums_level_aggregates() {
        let mut b = LimitOrderBook::new("X".into());
        for i in 0..5_u64 {
            b.insert_resting(order(i + 1, 1, Side::Ask, 100 + i, 10), 100 + i);
        }
        assert_eq!(b.available_within(Side::Bid, Some(102)), 30);
        assert_eq!(b.available_within(Side::Bid, None), 50);
        assert_eq!(b.available_within(Side::Bid, Some(99)), 0);
    }

    #[test]
    fn interleaved_cancel_fill_churn_keeps_channels_tight() {
        // Adversarial churn: cancel and fill across the channel chain,
        // verifying aggregates and FIFO order after every step.
        let mut b = level_with(16);
        let mut rng_state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut live: Vec<u64> = (1..=16).collect();
        for step in 0..40 {
            // xorshift64*
            rng_state ^= rng_state >> 12;
            rng_state ^= rng_state << 25;
            rng_state ^= rng_state >> 27;
            let r = rng_state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            if live.is_empty() {
                break;
            }
            let idx = (r >> 32) as usize % live.len();
            let victim = live[idx];
            if step % 3 == 0 {
                // Cancel one order.
                assert!(b.cancel(victim).is_some());
                live.remove(idx);
            } else {
                // Partially fill the head order (1 lot per step; two
                // consecutive fills close it and cancel it).
                let taker = order(900 + step, 99, Side::Bid, 100, 1);
                let out =
                    b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
                for f in &out.fills {
                    b.apply_fill(f);
                }
                live.retain(|&id| b.get(id).is_some());
            }
            // Resync both directions: the model must equal the book.
            live.retain(|&id| b.get(id).is_some());
            if live.is_empty() {
                break; // the level (correctly) dropped with its last order
            }
            let queue = b.asks.get(&100).expect("level alive while orders remain");
            assert_eq!(
                queue.live_orders(),
                live.len().try_into().unwrap(),
                "step {step}"
            );
            let expected: Vec<OrderId> = live.clone();
            assert_eq!(
                queue.iter_live().collect::<Vec<_>>(),
                expected,
                "step {step}"
            );
            assert!(b.invariants_hold().is_ok(), "step {step}");
        }
    }

    #[test]
    fn iceberg_requeue_moves_to_channel_back() {
        use poc_core::{OrderType, TimeInForce};
        let mut o = order(1, 10, Side::Ask, 100, 10);
        o.order_type = OrderType::Limit;
        o.tif = TimeInForce::Gtc;
        o.display_lots = Some(4);
        let mut b = LimitOrderBook::new("X".into());
        b.insert_resting(o, 100);
        for id in 2..=3_u64 {
            b.insert_resting(order(id, id, Side::Ask, 100, 2), 100);
        }
        // Queue starts [1, 2, 3] with order 1 displaying 4.
        let taker = order(9, 99, Side::Bid, 100, 4);
        let out = b.match_taker(&taker, Some(100), false, SelfTradePrevention::CancelNewest);
        for f in &out.fills {
            b.apply_fill(f);
        }
        // Order 1's slice is exhausted: it requeues behind 2 and 3 with a
        // fresh slice of min(6, 4) = 4.
        let queue = b.asks.get(&100).expect("level remains");
        let ids: Vec<OrderId> = queue.iter_live().collect();
        assert_eq!(ids, vec![2, 3, 1]);
        assert_eq!(queue.total_visible(), 2 + 2 + 4);
        assert!(b.invariants_hold().is_ok());
    }

    #[test]
    fn uncross_prefix_sums_match_naive_clearing() {
        // Randomized differential test: the prefix-sum clearing price must
        // agree with a naive candidate-scan implementation on a random
        // auction book.
        let mut rng_state: u64 = 0x1234_5678_9ABC_DEF0;
        let mut next = move || {
            rng_state ^= rng_state >> 12;
            rng_state ^= rng_state << 25;
            rng_state ^= rng_state >> 27;
            rng_state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for case in 0..25_u64 {
            let mut b = LimitOrderBook::new("X".into());
            b.set_auction_mode(true);
            let n = 4 + (next() % 8);
            for id in (1_u64..).take(n as usize) {
                let side = if next() & 1 == 0 {
                    Side::Bid
                } else {
                    Side::Ask
                };
                let price = 95 + (next() % 11);
                let qty = 1 + (next() % 5);
                b.insert_resting(order(id, id, side, price, qty), price);
            }
            let got = b.uncross();

            // Naive reference: candidate scan.
            let naive_volume = |p: u64| -> u64 {
                let bid: u64 = b
                    .resting_orders()
                    .filter(|r| r.order.side == Side::Bid)
                    .filter(|r| r.price_ticks >= p)
                    .map(|r| r.order.open_qty())
                    .sum();
                let ask: u64 = b
                    .resting_orders()
                    .filter(|r| r.order.side == Side::Ask)
                    .filter(|r| r.price_ticks <= p)
                    .map(|r| r.order.open_qty())
                    .sum();
                bid.min(ask)
            };
            let mid = {
                let (bb, ba) = b.bbo();
                match (bb, ba) {
                    (Some(x), Some(y)) => (x + y) / 2,
                    _ => 100,
                }
            };
            let mut level_prices: Vec<u64> = b.resting_orders().map(|r| r.price_ticks).collect();
            level_prices.sort_unstable();
            level_prices.dedup();
            let naive_best = level_prices
                .into_iter()
                .map(|p| (p, naive_volume(p)))
                .filter(|&(_, v)| v > 0)
                .min_by(|&(pa, va), &(pb, vb)| {
                    vb.cmp(&va)
                        .then(pa.abs_diff(mid).cmp(&pb.abs_diff(mid)))
                        .then(pa.cmp(&pb))
                });
            match naive_best {
                Some((p, v)) => {
                    assert_eq!(
                        got.clearing_price_ticks,
                        Some(p),
                        "case {case}: clearing price"
                    );
                    assert_eq!(got.matched_lots, v, "case {case}: volume");
                }
                None => {
                    assert_eq!(got.clearing_price_ticks, None, "case {case}: no cross");
                }
            }
        }
    }
}
