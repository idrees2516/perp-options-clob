//! Domain-separated merkle tree over 32-byte leaves.
//!
//! * Leaf preimage: `sha256("POC-LEAF" || leaf)` — one leaf per account
//!   commitment, sorted by subaccount id (deterministic ordering).
//! * Internal node: `sha256("POC-NODE" || left || right)`.
//! * Odd nodes duplicate the last element at each level (the standard
//!   Bitcoin-style rule; avoids lone-leaf promotion ambiguities).
//!
//! Domain separation makes leaf data and node data unforgeable against
//! each other, so an inclusion proof can never be repurposed as a
//! different proof shape.

use crate::hash::sha256;

const LEAF_TAG: &[u8] = b"POC-LEAF";
const NODE_TAG: &[u8] = b"POC-NODE";

/// Hash one leaf payload.
#[must_use]
pub fn leaf_hash(payload: &[u8]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(LEAF_TAG.len() + payload.len());
    buf.extend_from_slice(LEAF_TAG);
    buf.extend_from_slice(payload);
    sha256(&buf)
}

/// Hash one internal node from its children.
fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0_u8; 8 + 64];
    buf[..8].copy_from_slice(NODE_TAG);
    buf[8..40].copy_from_slice(left);
    buf[40..].copy_from_slice(right);
    sha256(&buf)
}

/// Merkle root of an ordered, non-empty leaf list.
///
/// The empty root is the all-zero placeholder (a canonical constant so
/// genesis roots are comparable across implementations).
#[must_use]
pub fn merkle_root(hashes: &[[u8; 32]]) -> [u8; 32] {
    if hashes.is_empty() {
        return [0_u8; 32];
    }
    let mut level: Vec<[u8; 32]> = hashes.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let right = pair.get(1).unwrap_or(&pair[0]);
            next.push(node_hash(&pair[0], right));
        }
        level = next;
    }
    level[0]
}

/// A merkle inclusion proof for one leaf (sibling hashes, leaf level up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InclusionProof {
    /// Leaf index the proof is for.
    pub index: u64,
    /// Sibling hashes, leaf level first.
    pub siblings: Vec<[u8; 32]>,
}

/// Build the inclusion proof for `hashes[index]`.
#[must_use]
pub fn inclusion_proof(hashes: &[[u8; 32]], index: usize) -> Option<InclusionProof> {
    if index >= hashes.len() {
        return None;
    }
    let mut siblings = Vec::new();
    let mut level: Vec<[u8; 32]> = hashes.to_vec();
    let mut idx = index;
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            next.push(node_hash(&pair[0], pair.get(1).unwrap_or(&pair[0])));
        }
        // The sibling at this level (duplicate-self on the right edge).
        let sibling_idx = if idx % 2 == 0 {
            (idx + 1).min(level.len() - 1)
        } else {
            idx - 1
        };
        siblings.push(level[sibling_idx]);
        idx /= 2;
        level = next;
    }
    Some(InclusionProof {
        index: u64::try_from(index).ok()?,
        siblings,
    })
}

impl InclusionProof {
    /// Verify this proof against an expected root.
    #[must_use]
    pub fn verify(&self, leaf: &[u8; 32], root: &[u8; 32]) -> bool {
        let mut cur = *leaf;
        let mut idx = self.index;
        for &sib in &self.siblings {
            cur = if idx % 2 == 0 {
                node_hash(&cur, &sib)
            } else {
                node_hash(&sib, &cur)
            };
            idx /= 2;
        }
        cur == *root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: usize) -> Vec<[u8; 32]> {
        (0..n).map(|i| leaf_hash(&[i as u8])).collect()
    }

    #[test]
    fn single_leaf_root() {
        let h = leaves(1);
        assert_eq!(merkle_root(&h), h[0]);
        let p = inclusion_proof(&h, 0).unwrap();
        assert!(p.siblings.is_empty());
        assert!(p.verify(&h[0], &h[0]));
    }

    #[test]
    fn proofs_verify_at_every_size() {
        for n in [2_usize, 3, 4, 5, 8, 9, 16, 17, 33] {
            let h = leaves(n);
            let root = merkle_root(&h);
            for i in 0..n {
                let p = inclusion_proof(&h, i).unwrap();
                assert!(p.verify(&h[i], &root), "size {n} index {i} failed");
                // A wrong leaf must fail.
                let wrong = leaf_hash(&[0xFF]);
                assert!(!p.verify(&wrong, &root));
            }
        }
    }

    #[test]
    fn domain_separation_leaf_is_not_a_node() {
        let l = leaves(2);
        let root = merkle_root(&l);
        assert_ne!(root, l[0]);
        assert_ne!(leaf_hash(&[]), node_hash(&[0; 32], &[0; 32]));
    }

    #[test]
    fn ordering_matters() {
        let mut h = leaves(4);
        let r1 = merkle_root(&h);
        h.swap(0, 1);
        let r2 = merkle_root(&h);
        assert_ne!(r1, r2);
    }
}
