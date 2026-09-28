//! The settled state: one commitment leaf per subaccount plus the venue's
//! internal pools, and the canonical mutation algebra that moves it
//! forward.

use std::collections::BTreeMap;

use poc_core::{SubaccountId, Symbol};

use crate::hash::sha256;
use crate::merkle::{inclusion_proof, leaf_hash, merkle_root, InclusionProof};

/// Reserved ids for the venue's own pools. Real subaccounts are expected to
/// stay below `USER_SUBACCOUNT_CEILING` (enforced by the capture, not the
/// engine).
pub const HOUSE_SUBACCOUNT: SubaccountId = SubaccountId::MAX - 4;
/// Reserved synthetic subaccount the insurance pool trades through on
/// the settlement capture.
pub const INSURANCE_SUBACCOUNT: SubaccountId = SubaccountId::MAX - 3;
/// Reserved synthetic subaccount holding unsettled reward emissions.
pub const REWARDS_SUBACCOUNT: SubaccountId = SubaccountId::MAX - 2;
/// Reserved synthetic subaccount holding the buyback overflow pool.
pub const BUYBACK_SUBACCOUNT: SubaccountId = SubaccountId::MAX - 1;
/// The largest subaccount id the settlement layer attributes to users.
pub const USER_SUBACCOUNT_CEILING: SubaccountId = SubaccountId::MAX - 8;

// Compile-time layout guard: reserved pool ids must not collide with the
// user id space.
const _: () = {
    assert!(HOUSE_SUBACCOUNT >= USER_SUBACCOUNT_CEILING);
    assert!(INSURANCE_SUBACCOUNT >= USER_SUBACCOUNT_CEILING);
    assert!(REWARDS_SUBACCOUNT >= USER_SUBACCOUNT_CEILING);
    assert!(BUYBACK_SUBACCOUNT >= USER_SUBACCOUNT_CEILING);
};

/// One account's settled balances (the commitment payload).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountCommitment {
    /// Owning subaccount.
    pub subaccount: SubaccountId,
    /// Quote cash, signed minor units.
    pub cash_quote_minor: i128,
    /// Signed positions per instrument symbol.
    pub positions: BTreeMap<Symbol, i64>,
}

impl AccountCommitment {
    /// Canonical byte encoding (length-prefixed, big-endian, symbols in
    /// BTreeMap order — fully deterministic).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + 16 * self.positions.len());
        out.extend_from_slice(&self.subaccount.to_be_bytes());
        out.extend_from_slice(&self.cash_quote_minor.to_be_bytes());
        out.extend_from_slice(
            &u32::try_from(self.positions.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        for (symbol, lots) in &self.positions {
            out.extend_from_slice(
                &u16::try_from(symbol.len())
                    .unwrap_or(u16::MAX)
                    .to_be_bytes(),
            );
            out.extend_from_slice(symbol.as_bytes());
            out.extend_from_slice(&lots.to_be_bytes());
        }
        out
    }

    /// The leaf hash of this commitment.
    #[must_use]
    pub fn leaf(&self) -> [u8; 32] {
        leaf_hash(&self.encode())
    }
}

/// A signed balance/position mutation on one subaccount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountMutation {
    /// Target subaccount.
    pub subaccount: SubaccountId,
    /// Cash delta, quote minor.
    pub cash_delta_quote_minor: i128,
    /// Position deltas per symbol.
    pub position_deltas: BTreeMap<Symbol, i64>,
}

/// The full settled state and its root.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SettlementState {
    /// All commitment leaves (subaccounts + reserved venue pools).
    pub accounts: BTreeMap<SubaccountId, AccountCommitment>,
}

impl SettlementState {
    /// Empty state (the genesis root).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Upsert one leaf.
    pub fn upsert(&mut self, leaf: AccountCommitment) {
        self.accounts.insert(leaf.subaccount, leaf);
    }

    /// Apply a mutation to one account (creating it if absent).
    pub fn apply(&mut self, m: &AccountMutation) {
        let acct = self
            .accounts
            .entry(m.subaccount)
            .or_insert_with(|| AccountCommitment {
                subaccount: m.subaccount,
                ..AccountCommitment::default()
            });
        acct.cash_quote_minor = acct
            .cash_quote_minor
            .saturating_add(m.cash_delta_quote_minor);
        for (symbol, delta) in &m.position_deltas {
            let lots = acct.positions.entry(symbol.clone()).or_insert(0);
            *lots = lots.saturating_add(*delta);
            if *lots == 0 {
                acct.positions.remove(symbol);
            }
        }
    }

    /// Ordered leaf hashes (BTreeMap iteration = ascending subaccount id).
    #[must_use]
    pub fn leaf_hashes(&self) -> Vec<[u8; 32]> {
        self.accounts
            .values()
            .map(AccountCommitment::leaf)
            .collect()
    }

    /// The merkle root of the current state.
    #[must_use]
    pub fn root(&self) -> [u8; 32] {
        merkle_root(&self.leaf_hashes())
    }

    /// Inclusion proof for one subaccount's leaf against the current root.
    #[must_use]
    pub fn proof(&self, subaccount: SubaccountId) -> Option<(InclusionProof, [u8; 32])> {
        let hashes = self.leaf_hashes();
        let idx = self.accounts.keys().position(|&k| k == subaccount)?;
        Some((inclusion_proof(&hashes, idx)?, self.root()))
    }

    /// Total user cash (all non-reserved accounts), for conservation
    /// checks.
    #[must_use]
    pub fn user_cash_total(&self) -> i128 {
        self.accounts
            .values()
            .filter(|a| a.subaccount < USER_SUBACCOUNT_CEILING)
            .map(|a| a.cash_quote_minor)
            .fold(0_i128, |acc, x| acc.saturating_add(x))
    }
}

/// Hash a batch header: `sha256("POC-BATCH" || sequence || prev_root ||
/// new_root || ops_root)`.
#[must_use]
pub fn batch_header_hash(
    sequence: u64,
    prev_root: &[u8; 32],
    new_root: &[u8; 32],
    ops_root: &[u8; 32],
) -> [u8; 32] {
    let mut buf = [0_u8; 9 + 8 + 96];
    buf[..9].copy_from_slice(b"POC-BATCH");
    buf[9..17].copy_from_slice(&sequence.to_be_bytes());
    buf[17..49].copy_from_slice(prev_root);
    buf[49..81].copy_from_slice(new_root);
    buf[81..].copy_from_slice(ops_root);
    sha256(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(sub: u64, cash: i128) -> AccountCommitment {
        let mut a = AccountCommitment {
            subaccount: sub,
            cash_quote_minor: cash,
            positions: BTreeMap::new(),
        };
        a.positions.insert("BTC-PERP".into(), 3);
        a
    }

    #[test]
    fn mutation_round_trip() {
        let mut s = SettlementState::empty();
        s.upsert(acct(1, 100));
        s.apply(&AccountMutation {
            subaccount: 1,
            cash_delta_quote_minor: -40,
            position_deltas: [("BTC-PERP".into(), -2)].into_iter().collect(),
        });
        let a = &s.accounts[&1];
        assert_eq!(a.cash_quote_minor, 60);
        assert_eq!(a.positions["BTC-PERP"], 1);
        // Flat positions prune out of the commitment.
        s.apply(&AccountMutation {
            subaccount: 1,
            cash_delta_quote_minor: 0,
            position_deltas: [("BTC-PERP".into(), -1)].into_iter().collect(),
        });
        assert!(!s.accounts[&1].positions.contains_key("BTC-PERP"));
    }

    #[test]
    fn proof_verifies_against_root() {
        let mut s = SettlementState::empty();
        for i in 1..=9_u64 {
            s.upsert(acct(i, i as i128 * 10));
        }
        let (proof, root) = s.proof(5).unwrap();
        let leaf = s.accounts[&5].leaf();
        assert!(proof.verify(&leaf, &root));
    }

    #[test]
    fn reserved_ids_are_above_user_ceiling() {
        // The compile-time checks below guarantee this; keep the runtime
        // mirror as documentation of the layout invariant.
        let all = [
            HOUSE_SUBACCOUNT,
            INSURANCE_SUBACCOUNT,
            REWARDS_SUBACCOUNT,
            BUYBACK_SUBACCOUNT,
        ];
        assert!(all.iter().all(|&id| id >= USER_SUBACCOUNT_CEILING));
    }

    #[test]
    fn encoding_is_deterministic_and_order_sensitive() {
        let mut a = acct(1, 5);
        a.positions.insert("AAA".into(), 1);
        a.positions.insert("ZZZ".into(), 2);
        let e1 = a.encode();
        let mut b = acct(1, 5);
        b.positions.insert("ZZZ".into(), 2);
        b.positions.insert("AAA".into(), 1);
        let e2 = b.encode();
        assert_eq!(e1, e2, "BTreeMap order dominates insertion order");
        let mut c = acct(2, 5);
        c.positions.insert("AAA".into(), 1);
        c.positions.insert("ZZZ".into(), 2);
        assert_ne!(e1, c.encode(), "subaccount is bound into the leaf");
    }
}
