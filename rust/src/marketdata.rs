//! Market data derivation: consumes engine event batches and maintains
//! per-symbol session statistics for feeds and surveillance.
//!
//! This runs strictly off the critical path — the engine emits events; this
//! module derives. Cheap enough to run per batch in tests and simulators.

use crate::journal::{EvBuf, Event};
use crate::types::{Price, SymbolId};
use std::collections::HashMap;

/// Session statistics for one symbol.
#[derive(Clone, Copy, Debug, Default)]
pub struct SymStats {
    /// Session open.
    pub open: Price,
    /// Session high.
    pub high: Price,
    /// Session low.
    pub low: Price,
    /// Last trade.
    pub last: Price,
    /// Volume (contracts).
    pub vol: u64,
    /// Number of prints.
    pub prints: u64,
    /// Last bid.
    pub bid: Price,
    /// Last bid qty.
    pub bid_qty: u64,
    /// Last ask.
    pub ask: Price,
    /// Last ask qty.
    pub ask_qty: u64,
}

impl SymStats {
    /// Bid-ask spread, if two-sided.
    pub fn spread(&self) -> Option<Price> {
        if self.bid > 0 && self.ask > 0 {
            Some(self.ask.saturating_sub(self.bid))
        } else {
            None
        }
    }
    /// Midpoint, if two-sided.
    pub fn mid(&self) -> Option<Price> {
        if self.bid > 0 && self.ask > 0 {
            Some((self.bid + self.ask) / 2)
        } else {
            None
        }
    }
}

/// Market data state over a set of symbols.
#[derive(Clone, Debug, Default)]
pub struct Mds {
    stats: HashMap<SymbolId, SymStats>,
    /// Trades seen.
    pub trades: u64,
    /// Auction prints seen.
    pub auction_prints: u64,
    /// Expiries seen.
    pub expiries: u64,
    /// Halts seen.
    pub halts: u64,
}

impl Mds {
    /// An empty feed state.
    pub fn new() -> Mds {
        Mds::default()
    }

    /// Ingest one event batch.
    pub fn ingest(&mut self, ev: &EvBuf) {
        for e in &ev.events {
            self.on_event(e);
        }
    }

    /// Ingest one event.
    pub fn on_event(&mut self, e: &Event) {
        match *e {
            Event::Trade {
                symbol,
                price,
                qty,
                ..
            } => {
                let s = self.stats.entry(symbol).or_default();
                if s.open == 0 {
                    s.open = price;
                }
                if price > s.high {
                    s.high = price;
                }
                if s.low == 0 || price < s.low {
                    s.low = price;
                }
                s.last = price;
                s.vol += qty as u64;
                s.prints += 1;
                self.trades += 1;
                if let Event::Trade { aggressor: 2, .. } = e {
                    self.auction_prints += 1;
                }
            }
            Event::Bbo {
                symbol,
                bid,
                bid_qty,
                ask,
                ask_qty,
            } => {
                let s = self.stats.entry(symbol).or_default();
                s.bid = bid;
                s.bid_qty = bid_qty;
                s.ask = ask;
                s.ask_qty = ask_qty;
            }
            Event::Expired { .. } => {
                self.expiries += 1;
            }
            Event::Halted { .. } => {
                self.halts += 1;
            }
            _ => {}
        }
    }

    /// Stats for a symbol.
    pub fn stats(&self, symbol: SymbolId) -> SymStats {
        self.stats.get(&symbol).copied().unwrap_or_default()
    }

    /// Number of tracked symbols.
    pub fn len(&self) -> usize {
        self.stats.len()
    }

    /// Is the feed empty?
    pub fn is_empty(&self) -> bool {
        self.stats.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Event;
    use crate::types::SymbolId;

    #[test]
    fn tracks_ohlc_and_bbo() {
        let mut m = Mds::new();
        let mut ev = EvBuf::new();
        ev.push(Event::Trade {
            symbol: 3,
            price: 100,
            qty: 5,
            taker: 1,
            maker: 2,
            taker_acct: 1,
            maker_acct: 2,
            aggressor: 0,
            taker_fee: 0,
            maker_fee: 0,
            maker_rebate: false,
        });
        ev.push(Event::Trade {
            symbol: 3,
            price: 90,
            qty: 2,
            taker: 1,
            maker: 2,
            taker_acct: 1,
            maker_acct: 2,
            aggressor: 1,
            taker_fee: 0,
            maker_fee: 0,
            maker_rebate: false,
        });
        ev.push(Event::Bbo {
            symbol: 3,
            bid: 91,
            bid_qty: 7,
            ask: 93,
            ask_qty: 4,
        });
        m.ingest(&ev);
        let s = m.stats(3);
        assert_eq!((s.open, s.high, s.low, s.last, s.vol), (100, 100, 90, 90, 7));
        assert_eq!(s.spread(), Some(2));
        assert_eq!(s.mid(), Some(92));
        assert_eq!(m.len(), 1);
        let _ = SymbolId::MAX;
    }
}
