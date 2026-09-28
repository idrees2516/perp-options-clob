//! Proof-of-reserves (G-35): liability commitment, per-account
//! inclusion proofs, and the reserve-attestation integration point.
//!
//! ## What proof-of-reserves buys
//!
//! A derivatives venue holds customer balances and tells customers it
//! holds them. Proof-of-reserves is how it *shows* it: the venue
//! publishes a cryptographic commitment to its complete liability set,
//! every customer verifies their own balance is inside the commitment,
//! and an external attestation of actual reserves (on-chain wallet
//! sign-overs, bank letters, custodian statements) proves the total
//! committed liabilities are covered. Hyperliquid made the pattern
//! culture in this market segment; the merkle-tree mechanics are the
//! same ones the settlement batch uses, applied to a different ledger.
//!
//! ## The construction
//!
//! * **Liability entries** — one per subaccount: quote cash, every
//!   non-quote collateral balance at its full oracle value, and vault
//!   share claims at NAV. Liabilities are what customers can *claim*,
//!   so they are measured pre-haircut: the venue's haircut is its own
//!   risk buffer, not the customer's.
//! * **Nonce binding** — the report nonce is hashed into every leaf,
//!   which makes every publication's root unique even for identical
//!   balance sets (replay-proof) and lets an auditor confirm a root was
//!   minted *for this report*, not recycled from an older, more
//!   favourable snapshot.
//! * **Merkle root** — the domain-separated tree from
//!   [`crate::merkle`], leaves sorted by subaccount id
//!   (deterministic ordering, no adversary-chosen leaf order).
//! * **Inclusion proofs** — every account can verify itself against
//!   the published root without trusting the operator's database.
//! * **[`PorLedger`]** — the venue-side append-only publication
//!   history: nonces and timestamps strictly increase, roots are
//!   advisory (a repeat root means "nothing changed", which is fine).
//! * **[`ReserveAttestor`]** — the pluggable integration point where
//!   on-chain reserve proofs, custodian attestations, or auditor letters
//!   plug in; the ledger itself never asserts solvency, it only
//!   computes the comparison.
//!
//! ## What it deliberately is not
//!
//! The ledger does not prove the engine's internal accounting — that
//! is the settlement batch's job ([`crate::batch`]). It proves the
//! *customer-facing liability set*: the balances the venue would have
//! to cover if every customer withdrew at once (at oracle prices, the
//! only prices that can be committed to deterministically).

use crate::hash::sha256;
use crate::merkle::{inclusion_proof, leaf_hash, merkle_root, InclusionProof};
use std::collections::BTreeMap;

const POR_LEAF_TAG: &[u8] = b"POC-POR";

/// One account's provable liability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiabilityEntry {
    /// The subaccount the liability belongs to.
    pub subaccount: u64,
    /// Quote-currency cash owed to the account, quote minor.
    pub quote_cash_minor: u128,
    /// Non-quote collateral held for the account: `(currency code,
    /// balance in the currency's minor units, balance valued at the
    /// full — un-haircut — oracle price, quote minor)`.
    pub collateral: Vec<(String, u128, u128)>,
    /// Vault share claims valued at current NAV, quote minor.
    pub vault_claims_quote_minor: u128,
}

impl LiabilityEntry {
    /// The entry's total liability, quote minor.
    #[must_use]
    pub fn total_quote_minor(&self) -> u128 {
        self.quote_cash_minor
            .saturating_add(self.vault_claims_quote_minor)
            .saturating_add(self.collateral.iter().map(|(_, _, v)| v).sum())
    }

    /// Deterministic leaf payload: domain tag || subaccount || nonce ||
    /// quote || vault claims || collateral count || (len-prefixed code,
    /// minor, quote value)*.
    ///
    /// Length prefixes are u32 big-endian, integers u128/u64
    /// big-endian — fixed-width, no ambiguity, byte-identical on every
    /// platform.
    #[must_use]
    pub fn leaf_payload(&self, nonce: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.collateral.len() * 48);
        out.extend_from_slice(POR_LEAF_TAG);
        out.extend_from_slice(&self.subaccount.to_be_bytes());
        out.extend_from_slice(&nonce.to_be_bytes());
        out.extend_from_slice(&self.quote_cash_minor.to_be_bytes());
        out.extend_from_slice(&self.vault_claims_quote_minor.to_be_bytes());
        let count = u32::try_from(self.collateral.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&count.to_be_bytes());
        for (code, minor, value) in &self.collateral {
            let code_bytes = code.as_bytes();
            let len = u32::try_from(code_bytes.len()).unwrap_or(u32::MAX);
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(code_bytes);
            out.extend_from_slice(&minor.to_be_bytes());
            out.extend_from_slice(&value.to_be_bytes());
        }
        out
    }
}

/// A built liability tree: entries, hashes, root, and total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiabilityTree {
    /// Entries sorted ascending by subaccount id.
    pub entries: Vec<LiabilityEntry>,
    /// Leaf hashes in the same order as `entries`.
    pub hashes: Vec<[u8; 32]>,
    /// The report nonce every leaf was bound to.
    pub nonce: u64,
    /// Sum of all entry totals, quote minor.
    pub total_liabilities_quote_minor: u128,
}

impl LiabilityTree {
    /// Build the tree from an unsorted iterator of entries.
    ///
    /// Duplicate subaccounts are rejected (the liability set is a set,
    /// not a multiset — a venue that owes one account twice owes it
    /// once).
    pub fn build(
        entries: impl IntoIterator<Item = LiabilityEntry>,
        nonce: u64,
    ) -> Result<Self, PorError> {
        let mut map: BTreeMap<u64, LiabilityEntry> = BTreeMap::new();
        for entry in entries {
            if map.insert(entry.subaccount, entry).is_some() {
                return Err(PorError::DuplicateSubaccount);
            }
        }
        let entries: Vec<LiabilityEntry> = map.into_values().collect();
        let hashes: Vec<[u8; 32]> = entries
            .iter()
            .map(|e| leaf_hash(&e.leaf_payload(nonce)))
            .collect();
        let total_liabilities_quote_minor = entries
            .iter()
            .map(LiabilityEntry::total_quote_minor)
            .fold(0_u128, u128::saturating_add);
        Ok(Self {
            entries,
            hashes,
            nonce,
            total_liabilities_quote_minor,
        })
    }

    /// The merkle root of the liability set.
    #[must_use]
    pub fn root(&self) -> [u8; 32] {
        merkle_root(&self.hashes)
    }

    /// The inclusion proof for one subaccount's liability.
    #[must_use]
    pub fn proof(&self, subaccount: u64) -> Option<(LiabilityEntry, InclusionProof)> {
        let index = self
            .entries
            .iter()
            .position(|e| e.subaccount == subaccount)?;
        let proof = inclusion_proof(&self.hashes, index)?;
        Some((self.entries[index].clone(), proof))
    }
}

/// One published proof-of-reserves report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PorReport {
    /// Merkle root of the liability tree.
    pub root: [u8; 32],
    /// Total liabilities the root commits to, quote minor.
    pub total_liabilities_quote_minor: u128,
    /// Publication nonce (hash-bound into every leaf).
    pub nonce: u64,
    /// Publication timestamp, ms.
    pub ts: u64,
    /// Number of liability entries in the tree.
    pub entry_count: u32,
}

/// Proof-of-reserves errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PorError {
    /// Two entries carried the same subaccount.
    DuplicateSubaccount,
    /// A report violated the ledger's monotonicity (nonce or ts).
    NonMonotonic,
    /// The attestation could not be produced.
    Attestation,
}

impl std::fmt::Display for PorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PorError::DuplicateSubaccount => {
                write!(f, "duplicate subaccount in liability set")
            }
            PorError::NonMonotonic => {
                write!(f, "publication nonce/timestamp must strictly increase")
            }
            PorError::Attestation => write!(f, "reserve attestation unavailable"),
        }
    }
}

impl std::error::Error for PorError {}

/// The venue-side append-only publication ledger.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PorLedger {
    /// Published reports, oldest first.
    pub reports: Vec<PorReport>,
}

impl PorLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a report. Nonce and timestamp must strictly increase —
    /// the publication history is totally ordered, so an auditor can
    /// detect a withheld or reordered report by comparing ledgers.
    pub fn publish(&mut self, report: PorReport) -> Result<(), PorError> {
        if let Some(last) = self.reports.last() {
            if report.nonce <= last.nonce || report.ts <= last.ts {
                return Err(PorError::NonMonotonic);
            }
        }
        self.reports.push(report);
        Ok(())
    }

    /// The latest published report, if any.
    #[must_use]
    pub fn latest(&self) -> Option<&PorReport> {
        self.reports.last()
    }

    /// The complete history (audit view).
    #[must_use]
    pub fn history(&self) -> &[PorReport] {
        &self.reports
    }
}

/// Verify one account's liability against a published root.
///
/// This is the customer-side check: with the venue's published root,
/// their own entry, and the inclusion proof, they confirm their
/// balance is committed. `expected_nonce` must match the report the
/// root came from — a proof from a different report does not verify.
#[must_use]
pub fn verify_liability(
    entry: &LiabilityEntry,
    proof: &InclusionProof,
    root: &[u8; 32],
    expected_nonce: u64,
) -> bool {
    let leaf = leaf_hash(&entry.leaf_payload(expected_nonce));
    proof.verify(&leaf, root)
}

/// The external reserve-attestation integration point.
///
/// Implementations produce the *reserve side* of the solvency
/// comparison: on-chain wallet sign-overs (the Hyperliquid-style
/// reserve addresses signing the report nonce), custodian letters, or
/// auditor statements. The trait deliberately returns a quote-minor
/// amount and an error channel — attestation quality is the
/// implementor's problem; the ledger only compares numbers.
pub trait ReserveAttestor {
    /// The attested reserve value backing `report`, quote minor.
    fn attested_reserve_quote_minor(&mut self, report: &PorReport) -> Result<u128, PorError>;
}

/// The result of comparing an attestation against a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolvencyCheck {
    /// The attested reserve value, quote minor.
    pub attested_reserve_quote_minor: u128,
    /// The report's committed liabilities, quote minor.
    pub total_liabilities_quote_minor: u128,
}

impl SolvencyCheck {
    /// Whether reserves cover liabilities.
    #[must_use]
    pub fn solvent(&self) -> bool {
        self.attested_reserve_quote_minor >= self.total_liabilities_quote_minor
    }

    /// Coverage ratio in permille (attested / liabilities; saturating
    /// at 1000 when liabilities are zero and reserves positive).
    #[must_use]
    pub fn coverage_permille(&self) -> u64 {
        if self.total_liabilities_quote_minor == 0 {
            return if self.attested_reserve_quote_minor > 0 {
                1_000
            } else {
                0
            };
        }
        let scaled = self.attested_reserve_quote_minor.saturating_mul(1_000);
        u64::try_from(scaled / self.total_liabilities_quote_minor).unwrap_or(u64::MAX)
    }
}

/// Run one solvency comparison: attestation vs report.
pub fn check_solvency(
    attestor: &mut dyn ReserveAttestor,
    report: &PorReport,
) -> Result<SolvencyCheck, PorError> {
    let attested = attestor.attested_reserve_quote_minor(report)?;
    Ok(SolvencyCheck {
        attested_reserve_quote_minor: attested,
        total_liabilities_quote_minor: report.total_liabilities_quote_minor,
    })
}

/// A test/staging attestation: reports a fixed reserve value, with an
/// optional nonce it "signs" (the stand-in for on-chain sign-overs).
#[derive(Debug, Clone)]
pub struct FixedAttestor {
    /// Reserve value to attest, quote minor.
    pub reserve_quote_minor: u128,
    /// The report nonce this attestation covers (checked).
    pub signed_nonce: u64,
}

impl ReserveAttestor for FixedAttestor {
    fn attested_reserve_quote_minor(&mut self, report: &PorReport) -> Result<u128, PorError> {
        if report.nonce != self.signed_nonce {
            return Err(PorError::Attestation);
        }
        Ok(self.reserve_quote_minor)
    }
}

/// Convenience: the root-hash link between a tree and its report —
/// `sha256("POC-POR-REPORT" || root || nonce || ts || total)`, the
/// string a venue publishes on-chain / in the auditor letter so the
/// published commitment and the merkle root are bound together.
#[must_use]
pub fn report_commitment(report: &PorReport) -> [u8; 32] {
    let mut buf = Vec::with_capacity(16 + 32 + 16 + 16 + 16);
    buf.extend_from_slice(b"POC-POR-REPORT");
    buf.extend_from_slice(&report.root);
    buf.extend_from_slice(&report.nonce.to_be_bytes());
    buf.extend_from_slice(&report.ts.to_be_bytes());
    buf.extend_from_slice(&report.total_liabilities_quote_minor.to_be_bytes());
    sha256(&buf)
}

/// Build a liability tree and its report from the engine's
/// proof-of-reserves projection (G-35 glue).
///
/// The engine exposes [`poc_engine::Engine::por_liabilities`] — the
/// read-only liability rows (positive quote cash, non-quote collateral
/// at full oracle value, vault share claims at NAV). This function
/// turns those rows into the committed tree and the publishable
/// report. Publication is deliberately a *settlement-layer* act: it
/// reads engine state, commits it, and never mutates the engine — a
/// projection, not a transaction.
///
/// # Errors
/// [`PorError::DuplicateSubaccount`] should be impossible from the
/// engine's per-account rows (defensive only).
pub fn build_report_from_rows(
    rows: Vec<poc_engine::PorLiabilityRow>,
    nonce: u64,
    ts: u64,
) -> Result<(LiabilityTree, PorReport), PorError> {
    let entries: Vec<LiabilityEntry> = rows
        .into_iter()
        .map(|row| LiabilityEntry {
            subaccount: row.subaccount,
            quote_cash_minor: row.quote_cash_minor,
            collateral: row.collateral,
            vault_claims_quote_minor: row.vault_claims_quote_minor,
        })
        .collect();
    let tree = LiabilityTree::build(entries, nonce)?;
    let report = PorReport {
        root: tree.root(),
        total_liabilities_quote_minor: tree.total_liabilities_quote_minor,
        nonce,
        ts,
        entry_count: u32::try_from(tree.entries.len()).unwrap_or(u32::MAX),
    };
    Ok((tree, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sub: u64, quote: u128) -> LiabilityEntry {
        LiabilityEntry {
            subaccount: sub,
            quote_cash_minor: quote,
            collateral: Vec::new(),
            vault_claims_quote_minor: 0,
        }
    }

    #[test]
    fn every_entry_proves_against_the_root() {
        let entries = vec![entry(1, 100), entry(2, 250), entry(3, 7)];
        let tree = LiabilityTree::build(entries, 42).unwrap();
        let root = tree.root();
        let report = PorReport {
            root,
            total_liabilities_quote_minor: tree.total_liabilities_quote_minor,
            nonce: 42,
            ts: 1_000,
            entry_count: tree.entries.len() as u32,
        };
        for e in &tree.entries {
            let (entry, proof) = tree.proof(e.subaccount).unwrap();
            assert!(verify_liability(&entry, &proof, &report.root, report.nonce));
        }
    }

    #[test]
    fn wrong_nonce_fails_verification() {
        let entries = vec![entry(1, 100)];
        let tree = LiabilityTree::build(entries, 42).unwrap();
        let (entry, proof) = tree.proof(1).unwrap();
        // A proof bound to nonce 42 must not verify against the same
        // root when the auditor checks nonce 43.
        assert!(!verify_liability(&entry, &proof, &tree.root(), 43));
        assert!(verify_liability(&entry, &proof, &tree.root(), 42));
    }

    #[test]
    fn duplicates_rejected() {
        let entries = vec![entry(1, 100), entry(1, 200)];
        assert_eq!(
            LiabilityTree::build(entries, 0).unwrap_err(),
            PorError::DuplicateSubaccount
        );
    }

    #[test]
    fn total_matches_sum_of_entries() {
        let mut e1 = entry(1, 100);
        e1.vault_claims_quote_minor = 50;
        e1.collateral = vec![("BTC".into(), 1_000, 65_000)];
        let e2 = entry(2, 10);
        let tree = LiabilityTree::build(vec![e1, e2], 1).unwrap();
        assert_eq!(tree.total_liabilities_quote_minor, 100 + 50 + 65_000 + 10);
    }

    #[test]
    fn balance_change_changes_the_root() {
        let tree_a = LiabilityTree::build(vec![entry(1, 100), entry(2, 5)], 7).unwrap();
        let tree_b = LiabilityTree::build(vec![entry(1, 101), entry(2, 5)], 7).unwrap();
        assert_ne!(tree_a.root(), tree_b.root());
        // Same balances, different nonce -> different root too.
        let tree_c = LiabilityTree::build(vec![entry(1, 100), entry(2, 5)], 8).unwrap();
        assert_ne!(tree_a.root(), tree_c.root());
    }

    #[test]
    fn entries_sorted_by_subaccount() {
        let tree = LiabilityTree::build(vec![entry(9, 1), entry(2, 1), entry(5, 1)], 1).unwrap();
        let ids: Vec<u64> = tree.entries.iter().map(|e| e.subaccount).collect();
        assert_eq!(ids, vec![2, 5, 9]);
    }

    #[test]
    fn ledger_enforces_monotonic_publication() {
        let mut ledger = PorLedger::new();
        let r1 = PorReport {
            root: [1; 32],
            total_liabilities_quote_minor: 10,
            nonce: 1,
            ts: 100,
            entry_count: 1,
        };
        let r2 = PorReport {
            root: [2; 32],
            total_liabilities_quote_minor: 10,
            nonce: 2,
            ts: 200,
            entry_count: 1,
        };
        assert!(ledger.publish(r1).is_ok());
        assert!(ledger.publish(r2).is_ok());
        assert_eq!(ledger.history().len(), 2);
        // Nonce regression rejected.
        let bad = PorReport {
            root: [3; 32],
            total_liabilities_quote_minor: 10,
            nonce: 2,
            ts: 300,
            entry_count: 1,
        };
        assert_eq!(ledger.publish(bad).unwrap_err(), PorError::NonMonotonic);
        // Timestamp regression rejected.
        let bad = PorReport {
            root: [3; 32],
            total_liabilities_quote_minor: 10,
            nonce: 3,
            ts: 200,
            entry_count: 1,
        };
        assert_eq!(ledger.publish(bad).unwrap_err(), PorError::NonMonotonic);
        assert_eq!(ledger.latest().unwrap().nonce, 2);
    }

    #[test]
    fn solvency_comparison_math() {
        let mut attestor = FixedAttestor {
            reserve_quote_minor: 1_500,
            signed_nonce: 5,
        };
        let report = PorReport {
            root: [0; 32],
            total_liabilities_quote_minor: 1_000,
            nonce: 5,
            ts: 1,
            entry_count: 1,
        };
        let check = check_solvency(&mut attestor, &report).unwrap();
        assert!(check.solvent());
        assert_eq!(check.coverage_permille(), 1_500);
        // Under-reserved.
        let report = PorReport {
            root: [0; 32],
            total_liabilities_quote_minor: 3_000,
            nonce: 5,
            ts: 1,
            entry_count: 1,
        };
        let check = check_solvency(&mut attestor, &report).unwrap();
        assert!(!check.solvent());
        assert_eq!(check.coverage_permille(), 500);
        // Wrong nonce: attestation does not apply.
        let report = PorReport {
            root: [0; 32],
            total_liabilities_quote_minor: 1_000,
            nonce: 6,
            ts: 1,
            entry_count: 1,
        };
        assert!(check_solvency(&mut attestor, &report).is_err());
    }

    #[test]
    fn report_commitment_binds_root_and_nonce() {
        let tree = LiabilityTree::build(vec![entry(1, 100)], 9).unwrap();
        let report = PorReport {
            root: tree.root(),
            total_liabilities_quote_minor: 100,
            nonce: 9,
            ts: 50,
            entry_count: 1,
        };
        let c1 = report_commitment(&report);
        let mut changed = report.clone();
        changed.nonce = 10;
        assert_ne!(c1, report_commitment(&changed));
        changed = report.clone();
        changed.ts = 51;
        assert_ne!(c1, report_commitment(&changed));
    }

    #[test]
    fn collateral_valued_in_the_leaf() {
        let mut e = entry(1, 0);
        e.collateral = vec![("ETH".into(), 5_000_000_000_000, 15_000)];
        let payload = e.leaf_payload(1);
        // Domain tag present; layout: tag(8) sub(8) nonce(8) quote(16)
        // vault(16) count(4) then length-prefixed entries.
        assert!(payload.starts_with(POR_LEAF_TAG));
        let count_offset = POR_LEAF_TAG.len() + 8 + 8 + 16 + 16;
        assert_eq!(
            &payload[count_offset..count_offset + 4],
            &1_u32.to_be_bytes()
        );
        // The "ETH" code follows with its u32 length prefix.
        let code_len_offset = count_offset + 4;
        assert_eq!(
            &payload[code_len_offset..code_len_offset + 4],
            &3_u32.to_be_bytes()
        );
        assert_eq!(&payload[code_len_offset + 4..code_len_offset + 7], b"ETH");
    }
}
