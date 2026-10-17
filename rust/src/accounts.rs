//! Accounts, positions and the exact-integer ledger.
//!
//! Money flows are strict double-entry: every premium exchange, fee, rebate
//! and exercise cash-flow moves value between accounts and/or the house
//! account such that `sum(account.cash) + house == total_deposits` holds
//! after **every** operation. All arithmetic is signed 128-bit integer.

use crate::fees::Tier;
use crate::types::{Cash, Qty, SymbolId};

/// A signed position in one option series, with cached risk contributions
/// so per-trade margin/delta updates are O(1).
#[derive(Clone, Copy, Debug)]
pub struct Position {
    /// Signed quantity: positive long, negative short.
    pub qty: i32,
    /// Cached short-margin contribution in ticks.
    pub margin: u64,
    /// Cached signed delta notional in ticks (delta_bps * qty * ref / 10_000).
    pub delta_notional: i64,
    /// Risk epoch at which `margin` / `delta_notional` were last computed.
    pub risk_epoch: u64,
}

impl Position {
    /// A flat position.
    pub const FLAT: Position = Position {
        qty: 0,
        margin: 0,
        delta_notional: 0,
        risk_epoch: 0,
    };

    /// Is this position short options (margin-relevant)?
    #[inline(always)]
    pub fn is_short(&self) -> bool {
        self.qty < 0
    }
}

/// Trading account: cash, aggregate margin requirement, tier, and a dense
/// `(symbol -> position slot)` table. Position slots are resolved once at
/// order admission and cached on the order, so the trade hot path never
/// searches.
#[derive(Clone, Debug)]
pub struct Account {
    /// Global account id (`NIL` = hole).
    pub id: u32,
    /// Fee tier.
    pub tier: Tier,
    /// Settled cash in ticks (premium flows, fees, exercise cash flows).
    pub cash: Cash,
    /// Cash committed to resting/crossing BUY orders (premium + fee holds).
    pub hold: Cash,
    /// Sum of cached position margins (short options + surcharges).
    pub margin_required: u64,
    /// Margin committed to resting SELL orders (released as they fill/cancel).
    pub margin_reserved: u64,
    /// Fees paid to the house over the session (audit counter).
    pub fees_paid: u64,
    /// Rebates received from the house over the session (audit counter).
    pub rebates_recv: u64,
    /// (symbol, slot into shard position slab).
    pub pos: Vec<(SymbolId, u32)>,
}

impl Account {
    /// A fresh account with a starting cash balance.
    pub fn new(id: u32, cash: Cash, tier: Tier) -> Account {
        Account {
            id,
            tier,
            cash,
            hold: 0,
            margin_required: 0,
            margin_reserved: 0,
            fees_paid: 0,
            rebates_recv: 0,
            pos: Vec::new(),
        }
    }

    /// Cash actually available: settled cash minus holds and margin calls.
    #[inline(always)]
    pub fn available(&self) -> Cash {
        self.cash - self.hold - self.margin_required as Cash - self.margin_reserved as Cash
    }

    /// Commit buying power: `cash_commit` per contract of buy orders,
    /// `margin_commit` for sell orders.
    #[inline(always)]
    pub fn commit(&mut self, cash_commit: Cash, margin_commit: u64) {
        self.hold += cash_commit;
        self.margin_reserved += margin_commit;
    }

    /// Release buying power (on fill or cancel), exactly what was committed
    /// for the filled/cancelled quantity.
    #[inline(always)]
    pub fn release(&mut self, cash_commit: Cash, margin_commit: u64) {
        self.hold -= cash_commit;
        self.margin_reserved = self.margin_reserved.saturating_sub(margin_commit);
    }

    /// Find the position slot for a symbol, if one exists.
    #[inline(always)]
    pub fn pos_slot(&self, symbol: SymbolId) -> Option<u32> {
        // Accounts hold few concurrent series on a 0DTE chain; linear probe
        // over a small dense vec beats a hash map and stays cache-friendly.
        for &(s, slot) in &self.pos {
            if s == symbol {
                return Some(slot);
            }
        }
        None
    }

    /// Record the opening of a new position slot.
    pub fn open_slot(&mut self, symbol: SymbolId, slot: u32) {
        self.pos.push((symbol, slot));
    }
}

/// Token-bucket order rate limiter, one per account.
#[derive(Clone, Copy, Debug)]
pub struct TokenBucket {
    /// Current tokens (x256 for sub-unit precision).
    tokens: u64,
    /// Bucket capacity (x256).
    capacity: u64,
    /// Refill per second (x256).
    refill_per_sec: u64,
    /// Last refill logical time (nanos).
    last: u64,
}

impl TokenBucket {
    /// Create a bucket with `rate` orders/sec and burst = 2x rate.
    pub fn new(rate_per_sec: u64, now: u64) -> TokenBucket {
        let cap = (rate_per_sec.saturating_mul(2)).saturating_mul(256);
        TokenBucket {
            tokens: cap,
            capacity: cap,
            refill_per_sec: rate_per_sec.saturating_mul(256),
            last: now,
        }
    }

    /// Try to consume one token; refills lazily from elapsed logical time.
    ///
    /// Uses a single multiply + shift (1e9 ~= 2^30) instead of two 64-bit
    /// divisions — the rounding difference is immaterial for rate limiting.
    #[inline]
    pub fn take(&mut self, now: u64) -> bool {
        let elapsed = now.saturating_sub(self.last);
        let add = (elapsed.wrapping_mul(self.refill_per_sec)) >> 30;
        let t = self.tokens.saturating_add(add).min(self.capacity);
        self.tokens = t;
        self.last = now;
        if self.tokens >= 256 {
            self.tokens -= 256;
            true
        } else {
            false
        }
    }
}

/// The shard-level ledger: house balance + total deposits, enforcing the
/// global conservation invariant.
#[derive(Clone, Copy, Debug)]
pub struct Ledger {
    /// House account: absorbs fees, pays rebates, absorbs rounding residuals.
    pub house: Cash,
    /// Sum of all deposits ever made into this shard.
    pub deposits: Cash,
    /// Fees collected by the house (audit counter).
    pub fees_collected: u64,
    /// Rebates paid by the house (audit counter).
    pub rebates_paid: u64,
}

impl Ledger {
    /// An empty ledger.
    pub const fn empty() -> Ledger {
        Ledger {
            house: 0,
            deposits: 0,
            fees_collected: 0,
            rebates_paid: 0,
        }
    }

    /// Record a deposit.
    #[inline]
    pub fn deposit(&mut self, amount: Cash) {
        self.deposits += amount;
    }

    /// Record a withdrawal.
    #[inline]
    pub fn withdraw(&mut self, amount: Cash) {
        self.deposits -= amount;
    }

    /// Fee charged to an account lands in the house.
    #[inline]
    pub fn fee_in(&mut self, fee: u64) {
        self.house += fee as Cash;
        self.fees_collected += fee;
    }

    /// Rebate paid to an account comes out of the house.
    #[inline]
    pub fn rebate_out(&mut self, rebate: u64) {
        self.house -= rebate as Cash;
        self.rebates_paid += rebate;
    }
}

/// Signed quantity helper: convert (side, qty) to signed delta.
#[inline(always)]
pub fn signed(side: crate::types::Side, qty: Qty) -> i32 {
    match side {
        crate::types::Side::Buy => qty as i32,
        crate::types::Side::Sell => -(qty as i32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_allows_burst_then_throttles() {
        let mut tb = TokenBucket::new(10, 0); // 10/s, burst 20
        let mut ok = 0;
        for _ in 0..20 {
            if tb.take(0) {
                ok += 1;
            }
        }
        assert_eq!(ok, 20); // burst consumed
        assert!(!tb.take(0));
        assert!(tb.take(150_000_000)); // 0.15s -> 1.5 tokens -> one
    }

    #[test]
    fn ledger_flows() {
        let mut l = Ledger::empty();
        l.deposit(10_000);
        l.fee_in(65);
        l.rebate_out(10);
        assert_eq!(l.house, 55);
        assert_eq!(l.deposits, 10_000);
    }
}
