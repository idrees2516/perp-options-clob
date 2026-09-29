//! Book-state commitments — provable order book for external verification.
//!
//! The account merkle tree proves *balances*; this module proves the
//! **matching state**: a domain-separated SHA-256 hash chain over every
//! resting order, canonicalized by order id. Publishing
//! [`BookCommitment::root`] alongside each settlement batch gives an
//! external verifier (a zk prover, a watchdog, a market-data auditor)
//! everything needed to check that the book it is shown matches the
//! book the engine actually holds — the property zkLighter's rollup
//! order book is designed around, available here for any verifier
//! without any circuit.
//!
//! ## What it pins down
//!
//! * which orders rest (id, owner, side, symbol);
//! * at what price and quantity (limit, open, and displayed iceberg
//!   quantity);
//! * nothing else — the commitment is canonical and order-stable, so two
//!   engines that replayed the same journal produce the identical root
//!   (bit for bit), and any divergence in matching outcome changes it.
//!
//! ## Determinism contract
//!
//! `BookCommitment::capture` iterates instruments in symbol order and
//! resting orders in id order — both canonical `BTreeMap` orders — so the
//! commitment is a pure function of engine state, reproducible by replay.

use poc_engine::Engine;

use crate::hash::sha256;

/// Domain separator for order-book commitment leaves.
const LEAF_DOMAIN: &[u8] = b"POC-BOOK-LEAF";
/// Domain separator for the chain links.
const LINK_DOMAIN: &[u8] = b"POC-BOOK-LINK";

/// One resting order's committed projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookEntry {
    /// Instrument symbol.
    pub symbol: String,
    /// Order id.
    pub order_id: u64,
    /// Owner.
    pub subaccount: u64,
    /// `true` = bid.
    pub is_bid: bool,
    /// Resting limit price in ticks.
    pub price_ticks: u64,
    /// Open (unfilled) quantity in lots.
    pub open_lots: u64,
    /// Displayed quantity (iceberg slice; equals open when fully shown).
    pub visible_lots: u64,
}

impl BookEntry {
    /// Domain-separated leaf encoding.
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(96);
        buf.extend_from_slice(LEAF_DOMAIN);
        buf.extend_from_slice(&(self.symbol.len() as u32).to_be_bytes());
        buf.extend_from_slice(self.symbol.as_bytes());
        buf.extend_from_slice(&self.order_id.to_be_bytes());
        buf.extend_from_slice(&self.subaccount.to_be_bytes());
        buf.push(u8::from(self.is_bid));
        buf.extend_from_slice(&self.price_ticks.to_be_bytes());
        buf.extend_from_slice(&self.open_lots.to_be_bytes());
        buf.extend_from_slice(&self.visible_lots.to_be_bytes());
        buf
    }
}

/// A captured, hash-chained commitment over one engine's resting orders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookCommitment {
    /// Canonical entries (symbol order, then id order).
    pub entries: Vec<BookEntry>,
    /// SHA-256 chain root over the entries.
    pub root: [u8; 32],
    /// Number of committed orders.
    pub count: usize,
}

impl BookCommitment {
    /// Capture and commit the full resting-order state of an engine.
    #[must_use]
    pub fn capture(engine: &Engine) -> Self {
        let mut entries = Vec::new();
        for_each_book(engine, |symbol, book| {
            for resting in book.resting_orders() {
                let order = &resting.order;
                entries.push(BookEntry {
                    symbol: symbol.to_owned(),
                    order_id: order.id,
                    subaccount: order.subaccount,
                    is_bid: order.side == poc_core::Side::Bid,
                    price_ticks: resting.price_ticks,
                    open_lots: order.open_qty(),
                    visible_lots: resting.visible_lots,
                });
            }
        });
        entries.sort_by(|a, b| a.symbol.cmp(&b.symbol).then(a.order_id.cmp(&b.order_id)));
        let root = chain_root(&entries);
        let count = entries.len();
        Self {
            entries,
            root,
            count,
        }
    }

    /// Recompute the root from the stored entries (self-check).
    #[must_use]
    pub fn verify(&self) -> bool {
        let mut sorted = self.entries.clone();
        sorted.sort_by(|a, b| a.symbol.cmp(&b.symbol).then(a.order_id.cmp(&b.order_id)));
        chain_root(&sorted) == self.root
    }
}

/// SHA-256 hash chain: `root_0 = H(LINK || H(leaf_0))`,
/// `root_{i+1} = H(LINK || root_i || H(leaf_{i+1}))`.
fn chain_root(entries: &[BookEntry]) -> [u8; 32] {
    let mut root: Option<[u8; 32]> = None;
    for e in entries {
        let leaf = sha256(&e.encode());
        root = Some(match root {
            None => sha256(&[LINK_DOMAIN, &leaf].concat()),
            Some(prev) => sha256(&[LINK_DOMAIN, &prev, &leaf].concat()),
        });
    }
    root.unwrap_or(sha256(LINK_DOMAIN))
}

/// Iterate the engine's books in symbol-sorted (canonical) order,
/// borrowing each book for its resting-order sweep.
fn for_each_book<'a>(
    engine: &'a Engine,
    mut f: impl FnMut(&'a str, &'a poc_orderbook::LimitOrderBook),
) {
    for symbol in engine.instruments().keys() {
        if let Some(book) = engine.book(symbol) {
            f(symbol, book);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poc_core::{Instrument, PerpMarket, Side};
    use poc_engine::{Command, EngineConfig, OrderRequest};

    fn engine_with_market() -> Engine {
        let mut e = Engine::new(EngineConfig::default());
        e.register_instrument(Instrument::Perp(PerpMarket::default()));
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: 1_000,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: 1_000 });
        e
    }

    #[test]
    fn empty_book_has_canonical_root() {
        let e = engine_with_market();
        let c = BookCommitment::capture(&e);
        assert_eq!(c.count, 0);
        assert!(c.verify());
        // Two empty books agree bit for bit.
        let e2 = engine_with_market();
        assert_eq!(c.root, BookCommitment::capture(&e2).root);
    }

    #[test]
    fn replayed_engines_agree_bit_for_bit() {
        let commands = |e: &mut Engine| {
            for sub in 1..=3_u64 {
                e.process(Command::Deposit {
                    subaccount: sub,
                    amount_quote_minor: 10_000_000,
                });
            }
            e.process(Command::Place {
                request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_900, 5),
                now: 2_000,
            });
            e.process(Command::Place {
                request: OrderRequest::limit(2, "BTC-PERP", Side::Bid, 79_800, 7),
                now: 2_001,
            });
            e.process(Command::Place {
                request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_100, 4),
                now: 2_002,
            });
            e.process(Command::Place {
                request: OrderRequest::limit(3, "BTC-PERP", Side::Bid, 80_100, 2),
                now: 2_003,
            });
            e.process(Command::Cancel {
                subaccount: 1,
                order_id: 1,
                now: 2_004,
            });
        };
        let mut a = engine_with_market();
        commands(&mut a);
        let mut b = engine_with_market();
        commands(&mut b);
        let (ca, cb) = (BookCommitment::capture(&a), BookCommitment::capture(&b));
        assert_eq!(ca.root, cb.root);
        assert_eq!(ca.count, 2, "one canceled, one fully filled");
        assert!(ca.verify());

        // Any single mutation changes the root.
        let mut c = engine_with_market();
        commands(&mut c);
        c.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_700, 1),
            now: 2_010,
        });
        let cc = BookCommitment::capture(&c);
        assert_ne!(ca.root, cc.root);
        assert_eq!(cc.count, 3);
        assert!(cc.verify());
    }

    #[test]
    fn commitment_captures_fills_and_cancels() {
        let mut e = engine_with_market();
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 79_950, 3),
            now: 2_001,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_960, 3),
            now: 2_002,
        });
        let c = BookCommitment::capture(&e);
        assert_eq!(c.count, 0, "fully-filled orders leave the book");
        // Partial fill leaves a remainder with reduced open quantity.
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 80_050, 5),
            now: 2_003,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 80_060, 2),
            now: 2_004,
        });
        let c2 = BookCommitment::capture(&e);
        assert_eq!(c2.count, 1);
        assert_eq!(c2.entries[0].open_lots, 3);
        assert!(!c2.entries[0].is_bid);
        assert!(c2.verify());
    }
}
