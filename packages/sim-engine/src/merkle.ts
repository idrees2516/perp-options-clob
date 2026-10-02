/**
 * Merkleized liabilities — proof-of-reserves (G-35).
 * Domain tags mirror poc-settlement: "POC-LEAF", "POC-NODE".
 */

import { bytesToHex, hexToBytes, sha256Tagged } from "./sha256";

export interface InclusionProof {
  /** Sibling hashes from leaf to root. */
  siblings: { hash: string; right: boolean }[];
  leafIndex: number;
}

/**
 * Build a merkle tree over hex-encoded leaves.
 * Odd nodes duplicate the last (bitcoin-style padding).
 */
export function buildMerkle(leaves: string[]): { root: string } {
  if (leaves.length === 0) return { root: sha256Tagged("POC-LEAF", "") };
  let level = leaves.slice();
  while (level.length > 1) {
    const next: string[] = [];
    for (let i = 0; i < level.length; i += 2) {
      const left = level[i]!;
      const right = i + 1 < level.length ? level[i + 1]! : left;
      next.push(sha256Tagged("POC-NODE", `${left}${right}`));
    }
    level = next;
  }
  return { root: level[0]! };
}

/** Inclusion proof for leaf i (0-based) against the leaf list. */
export function proveInclusion(leaves: string[], index: number): InclusionProof {
  const siblings: { hash: string; right: boolean }[] = [];
  let level = leaves.slice();
  let idx = index;
  while (level.length > 1) {
    const siblingIdx = idx % 2 === 0 ? (idx + 1 < level.length ? idx + 1 : idx) : idx - 1;
    siblings.push({
      hash: level[siblingIdx]!,
      right: idx % 2 === 0, // sibling is on the right when we're the left node
    });
    const next: string[] = [];
    for (let i = 0; i < level.length; i += 2) {
      const left = level[i]!;
      const right = i + 1 < level.length ? level[i + 1]! : left;
      next.push(sha256Tagged("POC-NODE", `${left}${right}`));
    }
    level = next;
    idx = Math.floor(idx / 2);
  }
  return { siblings, leafIndex: index };
}

/** Verify an inclusion proof against a claimed leaf and root. */
export function verifyInclusion(leaf: string, proof: InclusionProof, root: string): boolean {
  let acc = leaf;
  for (const sib of proof.siblings) {
    acc = sib.right
      ? sha256Tagged("POC-NODE", `${acc}${sib.hash}`)
      : sha256Tagged("POC-NODE", `${sib.hash}${acc}`);
  }
  return acc === root;
}

/** Merkle root over domain-tagged leaf payloads. */
export function rootOfTaggedLeaves(payloads: string[]): string {
  const leaves = payloads.map((p) => sha256Tagged("POC-LEAF", p));
  return buildMerkle(leaves).root;
}

/** Hex digest of raw bytes — re-exported for the report hash chain. */
export { bytesToHex, hexToBytes };
