//! Settlement batches and the validator that audits them.
//!
//! A [`SettlementBatch`] is the operator's on-chain unit: the ordered
//! mutation list for one window, the state root before and after, and a
//! hash-chained header (`hash_n = H(n, root_{n-1}, root_n, ops_root)`).
//! Anyone holding the chain of headers can:
//!
//! 1. **Verify linkage** — every batch's `prev_root` equals its
//!    predecessor's `new_root` (no silent forking of the settled state);
//! 2. **Replay** — applying the mutations to the pre-state must reproduce
//!    the claimed post-root (the on-chain verifier's job in production;
//!    here, the unit test's);
//! 3. **Audit conservation** — the sum of tracked value moves only by the
//!    declared custodial residual (deposits/withdrawals). Anything else is
//!    a mint or burn the operator cannot justify.

use crate::diff::StateCapture;
use crate::hash::sha256;
use crate::merkle::leaf_hash;
use crate::state::{batch_header_hash, AccountMutation, SettlementState};

/// One published settlement batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementBatch {
    /// Sequence number (0-based, strictly consecutive).
    pub sequence: u64,
    /// State root before the batch applied.
    pub prev_root: [u8; 32],
    /// State root after the batch applied.
    pub new_root: [u8; 32],
    /// The ordered mutations.
    pub mutations: Vec<AccountMutation>,
    /// Declared custodial flow: deposits minus withdrawals over the
    /// window (positive = value entered the tracked universe).
    pub custodial_residual: i128,
    /// Creation time (ms).
    pub ts: u64,
    /// The chained header hash.
    pub hash: [u8; 32],
}

impl SettlementBatch {
    /// Root of the mutation list (leaf = per-mutation canonical hash).
    #[must_use]
    pub fn ops_root(&self) -> [u8; 32] {
        let hashes: Vec<[u8; 32]> = self.mutations.iter().map(mutation_leaf).collect();
        if hashes.is_empty() {
            return [0_u8; 32];
        }
        crate::merkle::merkle_root(&hashes)
    }

    /// Recompute the header hash (must equal `self.hash`).
    #[must_use]
    pub fn compute_hash(&self) -> [u8; 32] {
        batch_header_hash(
            self.sequence,
            &self.prev_root,
            &self.new_root,
            &self.ops_root(),
        )
    }
}

/// Canonical mutation encoding for the ops merkle tree.
fn mutation_leaf(m: &AccountMutation) -> [u8; 32] {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"POC-OP");
    buf.extend_from_slice(&m.subaccount.to_be_bytes());
    buf.extend_from_slice(&m.cash_delta_quote_minor.to_be_bytes());
    buf.extend_from_slice(
        &u16::try_from(m.position_deltas.len())
            .unwrap_or(u16::MAX)
            .to_be_bytes(),
    );
    for (symbol, lots) in &m.position_deltas {
        buf.extend_from_slice(
            &u16::try_from(symbol.len())
                .unwrap_or(u16::MAX)
                .to_be_bytes(),
        );
        buf.extend_from_slice(symbol.as_bytes());
        buf.extend_from_slice(&lots.to_be_bytes());
    }
    leaf_hash(&buf)
}

/// Why a batch failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    /// Sequence numbers not consecutive from the initial root.
    BrokenSequence {
        /// Expected sequence.
        expected: u64,
        /// Actual sequence.
        actual: u64,
    },
    /// `prev_root` does not match the running root.
    ForkedChain,
    /// Header hash mismatch (corrupted or forged header).
    HeaderHashMismatch,
    /// Applying the mutations did not reach the claimed `new_root`.
    RootMismatch,
    /// Value moved without a declared custodial flow.
    UnexplainedFlow {
        /// Residual implied by the mutations.
        implied: i128,
        /// Residual declared by the operator.
        declared: i128,
    },
}

/// Build a batch from two captures of the engine.
///
/// The operator calls this at the end of each window; the resulting
/// batch is what gets committed on-chain.
#[must_use]
pub fn build_batch(
    sequence: u64,
    prev_root: [u8; 32],
    before: &StateCapture,
    after: &StateCapture,
    ts: u64,
) -> SettlementBatch {
    let (mutations, residual) = before.diff_to(after);

    // Post-state from the capture (for the new root).
    let mut post = SettlementState::empty();
    for (sub, leaf) in after.with_pools() {
        let _ = sub;
        post.upsert(leaf);
    }
    let new_root = post.root();

    let batch = SettlementBatch {
        sequence,
        prev_root,
        new_root,
        mutations,
        custodial_residual: residual,
        ts,
        hash: [0_u8; 32],
    };
    let mut with_hash = batch.clone();
    with_hash.hash = with_hash.compute_hash();
    with_hash
}

/// Validate a batch chain from a known starting root (linkage audit).
///
/// Checks header hashes, consecutive sequencing, and `prev_root` chaining.
/// Returns the final root. Full state replay is [`validate_batch`], run
/// per batch by watchers holding the leaves.
pub fn validate_chain(
    initial_root: [u8; 32],
    batches: &[SettlementBatch],
) -> Result<[u8; 32], ValidationError> {
    let mut root = initial_root;
    for (i, batch) in batches.iter().enumerate() {
        if batch.sequence != u64::try_from(i).unwrap_or(u64::MAX) {
            return Err(ValidationError::BrokenSequence {
                expected: u64::try_from(i).unwrap_or(u64::MAX),
                actual: batch.sequence,
            });
        }
        if batch.hash != batch.compute_hash() {
            return Err(ValidationError::HeaderHashMismatch);
        }
        if batch.prev_root != root {
            return Err(ValidationError::ForkedChain);
        }
        root = batch.new_root;
    }
    Ok(root)
}

/// Full validator: replay mutations against a concrete pre-state and
/// verify the claimed root and conservation.
///
/// This is the check an on-chain verifier (or a watchtower) runs: given
/// the pre-state (reconstructable from the previous batch + genesis),
/// the mutations must produce exactly `new_root`.
pub fn validate_batch(
    pre: &SettlementState,
    batch: &SettlementBatch,
    custodial_flow: i128,
) -> Result<SettlementState, ValidationError> {
    if batch.hash != batch.compute_hash() {
        return Err(ValidationError::HeaderHashMismatch);
    }
    if batch.prev_root != pre.root() {
        return Err(ValidationError::ForkedChain);
    }

    let before_total = pre
        .accounts
        .values()
        .map(|a| a.cash_quote_minor)
        .fold(0_i128, |acc, x| acc.saturating_add(x));

    let mut post = pre.clone();
    for m in &batch.mutations {
        post.apply(m);
    }

    let after_total = post
        .accounts
        .values()
        .map(|a| a.cash_quote_minor)
        .fold(0_i128, |acc, x| acc.saturating_add(x));

    // after − before = −residual (residual = before − after).
    let implied = before_total.saturating_sub(after_total);
    if implied != custodial_flow || custodial_flow != batch.custodial_residual {
        return Err(ValidationError::UnexplainedFlow {
            implied,
            declared: batch.custodial_residual,
        });
    }

    if post.root() != batch.new_root {
        return Err(ValidationError::RootMismatch);
    }
    Ok(post)
}

/// Simple auditor's hash for a byte blob (used by the exit queue proofs).
#[must_use]
pub fn audit_hash(data: &[u8]) -> [u8; 32] {
    sha256(data)
}
