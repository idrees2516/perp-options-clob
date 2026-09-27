//! Merkle proofs must verify for every leaf of every tree shape, and
//! reject wrong leaves.
#![no_main]

use libfuzzer_sys::fuzz_target;
use poc_settlement::{inclusion_proof, leaf_hash, merkle_root};

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let n = 1 + usize::from(data[0]) % 24;
    let leaves: Vec<[u8; 32]> = (0..n)
        .map(|i| leaf_hash(&[data[(i + 1) % data.len()], i as u8]))
        .collect();
    let root = merkle_root(&leaves);
    for i in 0..n {
        if let Some(proof) = inclusion_proof(&leaves, i) {
            assert!(proof.verify(&leaves[i], &root));
            let wrong = leaf_hash(&[0xFF]);
            assert!(!proof.verify(&wrong, &root));
        }
    }
});
