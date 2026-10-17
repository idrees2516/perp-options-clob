//! Price-time priority central limit order book.
//!
//! Design goals: O(1) cancel, O(1) best-quote read, allocation-free steady
//! state, one cache line per order.
//!
//! * Orders live in a slab (`Vec<Order>`, 64 bytes each) with an intrusive
//!   free-list; each order links `prev`/`next` inside its price level and
//!   remembers its level, so cancel is O(1) pointer surgery.
//! * Public order ids are `(slot, uid)` packed into a `u64` — slot recycling
//!   can never be confused with a live order because the generation differs.
//! * Levels live in a slab of intrusive doubly-linked chains, one per side.
//!   The chain head is always the **best** level (highest bid / lowest ask)
//!   and `next` walks toward worse prices — identical code both sides.
//! * Quantities aggregate per level, so a taker sweep touches one cache line
//!   per price level regardless of resting order count.
//! * `pub(crate)` fields let the engine run the match loop with zero
//!   indirection; the public API wraps the same operations for tests.

use crate::types::{id_slot, id_uid, pack_id, Kind, OrderId, Price, Qty, Side, Tif};

/// Sentinel slab index.
pub const NIL: u32 = u32::MAX;

/// A resting order. 64 bytes — exactly one cache line.
#[derive(Clone, Copy, Debug)]
pub struct Order {
    /// Unique insertion generation (session monotonic).
    pub uid: u32,
    /// Owning account.
    pub account: u32,
    /// Limit (or protection) price in ticks.
    pub price: Price,
    /// Original quantity.
    pub qty: Qty,
    /// Remaining quantity.
    pub leaves: Qty,
    /// Previous order in level queue.
    pub prev: u32,
    /// Next order in level queue.
    pub next: u32,
    /// Price level this order resides in.
    pub level: u32,
    /// Position slot for (account, symbol) — resolved at admission.
    pub pos_slot: u32,
    /// OCO partner slot (or [`NIL`]).
    pub oco: u32,
    /// OCO partner generation guard.
    pub oco_uid: u32,
    /// Per-contract commitment (cash hold for buys, margin hold for sells),
    /// reserved at admission and released exactly per filled/cancelled qty.
    pub commit_pc: u64,
    /// Symbol this order trades.
    pub symbol: u32,
    /// Side bit.
    pub side: Side,
    /// Behaviour flags.
    pub flags: u8,
    /// Time in force.
    pub tif: Tif,
    /// Limit or market.
    pub kind: Kind,
}

impl Order {
    // NOTE: there is deliberately no `oid()` helper — an order's public id is
    // `pack_id(slot, uid)` and the slot is the Vec index, which the struct
    // itself does not know. Callers must use the ids returned by insert /
    // reduce_head / the level queue head, never a reconstructed one.
}

/// A price level: aggregated quantity plus an intrusive FIFO of orders.
#[derive(Clone, Copy, Debug)]
pub struct Level {
    /// Level price.
    pub price: Price,
    /// Sum of resting leaves.
    pub total_qty: u64,
    /// First order in arrival order.
    pub head: u32,
    /// Last order in arrival order.
    pub tail: u32,
    /// Toward better prices; `NIL` at the head (best) level.
    pub prev: u32,
    /// Toward worse prices; `NIL` at the tail (worst) level.
    pub next: u32,
    /// Number of resting orders.
    pub n_orders: u32,
}

/// Per-side book state.
#[derive(Clone, Copy, Debug)]
pub struct SideBook {
    /// Best level slab index (chain head), or [`NIL`] when empty.
    pub best: u32,
    /// Number of price levels.
    pub n_levels: u32,
    /// Total resting quantity across levels.
    pub total_qty: u64,
}

impl SideBook {
    const EMPTY: SideBook = SideBook {
        best: NIL,
        n_levels: 0,
        total_qty: 0,
    };
}

/// The limit order book for one symbol.
#[derive(Clone, Debug)]
pub struct Book {
    /// Order slab (index 0 reserved as sentinel).
    pub(crate) orders: Vec<Order>,
    /// Order free-list head (links via `next`).
    pub(crate) order_free: u32,
    /// Level slab (index 0 reserved as sentinel).
    pub(crate) levels: Vec<Level>,
    /// Level free-list head (links via `next`).
    pub(crate) level_free: u32,
    /// [bid, ask].
    pub(crate) sides: [SideBook; 2],
    /// Insertion generation counter.
    pub(crate) uid_ctr: u32,
    /// Live resting order count.
    pub n_orders: u32,
}

impl Book {
    /// A book with capacity pre-reserved (avoids steady-state growth).
    pub fn with_capacity(order_cap: usize, level_cap: usize) -> Book {
        let mut orders = Vec::with_capacity(order_cap + 1);
        orders.push(Order {
            uid: 0,
            account: 0,
            price: 0,
            qty: 0,
            leaves: 0,
            prev: NIL,
            next: NIL,
            level: NIL,
            pos_slot: NIL,
            oco: NIL,
            oco_uid: 0,
            commit_pc: 0,
            symbol: 0,
            side: Side::Buy,
            flags: 0,
            tif: Tif::Gtc,
            kind: Kind::Limit,
        });
        let mut levels = Vec::with_capacity(level_cap + 1);
        levels.push(Level {
            price: 0,
            total_qty: 0,
            head: NIL,
            tail: NIL,
            prev: NIL,
            next: NIL,
            n_orders: 0,
        });
        Book {
            orders,
            order_free: NIL,
            levels,
            level_free: NIL,
            sides: [SideBook::EMPTY; 2],
            uid_ctr: 0,
            n_orders: 0,
        }
    }

    /// Number of live orders.
    #[inline(always)]
    pub fn len(&self) -> u32 {
        self.n_orders
    }

    /// Is the book empty?
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.n_orders == 0
    }

    /// Best bid (price, qty) if any.
    #[inline(always)]
    pub fn best_bid(&self) -> Option<(Price, u64)> {
        let s = &self.sides[0];
        if s.best == NIL {
            None
        } else {
            let l = &self.levels[s.best as usize];
            Some((l.price, l.total_qty))
        }
    }

    /// Best ask (price, qty) if any.
    #[inline(always)]
    pub fn best_ask(&self) -> Option<(Price, u64)> {
        let s = &self.sides[1];
        if s.best == NIL {
            None
        } else {
            let l = &self.levels[s.best as usize];
            Some((l.price, l.total_qty))
        }
    }

    /// Access a resting order by slot.
    #[inline(always)]
    pub fn order(&self, slot: u32) -> &Order {
        &self.orders[slot as usize]
    }

    /// Best level price on a side (0 if empty).
    #[inline(always)]
    pub(crate) fn best_price(&self, side: Side) -> Price {
        let s = &self.sides[side.idx()];
        if s.best == NIL {
            0
        } else {
            self.levels[s.best as usize].price
        }
    }

    /// Best level index of a side, or [`NIL`].
    #[inline(always)]
    pub(crate) fn best_level(&self, side: Side) -> u32 {
        self.sides[side.idx()].best
    }

    /// Insert an order; returns its public (packed) id. Appends to the tail
    /// of its price level (FIFO time priority).
    pub fn insert(&mut self, mut o: Order) -> OrderId {
        debug_assert!(o.qty > 0);
        // 1. Allocate the order slot.
        let slot = if self.order_free != NIL {
            let s = self.order_free;
            self.order_free = self.orders[s as usize].next;
            s
        } else {
            let s = self.orders.len() as u32;
            self.orders.push(o);
            s
        };
        self.uid_ctr = self.uid_ctr.wrapping_add(1);
        o.uid = self.uid_ctr;
        o.prev = NIL;
        o.next = NIL;
        // 2. Find or create its level.
        let lvl = self.find_or_create_level(o.side, o.price);
        o.level = lvl;
        self.orders[slot as usize] = o;
        // 3. Append to level tail.
        let li = lvl as usize;
        let tail = self.levels[li].tail;
        if tail != NIL {
            self.orders[tail as usize].next = slot;
        } else {
            self.levels[li].head = slot;
        }
        self.orders[slot as usize].prev = tail;
        self.levels[li].tail = slot;
        self.levels[li].n_orders += 1;
        self.levels[li].total_qty += o.leaves as u64;
        self.sides[o.side.idx()].total_qty += o.leaves as u64;
        self.n_orders += 1;
        pack_id(slot, o.uid)
    }

    /// Cancel by public (packed) id. Returns the order copy if the id
    /// targeted a live order (slot + generation match).
    pub fn cancel_by_id(&mut self, id: OrderId) -> Option<Order> {
        let slot = id_slot(id);
        let uid = id_uid(id);
        if slot == NIL || slot as usize >= self.orders.len() {
            return None;
        }
        let o = self.orders[slot as usize];
        if o.uid != uid || o.level == NIL {
            return None;
        }
        Some(self.cancel(slot))
    }

    /// Cancel (fully remove) an order by slot; returns a copy of it.
    pub fn cancel(&mut self, slot: u32) -> Order {
        let o = self.orders[slot as usize];
        let li = o.level as usize;
        // Unlink from order queue.
        if o.prev != NIL {
            self.orders[o.prev as usize].next = o.next;
        } else {
            self.levels[li].head = o.next;
        }
        if o.next != NIL {
            self.orders[o.next as usize].prev = o.prev;
        } else {
            self.levels[li].tail = o.prev;
        }
        self.levels[li].total_qty -= o.leaves as u64;
        self.levels[li].n_orders -= 1;
        self.sides[o.side.idx()].total_qty -= o.leaves as u64;
        if self.levels[li].n_orders == 0 {
            self.unlink_level(o.side, li as u32);
        }
        // Recycle the order slot.
        self.orders[slot as usize].next = self.order_free;
        self.orders[slot as usize].leaves = 0;
        self.orders[slot as usize].level = NIL;
        self.order_free = slot;
        self.n_orders -= 1;
        o
    }

    /// Reduce the head order of a side's best level by `q`; removes it when
    /// exhausted. Returns (order copy, level price, slot).
    #[inline]
    pub(crate) fn reduce_head(&mut self, side: Side, q: Qty) -> (Order, Price, u32) {
        let li = self.sides[side.idx()].best as usize;
        debug_assert!(li != NIL as usize);
        let head = self.levels[li].head;
        let o = self.orders[head as usize];
        debug_assert!(o.leaves >= q);
        let price = self.levels[li].price;
        if o.leaves == q {
            self.cancel(head);
        } else {
            self.orders[head as usize].leaves -= q;
            self.levels[li].total_qty -= q as u64;
            self.sides[side.idx()].total_qty -= q as u64;
        }
        (o, price, head)
    }

    /// Aggregate depth from best, up to `n` levels, into `out`.
    /// Returns the number of levels written.
    pub fn depth(&self, side: Side, n: usize, out: &mut [(Price, u64)]) -> usize {
        let mut lvl = self.sides[side.idx()].best;
        let mut i = 0;
        while lvl != NIL && i < n && i < out.len() {
            let l = &self.levels[lvl as usize];
            out[i] = (l.price, l.total_qty);
            lvl = l.next;
            i += 1;
        }
        i
    }

    /// Total resting quantity on a side.
    #[inline(always)]
    pub fn side_qty(&self, side: Side) -> u64 {
        self.sides[side.idx()].total_qty
    }

    /// Cancel every resting order, pushing **copies** of them (with their
    /// pre-cancel leaves) paired with their slots, in (side, level, FIFO)
    /// order — deterministic for replay hashing and exact commitment
    /// release. Returns the count.
    pub fn drain_all(&mut self, out: &mut Vec<(Order, u32)>) -> usize {
        let mut n = 0;
        for side in [Side::Buy, Side::Sell] {
            let mut lvl = self.sides[side.idx()].best;
            while lvl != NIL {
                let head = self.levels[lvl as usize].head;
                let next_lvl = self.levels[lvl as usize].next;
                let mut oid = head;
                while oid != NIL {
                    let o = self.orders[oid as usize];
                    let nx = o.next;
                    out.push((o, oid));
                    self.cancel(oid);
                    n += 1;
                    oid = nx;
                }
                lvl = next_lvl;
            }
        }
        n
    }

    /// Cumulative quantity available on `side` within `limit` price
    /// (for FOK pre-scans). Buy: asks priced <= limit. Sell: bids >= limit.
    pub fn cum_within(&self, side: Side, limit: Price) -> u64 {
        // Walk the *opposite* book from best while crossing.
        let opp = side.opp();
        let mut lvl = self.sides[opp.idx()].best;
        let mut cum = 0u64;
        while lvl != NIL {
            let l = &self.levels[lvl as usize];
            let crosses = match side {
                Side::Buy => l.price <= limit,
                Side::Sell => l.price >= limit,
            };
            if !crosses {
                break;
            }
            cum += l.total_qty;
            lvl = l.next;
        }
        cum
    }

    /// Find or create the level for (side, price). The chain is ordered
    /// best-first; `next` moves toward worse prices on both sides.
    #[inline]
    fn find_or_create_level(&mut self, side: Side, price: Price) -> u32 {
        let mut prev = NIL;
        let mut cur = self.sides[side.idx()].best;
        match side {
            Side::Buy => {
                while cur != NIL && self.levels[cur as usize].price > price {
                    prev = cur;
                    cur = self.levels[cur as usize].next;
                }
            }
            Side::Sell => {
                while cur != NIL && self.levels[cur as usize].price < price {
                    prev = cur;
                    cur = self.levels[cur as usize].next;
                }
            }
        }
        if cur != NIL && self.levels[cur as usize].price == price {
            return cur;
        }
        // New level sits between `prev` and `cur`.
        let slot = self.alloc_level();
        let li = slot as usize;
        self.levels[li] = Level {
            price,
            total_qty: 0,
            head: NIL,
            tail: NIL,
            prev,
            next: cur,
            n_orders: 0,
        };
        if prev != NIL {
            self.levels[prev as usize].next = slot;
        } else {
            // Inserted at the head of the chain: it is the new best.
            self.sides[side.idx()].best = slot;
        }
        if cur != NIL {
            self.levels[cur as usize].prev = slot;
        }
        self.sides[side.idx()].n_levels += 1;
        slot
    }

    fn alloc_level(&mut self) -> u32 {
        if self.level_free != NIL {
            let slot = self.level_free;
            self.level_free = self.levels[slot as usize].next;
            slot
        } else {
            let slot = self.levels.len() as u32;
            self.levels.push(Level {
                price: 0,
                total_qty: 0,
                head: NIL,
                tail: NIL,
                prev: NIL,
                next: NIL,
                n_orders: 0,
            });
            slot
        }
    }

    /// Unlink an empty level from its side chain and recycle it.
    fn unlink_level(&mut self, side: Side, lvl: u32) {
        let l = self.levels[lvl as usize];
        debug_assert!(l.n_orders == 0 && l.head == NIL);
        if l.prev != NIL {
            self.levels[l.prev as usize].next = l.next;
        }
        if l.next != NIL {
            self.levels[l.next as usize].prev = l.prev;
        }
        let s = &mut self.sides[side.idx()];
        if s.best == lvl {
            s.best = l.next; // toward worse = next best
        }
        s.n_levels -= 1;
        self.levels[lvl as usize].prev = NIL;
        self.levels[lvl as usize].next = self.level_free;
        self.level_free = lvl;
    }
}

/// Convenience constructor of an order for insertion.
#[allow(clippy::too_many_arguments)]
pub fn make_order(
    account: u32,
    symbol: u32,
    side: Side,
    price: Price,
    qty: Qty,
    tif: Tif,
    kind: Kind,
    flags: u8,
    pos_slot: u32,
    oco: u32,
    oco_uid: u32,
    commit_pc: u64,
) -> Order {
    Order {
        uid: 0,
        account,
        price,
        qty,
        leaves: qty,
        prev: NIL,
        next: NIL,
        level: NIL,
        pos_slot,
        oco,
        oco_uid,
        commit_pc,
        symbol,
        side,
        flags,
        tif,
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ins(book: &mut Book, side: Side, price: Price, qty: Qty) -> OrderId {
        book.insert(make_order(
            1, 0, side, price, qty, Tif::Gtc, Kind::Limit, 0, NIL, NIL, 0, 0,
        ))
    }

    #[test]
    fn insert_and_best() {
        let mut b = Book::with_capacity(64, 16);
        ins(&mut b, Side::Buy, 100, 5);
        ins(&mut b, Side::Buy, 101, 7);
        ins(&mut b, Side::Buy, 99, 3);
        assert_eq!(b.best_bid(), Some((101, 7)));
        assert_eq!(b.side_qty(Side::Buy), 15);
        ins(&mut b, Side::Sell, 105, 4);
        ins(&mut b, Side::Sell, 103, 6);
        assert_eq!(b.best_ask(), Some((103, 6)));
        assert_eq!(b.side_qty(Side::Sell), 10);
    }

    #[test]
    fn cancel_updates_best() {
        let mut b = Book::with_capacity(64, 16);
        let a = ins(&mut b, Side::Buy, 101, 7);
        let c = ins(&mut b, Side::Buy, 100, 3);
        assert_eq!(b.best_bid(), Some((101, 7)));
        b.cancel_by_id(a);
        assert_eq!(b.best_bid(), Some((100, 3)));
        b.cancel_by_id(c);
        assert_eq!(b.best_bid(), None);
        assert_eq!(b.n_orders, 0);
        assert_eq!(b.sides[0].n_levels, 0);
    }

    #[test]
    fn stale_id_cannot_hit_recycled_slot() {
        let mut b = Book::with_capacity(64, 16);
        let a = ins(&mut b, Side::Buy, 100, 5);
        b.cancel_by_id(a);
        let c = ins(&mut b, Side::Buy, 100, 5); // reuses slot, new uid
        assert_eq!(id_slot(a), id_slot(c));
        assert_ne!(id_uid(a), id_uid(c));
        assert!(b.cancel_by_id(a).is_none(), "stale id must fail");
        assert!(b.cancel_by_id(c).is_some());
    }

    #[test]
    fn fifo_within_level() {
        let mut b = Book::with_capacity(64, 16);
        let a = id_slot(ins(&mut b, Side::Buy, 100, 5));
        let c = id_slot(ins(&mut b, Side::Buy, 100, 5));
        let lvl = b.orders[a as usize].level;
        assert_eq!(b.levels[lvl as usize].head, a);
        assert_eq!(b.levels[lvl as usize].tail, c);
        b.cancel(a);
        assert_eq!(b.levels[lvl as usize].head, c);
        assert_eq!(b.levels[lvl as usize].tail, c);
    }

    #[test]
    fn partial_and_full_head_fill() {
        let mut b = Book::with_capacity(64, 16);
        ins(&mut b, Side::Sell, 105, 10);
        // Consume the SELL side's best level (an incoming buy aggressor).
        let (o, px, _) = b.reduce_head(Side::Sell, 4);
        assert_eq!((px, o.leaves), (105, 10));
        assert_eq!(b.best_ask(), Some((105, 6)));
        let (_, px2, _) = b.reduce_head(Side::Sell, 6);
        assert_eq!(px2, 105);
        assert_eq!(b.best_ask(), None);
        assert_eq!(b.n_orders, 0);
    }

    #[test]
    fn cum_within_fok_scan() {
        let mut b = Book::with_capacity(64, 16);
        ins(&mut b, Side::Sell, 103, 6);
        ins(&mut b, Side::Sell, 104, 4);
        ins(&mut b, Side::Sell, 106, 5);
        assert_eq!(b.cum_within(Side::Buy, 104), 10);
        assert_eq!(b.cum_within(Side::Buy, 102), 0);
        assert_eq!(b.cum_within(Side::Buy, 999), 15);
    }

    #[test]
    fn depth_walk() {
        let mut b = Book::with_capacity(64, 16);
        ins(&mut b, Side::Buy, 100, 5);
        ins(&mut b, Side::Buy, 102, 7);
        ins(&mut b, Side::Buy, 101, 3);
        let mut out = [(0u64, 0u64); 8];
        let n = b.depth(Side::Buy, 8, &mut out);
        assert_eq!(n, 3);
        assert_eq!(out[0], (102, 7));
        assert_eq!(out[1], (101, 3));
        assert_eq!(out[2], (100, 5));
    }

    #[test]
    fn slot_reuse() {
        let mut b = Book::with_capacity(64, 16);
        let a = ins(&mut b, Side::Buy, 100, 5);
        b.cancel_by_id(a);
        let c = ins(&mut b, Side::Buy, 100, 5);
        assert_eq!(id_slot(a), id_slot(c), "free slot must be reused");
        assert_eq!(b.orders.len(), 2);
    }

    #[test]
    fn drain_all_is_deterministic() {
        let mut b = Book::with_capacity(128, 32);
        ins(&mut b, Side::Buy, 100, 5);
        ins(&mut b, Side::Buy, 101, 5);
        ins(&mut b, Side::Sell, 103, 5);
        let mut orders = Vec::new();
        let n = b.drain_all(&mut orders);
        assert_eq!(n, 3);
        assert_eq!(orders.len(), 3);
        // Copies retain pre-cancel leaves for exact release.
        assert!(orders.iter().all(|(o, _)| o.leaves == 5));
        assert_eq!(b.n_orders, 0);
        assert_eq!(b.best_bid(), None);
        assert_eq!(b.best_ask(), None);
    }
}
