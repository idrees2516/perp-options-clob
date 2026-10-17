//! Read-side replica: the thread-safe snapshot of venue state that REST
//! queries and WS subscription snapshots read. Updated exclusively by the
//! shard workers from engine events (single writer per shard, RwLock for
//! readers) — never by the gateway threads directly, so the replica can
//! never disagree with the engine's journal-derived truth.
//!
//! Design notes:
//! * Symbol rows, underlier state, tape rings and the per-account order
//!   registry are all event-derived (`Ack`/`Trade`/`Canceled`/…).
//! * Balances and positions are NOT re-derived from events (that would
//!   duplicate settlement arithmetic — a correctness risk); the worker
//!   copies them straight out of the engine for accounts touched in each
//!   batch. Engine truth, zero duplicated math.
//! * Depth snapshots are refreshed at most every 100 ms per symbol by the
//!   worker (throttled fan-out), bounded to 10 levels.

use crate::engine::Phase;
use crate::symbol::Listing;
use crate::types::{AcctId, Price, SymbolId, UnderlyingId};
use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;

/// One tape row.
#[derive(Clone, Copy, Debug)]
pub struct TapeRow {
    /// Execution price.
    pub price: Price,
    /// Quantity.
    pub qty: u32,
    /// Aggressor: 0 buy, 1 sell, 2 auction.
    pub aggressor: u8,
    /// Engine nanos.
    pub ts: u64,
}

/// One listed symbol's replica row.
#[derive(Clone, Debug)]
pub struct SymbolRow {
    /// Global symbol id.
    pub symbol: SymbolId,
    /// Listing (underlying, expiry minute, kind, strike).
    pub listing: Listing,
    /// Trading phase.
    pub phase: Phase,
    /// Option mark (last trade).
    pub mark: Price,
    /// Session open.
    pub open: Price,
    /// Session high.
    pub high: Price,
    /// Session low.
    pub low: Price,
    /// Volume (contracts).
    pub vol: u64,
    /// Trades count.
    pub n_trades: u64,
    /// BBO: (bid, bid_qty, ask, ask_qty).
    pub bbo: (Price, u64, Price, u64),
    /// Top-10 depth (bids descending).
    pub depth_bids: Vec<(Price, u64)>,
    /// Top-10 depth (asks ascending).
    pub depth_asks: Vec<(Price, u64)>,
    /// Recent trades (most recent last), capped.
    pub tape: VecDeque<TapeRow>,
}

/// One underlier's state.
#[derive(Clone, Copy, Debug)]
pub struct UnderlyingRow {
    /// Underlying id.
    pub underlying: UnderlyingId,
    /// Last oracle mark.
    pub mark: Price,
    /// Session reference (first mark).
    pub reference: Price,
    /// Fast-market regime.
    pub fast: bool,
    /// Circuit-breaker halted.
    pub halted: bool,
}

/// One open (resting) order.
#[derive(Clone, Copy, Debug)]
pub struct OrderRow {
    /// Packed order id.
    pub order: u64,
    /// Global symbol.
    pub symbol: SymbolId,
    /// Owning account.
    pub account: AcctId,
    /// Side bit (false=buy).
    pub buy: bool,
    /// Limit price.
    pub price: Price,
    /// Original quantity.
    pub qty: u32,
    /// Remaining quantity.
    pub leaves: u32,
    /// TIF (0 gtc, 1 ioc, 2 fok).
    pub tif: u8,
    /// Kind (0 limit, 1 market).
    pub kind: u8,
    /// Post-only flag.
    pub post_only: bool,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Engine nanos at rest.
    pub ts: u64,
}

/// One position row.
#[derive(Clone, Copy, Debug)]
pub struct PositionRow {
    /// Global symbol.
    pub symbol: SymbolId,
    /// Signed quantity.
    pub qty: i32,
    /// Margin requirement (short options).
    pub margin: u64,
    /// Cached delta notional.
    pub delta_notional: i64,
}

/// Per-underlying balance snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct BalanceRow {
    /// Settled cash.
    pub cash: i128,
    /// Cash committed to resting buys.
    pub hold: i128,
    /// Margin required.
    pub margin_required: u64,
    /// Margin reserved for resting sells.
    pub margin_reserved: u64,
    /// Fees paid over the session.
    pub fees_paid: u64,
    /// Rebates received.
    pub rebates_recv: u64,
}

/// One account's replica state (across underlyings).
#[derive(Clone, Debug, Default)]
pub struct AccountRow {
    /// Balances per underlying.
    pub balances: HashMap<UnderlyingId, BalanceRow>,
    /// Non-flat positions (any underlying).
    pub positions: Vec<PositionRow>,
}

/// Per-shard operational metrics snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShardMetrics {
    /// Shard index.
    pub shard: u32,
    /// Underlying.
    pub underlying: UnderlyingId,
    /// Orders accepted.
    pub orders: u64,
    /// Orders rejected.
    pub rejects: u64,
    /// Trades.
    pub trades: u64,
    /// Volume.
    pub volume: u64,
    /// Fees collected.
    pub fees: u64,
    /// Cancels.
    pub cancels: u64,
    /// Commands.
    pub cmds: u64,
    /// Expiries settled.
    pub expiries: u64,
    /// Auctions cleared.
    pub auctions: u64,
    /// Pending queue depth (sampled).
    pub queue_depth: u32,
    /// Journal bytes written.
    pub journal_bytes: u64,
    /// State hash (hex would allocate; raw u64 here).
    pub state_hash: u64,
    /// Conservation holds.
    pub conservation: bool,
    /// Orders/sec EMA.
    pub tps_ema: f64,
}

/// The whole replica.
pub struct Replica {
    symbols: RwLock<HashMap<SymbolId, SymbolRow>>,
    underlyings: RwLock<HashMap<UnderlyingId, UnderlyingRow>>,
    accounts: RwLock<HashMap<AcctId, AccountRow>>,
    /// order id -> row (for cancel validation and open-order listings).
    orders: RwLock<HashMap<u64, OrderRow>>,
    shards: RwLock<Vec<ShardMetrics>>,
}

impl Default for Replica {
    fn default() -> Self {
        Self::new()
    }
}

impl Replica {
    /// An empty replica.
    pub fn new() -> Replica {
        Replica {
            symbols: RwLock::new(HashMap::new()),
            underlyings: RwLock::new(HashMap::new()),
            accounts: RwLock::new(HashMap::new()),
            orders: RwLock::new(HashMap::new()),
            shards: RwLock::new(Vec::new()),
        }
    }

    // ---------------- writers (shard workers only) ----------------

    /// Insert/refresh a symbol row (called on SymbolListed; preserves tape).
    pub fn upsert_symbol(&self, symbol: SymbolId, listing: Listing) {
        let mut g = self.symbols.write().unwrap();
        g.entry(symbol).or_insert_with(|| SymbolRow {
            symbol,
            listing,
            phase: Phase::Continuous,
            mark: 0,
            open: 0,
            high: 0,
            low: 0,
            vol: 0,
            n_trades: 0,
            bbo: (0, 0, 0, 0),
            depth_bids: Vec::new(),
            depth_asks: Vec::new(),
            tape: VecDeque::new(),
        });
    }

    /// Update a symbol's phase.
    pub fn set_phase(&self, symbol: SymbolId, phase: Phase) {
        if let Some(row) = self.symbols.write().unwrap().get_mut(&symbol) {
            row.phase = phase;
        }
    }

    /// Update a symbol's mark.
    pub fn set_mark(&self, symbol: SymbolId, price: Price) {
        if let Some(row) = self.symbols.write().unwrap().get_mut(&symbol) {
            row.mark = price;
            if row.open == 0 {
                row.open = price;
            }
            if price > row.high {
                row.high = price;
            }
            if row.low == 0 || price < row.low {
                row.low = price;
            }
        }
    }

    /// Update a symbol's BBO.
    pub fn set_bbo(&self, symbol: SymbolId, bbo: (Price, u64, Price, u64)) {
        if let Some(row) = self.symbols.write().unwrap().get_mut(&symbol) {
            row.bbo = bbo;
        }
    }

    /// Push a tape row.
    pub fn push_tape(&self, symbol: SymbolId, row: TapeRow) {
        if let Some(s) = self.symbols.write().unwrap().get_mut(&symbol) {
            s.tape.push_back(row);
            while s.tape.len() > 200 {
                s.tape.pop_front();
            }
            s.vol += row.qty as u64;
            s.n_trades += 1;
        }
    }

    /// Refresh a symbol's depth snapshot.
    pub fn set_depth(&self, symbol: SymbolId, bids: Vec<(Price, u64)>, asks: Vec<(Price, u64)>) {
        if let Some(row) = self.symbols.write().unwrap().get_mut(&symbol) {
            row.depth_bids = bids;
            row.depth_asks = asks;
        }
    }

    /// Upsert an underlier row.
    pub fn set_underlying(&self, row: UnderlyingRow) {
        let mut g = self.underlyings.write().unwrap();
        let e = g.entry(row.underlying).or_insert(row);
        if row.mark > 0 {
            e.mark = row.mark;
            if e.reference == 0 {
                e.reference = row.mark;
            }
        }
        e.fast = row.fast;
        e.halted = row.halted;
    }

    /// Register a resting order (Ack with leaves > 0).
    pub fn order_rest(&self, row: OrderRow) {
        self.orders.write().unwrap().insert(row.order, row);
    }

    /// Remove a resting order.
    pub fn order_gone(&self, order: u64) {
        self.orders.write().unwrap().remove(&order);
    }

    /// Decrement a resting order's leaves by a fill quantity.
    pub fn order_fill(&self, order: u64, qty: u32) {
        let mut g = self.orders.write().unwrap();
        if let Some(row) = g.get_mut(&order) {
            row.leaves = row.leaves.saturating_sub(qty);
            if row.leaves == 0 {
                g.remove(&order);
            }
        }
    }

    /// Snapshot an account's balances + positions (engine truth). The caller
    /// (shard worker `shard_idx`) provides the rows for its own shard only;
    /// positions belonging to other shards are preserved.
    pub fn snapshot_account(
        &self,
        account: AcctId,
        underlying: UnderlyingId,
        shard_idx: u32,
        balance: BalanceRow,
        positions: Vec<PositionRow>,
    ) {
        let mut g = self.accounts.write().unwrap();
        let row = g.entry(account).or_default();
        row.balances.insert(underlying, balance);
        // Replace this shard's positions, keep other shards' rows.
        row.positions
            .retain(|p| crate::platform::symbol_shard(p.symbol) != shard_idx);
        row.positions.extend(positions);
    }

    /// Replace the full shard-metrics vector.
    pub fn set_shard_metrics(&self, m: Vec<ShardMetrics>) {
        *self.shards.write().unwrap() = m;
    }

    // ---------------- readers (gateway threads) ----------------

    /// All symbol rows (sorted by symbol id).
    pub fn symbols(&self) -> Vec<SymbolRow> {
        let g = self.symbols.read().unwrap();
        let mut v: Vec<SymbolRow> = g.values().cloned().collect();
        v.sort_by_key(|s| s.symbol);
        v
    }

    /// One symbol row.
    pub fn symbol(&self, symbol: SymbolId) -> Option<SymbolRow> {
        self.symbols.read().unwrap().get(&symbol).cloned()
    }

    /// Underlier rows sorted by id.
    pub fn underlyings(&self) -> Vec<UnderlyingRow> {
        let g = self.underlyings.read().unwrap();
        let mut v: Vec<UnderlyingRow> = g.values().copied().collect();
        v.sort_by_key(|u| u.underlying);
        v
    }

    /// One underlier.
    pub fn underlying(&self, u: UnderlyingId) -> Option<UnderlyingRow> {
        self.underlyings.read().unwrap().get(&u).copied()
    }

    /// One account's row (clone).
    pub fn account(&self, account: AcctId) -> AccountRow {
        self.accounts
            .read()
            .unwrap()
            .get(&account)
            .cloned()
            .unwrap_or_default()
    }

    /// Open orders for an account (sorted by id).
    pub fn open_orders(&self, account: AcctId) -> Vec<OrderRow> {
        let g = self.orders.read().unwrap();
        let mut v: Vec<OrderRow> = g.values().filter(|o| o.account == account).copied().collect();
        v.sort_by_key(|o| o.order);
        v
    }

    /// Look up a specific order (ownership checks at the edge).
    pub fn order(&self, order: u64) -> Option<OrderRow> {
        self.orders.read().unwrap().get(&order).copied()
    }

    /// Shard metrics.
    pub fn shard_metrics(&self) -> Vec<ShardMetrics> {
        self.shards.read().unwrap().clone()
    }

    /// Global conservation across shards.
    pub fn conservation(&self) -> bool {
        self.shards.read().unwrap().iter().all(|m| m.conservation)
    }
}

/// Convert a Phase to its wire string.
pub fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Continuous => "continuous",
        Phase::AuctionOpen => "auction",
        Phase::Expired => "expired",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::OptKind;

    fn listing() -> Listing {
        Listing {
            underlying: 0,
            expiry_min: 30_000_000,
            kind: OptKind::Call,
            strike: 500_000,
        }
    }

    #[test]
    fn symbol_lifecycle() {
        let r = Replica::new();
        r.upsert_symbol(1, listing());
        r.set_mark(1, 250);
        r.set_mark(1, 300);
        r.set_mark(1, 200);
        let s = r.symbol(1).unwrap();
        assert_eq!(s.open, 250);
        assert_eq!(s.high, 300);
        assert_eq!(s.low, 200);
        r.set_bbo(1, (240, 10, 260, 8));
        assert_eq!(r.symbol(1).unwrap().bbo, (240, 10, 260, 8));
        for i in 0..250 {
            r.push_tape(
                1,
                TapeRow {
                    price: 200 + i % 50,
                    qty: 1,
                    aggressor: 0,
                    ts: i,
                },
            );
        }
        assert_eq!(r.symbol(1).unwrap().tape.len(), 200);
        assert_eq!(r.symbol(1).unwrap().vol, 250);
        r.set_phase(1, Phase::AuctionOpen);
        assert_eq!(phase_name(r.symbol(1).unwrap().phase), "auction");
    }

    #[test]
    fn order_registry_flow() {
        let r = Replica::new();
        r.order_rest(OrderRow {
            order: 7,
            symbol: 1,
            account: 3,
            buy: true,
            price: 250,
            qty: 10,
            leaves: 10,
            tif: 0,
            kind: 0,
            post_only: false,
            reduce_only: false,
            ts: 1,
        });
        r.order_fill(7, 4);
        assert_eq!(r.order(7).unwrap().leaves, 6);
        r.order_fill(7, 6);
        assert!(r.order(7).is_none());
        assert!(r.open_orders(3).is_empty());
    }

    #[test]
    fn account_snapshots() {
        let r = Replica::new();
        r.snapshot_account(
            3,
            0,
            0,
            BalanceRow {
                cash: 1_000,
                ..Default::default()
            },
            vec![PositionRow {
                symbol: 1,
                qty: 5,
                margin: 0,
                delta_notional: 0,
            }],
        );
        let a = r.account(3);
        assert_eq!(a.balances.get(&0).unwrap().cash, 1_000);
        assert_eq!(a.positions.len(), 1);
        r.snapshot_account(3, 0, 0, BalanceRow { cash: 900, ..Default::default() }, vec![]);
        let a2 = r.account(3);
        assert_eq!(a2.balances.get(&0).unwrap().cash, 900);
        assert_eq!(a2.positions.len(), 0);
        // A different shard's position survives a shard-0 snapshot.
        r.snapshot_account(
            3,
            0,
            0,
            BalanceRow::default(),
            vec![PositionRow {
                symbol: crate::platform::global_symbol(1, 5),
                qty: -2,
                margin: 100,
                delta_notional: 0,
            }],
        );
        r.snapshot_account(3, 0, 0, BalanceRow::default(), vec![]);
        assert_eq!(r.account(3).positions.len(), 1);
    }

    #[test]
    fn underlying_state() {
        let r = Replica::new();
        r.set_underlying(UnderlyingRow {
            underlying: 0,
            mark: 500_000,
            reference: 0,
            fast: false,
            halted: false,
        });
        r.set_underlying(UnderlyingRow {
            underlying: 0,
            mark: 510_000,
            reference: 0,
            fast: true,
            halted: false,
        });
        let u = r.underlying(0).unwrap();
        assert_eq!(u.mark, 510_000);
        assert_eq!(u.reference, 500_000);
        assert!(u.fast);
    }

    #[test]
    fn shard_metrics_conservation() {
        let r = Replica::new();
        r.set_shard_metrics(vec![
            ShardMetrics {
                shard: 0,
                conservation: true,
                ..Default::default()
            },
        ]);
        assert!(r.conservation());
        r.set_shard_metrics(vec![
            ShardMetrics {
                shard: 0,
                conservation: true,
                ..Default::default()
            },
            ShardMetrics {
                shard: 1,
                conservation: false,
                ..Default::default()
            },
        ]);
        assert!(!r.conservation());
    }
}
