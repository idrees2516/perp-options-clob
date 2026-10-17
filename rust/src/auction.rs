//! Closing/reopening auction: single-price clearing.
//!
//! Given the auction interest resting in a book, find the clearing price
//! that maximizes matched volume; break ties toward the price closest to a
//! reference (last mark), then toward the lower price. Allocation is
//! price-time priority on both sides, executed by the engine.

use crate::book::Book;
use crate::types::{Price, Side};

/// Result of the clearing computation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clearing {
    /// Clearing price (0 when nothing can cross).
    pub price: Price,
    /// Total quantity that can trade at `price`.
    pub qty: u64,
}

/// Compute the single-price clearing over a book.
///
/// Buy volume at price `p` = sum of bids with limit >= p; sell volume =
/// sum of asks with limit <= p. Matched(p) = min(buy, sell). Candidates are
/// all bid and ask limit prices; the best candidate wins (max volume, then
/// closest to `reference`, then lower).
pub fn clearing_price(book: &Book, reference: Price) -> Clearing {
    // Collect (price, qty) arrays for both sides, best-first.
    #[allow(clippy::needless_range_loop)]
    let mut bids: Vec<(Price, u64)> = Vec::with_capacity(64);
    let mut asks: Vec<(Price, u64)> = Vec::with_capacity(64);
    collect_levels(book, Side::Buy, &mut bids);
    collect_levels(book, Side::Sell, &mut asks);
    if bids.is_empty() || asks.is_empty() {
        return Clearing { price: 0, qty: 0 };
    }
    let best_bid = bids[0].0;
    let best_ask = asks[0].0;
    if best_bid < best_ask {
        return Clearing { price: 0, qty: 0 }; // locked market absent
    }

    // Candidate prices: every bid/ask limit.
    let mut cands: Vec<Price> = Vec::with_capacity(bids.len() + asks.len());
    cands.extend(bids.iter().map(|b| b.0));
    cands.extend(asks.iter().map(|a| a.0));
    cands.sort_unstable();
    cands.dedup();

    // Prefix sums: bids sorted desc -> cum buy as price decreases.
    // asks sorted asc -> cum sell as price increases.
    let mut bids_desc = bids;
    bids_desc.sort_unstable_by_key(|b| std::cmp::Reverse(b.0));
    let mut asks_asc = asks.clone();
    asks_asc.sort_by_key(|a| a.0);
    let cum_buy: Vec<u64> = prefix(&bids_desc);
    let cum_sell: Vec<u64> = prefix(&asks_asc);

    let buy_at = |p: Price| -> u64 {
        // sum of bids >= p == cum_buy[index of last bid >= p]
        let n = bids_desc.partition_point(|b| b.0 >= p);
        if n == 0 {
            0
        } else {
            cum_buy[n - 1]
        }
    };
    let sell_at = |p: Price| -> u64 {
        let n = asks_asc.partition_point(|a| a.0 <= p);
        if n == 0 {
            0
        } else {
            cum_sell[n - 1]
        }
    };

    let mut best = Clearing { price: 0, qty: 0 };
    for &p in &cands {
        let m = buy_at(p).min(sell_at(p));
        if m > best.qty {
            best = Clearing { price: p, qty: m };
        } else if m == best.qty && m > 0 {
            // Tie: closest to reference, then lower price.
            let d_new = p.abs_diff(reference);
            let d_old = best.price.abs_diff(reference);
            if d_new < d_old || (d_new == d_old && p < best.price) {
                best = Clearing { price: p, qty: m };
            }
        }
    }
    best
}

fn collect_levels(book: &Book, side: Side, out: &mut Vec<(Price, u64)>) {
    let mut buf = [(0u64, 0u64); 64];
    let n = book.depth(side, 64, &mut buf);
    out.extend_from_slice(&buf[..n]);
}

fn prefix(v: &[(Price, u64)]) -> Vec<u64> {
    let mut c = Vec::with_capacity(v.len());
    let mut s = 0u64;
    for &(_, q) in v {
        s += q;
        c.push(s);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::{make_order, Book, NIL};
    use crate::types::{Kind, Side, Tif};

    fn add(b: &mut Book, side: Side, price: u64, qty: u32) {
        b.insert(make_order(
            1, 0, side, price, qty, Tif::Gtc, Kind::Limit, 0, NIL, NIL, 0, 0,
        ));
    }

    #[test]
    fn simple_cross_clears_at_equilibrium() {
        let mut b = Book::with_capacity(64, 16);
        add(&mut b, Side::Buy, 105, 10);
        add(&mut b, Side::Buy, 104, 10);
        add(&mut b, Side::Sell, 103, 8);
        add(&mut b, Side::Sell, 104, 4);
        let c = clearing_price(&b, 104);
        // buy@>=104 = 20, sell@<=104 = 12 -> 12 at 104; at 103: buy 20, sell 8 -> 8.
        assert_eq!(c, Clearing { price: 104, qty: 12 });
    }

    #[test]
    fn no_cross_is_zero() {
        let mut b = Book::with_capacity(64, 16);
        add(&mut b, Side::Buy, 100, 5);
        add(&mut b, Side::Sell, 102, 5);
        let c = clearing_price(&b, 101);
        assert_eq!(c.qty, 0);
    }

    #[test]
    fn tie_breaks_toward_reference() {
        let mut b = Book::with_capacity(64, 16);
        add(&mut b, Side::Buy, 105, 5);
        add(&mut b, Side::Sell, 95, 5);
        // Candidates are the limit prices 105 and 95 (both match 5);
        // the one closest to the reference wins.
        let c = clearing_price(&b, 104);
        assert_eq!(c.price, 105);
        let d = clearing_price(&b, 96);
        assert_eq!(d.price, 95);
    }
}
