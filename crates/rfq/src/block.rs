//! Block trades with delayed public broadcast (audit gap G-13).
//!
//! Privately negotiated trades — typically RFQ executions — are registered
//! after they clear margin and fees, then printed on the public tape only
//! after a configurable delay (Deribit-style short window, default 15
//! minutes). The delay protects the negotiating parties' inventory and
//! hedging flows from front-running while still guaranteeing eventual
//! price discovery.

use std::collections::BTreeMap;

use poc_core::{Side, SubaccountId, Symbol, TimestampMs};

use crate::RfqError;

/// A privately negotiated trade report awaiting its delayed public print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockTrade {
    /// Engine-assigned id.
    pub block_id: u64,
    /// Both sides' consent (the venue's trust model journals both
    /// accounts). Leg sides are from the first counterparty's perspective.
    pub counterparties: (SubaccountId, SubaccountId),
    /// Legs + execution prices: `(symbol, side, qty lots, price ticks)` —
    /// the same shape as RFQ execution legs.
    pub legs: Vec<(Symbol, Side, u64, u64)>,
    /// Total notional across legs, quote minor.
    pub total_notional_quote_minor: u128,
    /// Execution timestamp (ms epoch).
    pub executed_ts: TimestampMs,
    /// Public print time (`executed_ts + delay`).
    pub broadcast_ts: TimestampMs,
    /// Whether the public tape has printed this block.
    pub broadcast: bool,
}

/// Ledger of negotiated blocks and their pending public prints.
///
/// Deterministic: blocks are keyed by id in a `BTreeMap`, so sweeps and
/// lookups iterate in ascending id order.
#[derive(Debug)]
pub struct BlockLedger {
    blocks: BTreeMap<u64, BlockTrade>,
    next_id: u64,
    delay_ms: TimestampMs,
}

impl BlockLedger {
    /// Default broadcast delay: 15 minutes (Deribit-style short window).
    pub const DEFAULT_DELAY_MS: TimestampMs = 900_000;

    /// New ledger that prints blocks `delay_ms` after execution.
    #[must_use]
    pub fn new(delay_ms: TimestampMs) -> Self {
        Self {
            blocks: BTreeMap::new(),
            next_id: 1,
            delay_ms,
        }
    }

    /// Register a settled block — the engine calls this *after* the trade
    /// has cleared margin and fees.
    ///
    /// Requires two distinct counterparties, non-empty legs, positive
    /// per-leg quantity and price. Returns the block id; the public print
    /// is due at `now + delay`.
    pub fn register(
        &mut self,
        a: SubaccountId,
        b: SubaccountId,
        legs: Vec<(Symbol, Side, u64, u64)>,
        total_notional: u128,
        now: TimestampMs,
    ) -> Result<u64, RfqError> {
        if a == b {
            return Err(RfqError::SelfQuote);
        }
        if legs.is_empty() {
            return Err(RfqError::InvalidQty);
        }
        if legs.iter().any(|l| l.2 == 0) {
            return Err(RfqError::InvalidQty);
        }
        if legs.iter().any(|l| l.3 == 0) {
            return Err(RfqError::ZeroPrice);
        }
        let broadcast_ts = now
            .checked_add(self.delay_ms)
            .ok_or(RfqError::MathOverflow)?;
        let block_id = self.next_id;
        self.next_id = block_id.checked_add(1).ok_or(RfqError::MathOverflow)?;
        self.blocks.insert(
            block_id,
            BlockTrade {
                block_id,
                counterparties: (a, b),
                legs,
                total_notional_quote_minor: total_notional,
                executed_ts: now,
                broadcast_ts,
                broadcast: false,
            },
        );
        Ok(block_id)
    }

    /// Mark all blocks whose `broadcast_ts` has come due as broadcast and
    /// return them (the public tape prints these). Already-broadcast blocks
    /// are not returned again. Ascending id order.
    pub fn sweep(&mut self, now: TimestampMs) -> Vec<&BlockTrade> {
        let mut due: Vec<u64> = Vec::new();
        for (id, block) in self.blocks.iter_mut() {
            if !block.broadcast && now >= block.broadcast_ts {
                block.broadcast = true;
                due.push(*id);
            }
        }
        due.into_iter()
            .filter_map(|id| self.blocks.get(&id))
            .collect()
    }

    /// Look up a block by id.
    #[must_use]
    /// Ids of settled blocks whose broadcast time has arrived (read-only).
    pub fn due_ids(&self, now: u64) -> Vec<u64> {
        self.blocks
            .values()
            .filter(|b| !b.broadcast && now >= b.broadcast_ts)
            .map(|b| b.block_id)
            .collect()
    }

    /// Mark one block as broadcast (idempotent; returns whether it flipped).
    pub fn mark_broadcast(&mut self, id: u64) -> bool {
        if let Some(b) = self.blocks.get_mut(&id) {
            if !b.broadcast {
                b.broadcast = true;
                return true;
            }
        }
        false
    }

    /// Look up one block by id.
    pub fn block(&self, id: u64) -> Option<&BlockTrade> {
        self.blocks.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAKER: SubaccountId = 1;
    const MAKER: SubaccountId = 2;
    const OTHER: SubaccountId = 3;

    fn legs() -> Vec<(String, Side, u64, u64)> {
        vec![
            ("BTC-PERP".to_string(), Side::Bid, 10, 65_000),
            ("ETH-PERP".to_string(), Side::Ask, 5, 3_000),
        ]
    }

    // 14. register -> hidden -> printed after the delay -----------------------

    #[test]
    fn block_ledger_delayed_broadcast() {
        let mut ledger = BlockLedger::new(BlockLedger::DEFAULT_DELAY_MS);
        assert_eq!(BlockLedger::DEFAULT_DELAY_MS, 900_000);
        let now = 90_000_000;

        let id = ledger.register(TAKER, MAKER, legs(), 650_000, now).unwrap();
        assert_eq!(id, 1);
        let b = ledger.block(id).unwrap();
        assert_eq!(b.counterparties, (TAKER, MAKER));
        assert_eq!(b.legs.len(), 2);
        assert_eq!(b.total_notional_quote_minor, 650_000);
        assert_eq!(b.executed_ts, now);
        assert_eq!(b.broadcast_ts, now + BlockLedger::DEFAULT_DELAY_MS);
        assert!(!b.broadcast);

        // Before the delay: nothing prints.
        assert!(ledger
            .sweep(now + BlockLedger::DEFAULT_DELAY_MS - 1)
            .is_empty());
        assert!(!ledger.block(id).unwrap().broadcast);

        // At the delay: prints exactly once.
        let printed = ledger.sweep(now + BlockLedger::DEFAULT_DELAY_MS);
        assert_eq!(printed.len(), 1);
        assert_eq!(printed[0].block_id, id);
        assert_eq!(printed[0].counterparties, (TAKER, MAKER));
        assert!(ledger.block(id).unwrap().broadcast);

        // After the delay: not printed again (idempotent).
        assert!(ledger
            .sweep(now + BlockLedger::DEFAULT_DELAY_MS + 1)
            .is_empty());

        // A second, later block prints only after its own window.
        let id2 = ledger
            .register(
                OTHER,
                MAKER,
                legs(),
                100,
                now + BlockLedger::DEFAULT_DELAY_MS,
            )
            .unwrap();
        assert_eq!(id2, 2);
        assert!(ledger
            .sweep(now + BlockLedger::DEFAULT_DELAY_MS + 1)
            .is_empty());
        let printed2 = ledger.sweep(now + 2 * BlockLedger::DEFAULT_DELAY_MS);
        assert_eq!(printed2.len(), 1);
        assert_eq!(printed2[0].block_id, id2);
    }

    // 14b. registration validation ---------------------------------------------

    #[test]
    fn block_register_validations() {
        let mut ledger = BlockLedger::new(1_000);
        let now = 91_000_000;

        // Self-pair rejected.
        assert_eq!(
            ledger.register(MAKER, MAKER, legs(), 1, now).unwrap_err(),
            RfqError::SelfQuote
        );
        // Empty legs rejected.
        assert_eq!(
            ledger.register(TAKER, MAKER, vec![], 1, now).unwrap_err(),
            RfqError::InvalidQty
        );
        // Zero quantity rejected.
        assert_eq!(
            ledger
                .register(TAKER, MAKER, vec![("X".into(), Side::Bid, 0, 100)], 1, now)
                .unwrap_err(),
            RfqError::InvalidQty
        );
        // Zero price rejected.
        assert_eq!(
            ledger
                .register(TAKER, MAKER, vec![("X".into(), Side::Bid, 1, 0)], 1, now)
                .unwrap_err(),
            RfqError::ZeroPrice
        );
        // Unknown block lookups are None.
        assert!(ledger.block(999).is_none());
        // Nothing registered -> nothing ever prints.
        assert!(ledger.sweep(now + 10_000_000).is_empty());
    }

    // 14c. zero delay prints immediately ----------------------------------------

    #[test]
    fn block_zero_delay_prints_immediately() {
        let mut ledger = BlockLedger::new(0);
        let now = 92_000_000;
        let id = ledger.register(TAKER, MAKER, legs(), 50, now).unwrap();
        assert_eq!(ledger.block(id).unwrap().broadcast_ts, now);
        let printed = ledger.sweep(now);
        assert_eq!(printed.len(), 1);
        assert!(ledger.block(id).unwrap().broadcast);
    }

    // 14d. determinism: same registrations, same ids and prints ------------------

    #[test]
    fn block_ledger_deterministic_ids() {
        fn scenario() -> (Vec<u64>, Vec<u64>, Vec<u64>) {
            let mut ledger = BlockLedger::new(500);
            let now = 93_000_000;
            let a = ledger.register(TAKER, MAKER, legs(), 10, now).unwrap();
            let b = ledger
                .register(OTHER, TAKER, legs(), 20, now + 100)
                .unwrap();
            let c = ledger
                .register(MAKER, OTHER, legs(), 30, now + 600)
                .unwrap();
            let first = ledger
                .sweep(now + 500)
                .into_iter()
                .map(|x| x.block_id)
                .collect();
            let second = ledger
                .sweep(now + 1_100)
                .into_iter()
                .map(|x| x.block_id)
                .collect();
            (vec![a, b, c], first, second)
        }
        assert_eq!(scenario(), scenario());
    }
}
