//! The exit hatch (G-30): withdrawal intents, the force-settlement
//! window, and proof-of-neglect.
//!
//! An operator that commits state roots on-chain but stops including
//! user withdrawals is holding funds hostage. The escape is the standard
//! dYdX/Hyperliquid/Lighter pattern:
//!
//! 1. a user registers a **withdrawal intent** (idempotent, per
//!    subaccount+nonce) against the latest committed root, with a merkle
//!    inclusion proof of their balance;
//! 2. the operator must include the withdrawal in a batch within
//!    `force_window_batches`;
//! 3. intents that outlive the window become **neglected** — anyone can
//!    present them as proof-of-neglect (the basis for slashing the
//!    operator, or triggering an L1-enforced emergency exit in the
//!    production deployment).

use std::collections::BTreeMap;

use poc_core::SubaccountId;

use crate::state::{AccountCommitment, SettlementState, USER_SUBACCOUNT_CEILING};

/// One registered withdrawal intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalIntent {
    /// Withdrawing subaccount.
    pub subaccount: SubaccountId,
    /// Operator nonce for idempotency.
    pub nonce: u64,
    /// Amount, quote minor.
    pub amount_quote_minor: u128,
    /// Inclusion proof of the account leaf at registration time.
    pub proof: crate::merkle::InclusionProof,
    /// Root the proof was taken against.
    pub root_at_registration: [u8; 32],
    /// Batch sequence when the intent was registered.
    pub registered_at_batch: u64,
    /// Deadline: the last batch sequence that may still settle it.
    pub deadline_batch: u64,
}

/// The exit queue over one underlying asset class (quote cash here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitQueue {
    /// Outstanding intents by (subaccount, nonce).
    intents: BTreeMap<(SubaccountId, u64), WithdrawalIntent>,
    /// Nonce budget already *consumed* by a settled withdrawal per
    /// subaccount (replay protection).
    settled_nonces: BTreeMap<SubaccountId, u64>,
    /// How many batches an intent may wait before it counts as neglected.
    force_window_batches: u64,
}

impl ExitQueue {
    /// New queue with the given force window.
    #[must_use]
    pub fn new(force_window_batches: u64) -> Self {
        Self {
            intents: BTreeMap::new(),
            settled_nonces: BTreeMap::new(),
            force_window_batches,
        }
    }

    /// Register an intent from a committed state.
    ///
    /// Fails (returning `None`) when the proof does not verify against
    /// the state, the account is a reserved pool, the amount exceeds the
    /// proven cash, or the nonce is stale/replayed.
    pub fn register(
        &mut self,
        state: &SettlementState,
        subaccount: SubaccountId,
        nonce: u64,
        amount_quote_minor: u128,
        current_batch: u64,
    ) -> Option<()> {
        if subaccount >= USER_SUBACCOUNT_CEILING {
            return None;
        }
        if self
            .settled_nonces
            .get(&subaccount)
            .is_some_and(|&n| n >= nonce)
        {
            return None;
        }
        let (proof, root) = state.proof(subaccount)?;
        let leaf = AccountCommitment::leaf(state.accounts.get(&subaccount)?);
        if !proof.verify(&leaf, &root) {
            return None;
        }
        let cash = poc_core::to_i128(amount_quote_minor);
        if cash > state.accounts.get(&subaccount)?.cash_quote_minor {
            return None;
        }
        self.intents.insert(
            (subaccount, nonce),
            WithdrawalIntent {
                subaccount,
                nonce,
                amount_quote_minor,
                proof,
                root_at_registration: root,
                registered_at_batch: current_batch,
                deadline_batch: current_batch.saturating_add(self.force_window_batches),
            },
        );
        Some(())
    }

    /// The operator settled a withdrawal: mark the nonce consumed and
    /// clear the intent.
    pub fn settle(&mut self, subaccount: SubaccountId, nonce: u64) {
        self.intents.remove(&(subaccount, nonce));
        let e = self.settled_nonces.entry(subaccount).or_insert(0);
        *e = (*e).max(nonce);
    }

    /// Intents that have outlived their window at `current_batch` — the
    /// proof-of-neglect set (deterministic order).
    #[must_use]
    pub fn neglected(&self, current_batch: u64) -> Vec<&WithdrawalIntent> {
        self.intents
            .values()
            .filter(|i| current_batch > i.deadline_batch)
            .collect()
    }

    /// Outstanding (not yet settled, not yet neglected) intents.
    #[must_use]
    pub fn outstanding(&self) -> Vec<&WithdrawalIntent> {
        self.intents.values().collect()
    }

    /// Look up one intent.
    #[must_use]
    pub fn intent(&self, subaccount: SubaccountId, nonce: u64) -> Option<&WithdrawalIntent> {
        self.intents.get(&(subaccount, nonce))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(sub: u64, cash: i128) -> SettlementState {
        let mut s = SettlementState::empty();
        s.upsert(AccountCommitment {
            subaccount: sub,
            cash_quote_minor: cash,
            positions: BTreeMap::new(),
        });
        s
    }

    #[test]
    fn register_requires_valid_proof_and_funds() {
        let s = state_with(1, 1_000);
        let mut q = ExitQueue::new(3);
        assert!(q.register(&s, 1, 1, 1_000, 0).is_some());
        // Over the proven cash.
        assert!(q.register(&s, 1, 2, 1_001, 0).is_none());
        // Unknown account (no proof possible).
        assert!(q.register(&s, 9, 1, 1, 0).is_none());
        // Reserved pool id.
        assert!(q.register(&s, USER_SUBACCOUNT_CEILING, 1, 1, 0).is_none());
    }

    #[test]
    fn nonce_replay_rejected() {
        let s = state_with(1, 1_000);
        let mut q = ExitQueue::new(3);
        q.register(&s, 1, 5, 10, 0);
        q.settle(1, 5);
        // Same nonce again: replay.
        assert!(q.register(&s, 1, 5, 10, 1).is_none());
        // Lower nonce: stale.
        assert!(q.register(&s, 1, 4, 10, 1).is_none());
        // Higher nonce: fine.
        assert!(q.register(&s, 1, 6, 10, 1).is_some());
    }

    #[test]
    fn neglect_after_window_and_recovery() {
        let s = state_with(1, 1_000);
        let mut q = ExitQueue::new(3);
        q.register(&s, 1, 1, 10, 0);
        // Batches 0..=3 are within the window.
        assert!(q.neglected(3).is_empty());
        assert_eq!(q.neglected(4).len(), 1);
        // Settling clears it.
        q.settle(1, 1);
        assert!(q.neglected(4).is_empty());
        assert!(q.outstanding().is_empty());
    }

    #[test]
    fn intent_carries_its_deadline() {
        let s = state_with(2, 500);
        let mut q = ExitQueue::new(10);
        q.register(&s, 2, 1, 100, 42);
        let i = q.intent(2, 1).unwrap();
        assert_eq!(i.registered_at_batch, 42);
        assert_eq!(i.deadline_batch, 52);
        assert_eq!(i.amount_quote_minor, 100);
    }
}
