//! # poc-settlement
//!
//! The on-chain settlement layer for the perpetuals + options CLOB
//! (gap-register **G-30**), built on the pattern shared by Lighter.xyz,
//! dYdX v4, and Hyperliquid: **state-diff batches with merkleized
//! commitments and an exit escape hatch**.
//!
//! ## Why state diffs
//!
//! Settling every trade on-chain is O(trades); a perp-options venue
//! clears hundreds of thousands of fills per hour. Instead, the operator:
//!
//! 1. runs the deterministic engine off-chain (see `poc-engine`);
//! 2. every window, captures the venue state before and after
//!    ([`diff::StateCapture`]) and derives the per-account mutation list
//!    — the *state diff*, O(changed accounts);
//! 3. publishes a [`batch::SettlementBatch`]: `prev_root → new_root`,
//!    the mutations' merkle root, and a hash-chained header;
//! 4. users verify their balance leaf against the committed root with a
//!    merkle inclusion proof ([`merkle`]), and exit through the
//!    force-window queue ([`exit::ExitQueue`]) if the operator stalls.
//!
//! ## Guarantees
//!
//! * **No silent forks** — `prev_root` chaining plus header hashes
//!   ([`batch::validate_chain`]).
//! * **No forged states** — replaying mutations must reproduce the
//!   claimed root ([`batch::validate_batch`]).
//! * **No unexplained mints** — tracked value moves only by the declared
//!   custodial residual (deposits − withdrawals); fees, funding, and
//!   PnL are zero-sum *inside* the tracked universe.
//! * **No hostage funds** — withdrawal intents past their force window
//!   become provable neglect ([`exit::ExitQueue::neglected`]).
//!
//! ## Hashing
//!
//! SHA-256 throughout (dependency-free, FIPS 180-4; see [`hash`]), with
//! domain separation between leaves (`POC-LEAF`), nodes (`POC-NODE`),
//! operation leaves (`POC-OP`), batch headers (`POC-BATCH`), and
//! proof-of-reserves liability leaves (`POC-POR`).

pub mod batch;
pub mod diff;
pub mod exit;
pub mod hash;
pub mod merkle;
pub mod por;
pub mod state;

pub use batch::{build_batch, validate_batch, validate_chain, SettlementBatch, ValidationError};
pub use diff::StateCapture;
pub use exit::{ExitQueue, WithdrawalIntent};
pub use por::{
    build_report_from_rows, check_solvency, report_commitment, verify_liability, FixedAttestor,
    LiabilityEntry, LiabilityTree, PorError, PorLedger, PorReport, ReserveAttestor, SolvencyCheck,
};
pub use state::{
    AccountCommitment, AccountMutation, SettlementState, HOUSE_SUBACCOUNT, INSURANCE_SUBACCOUNT,
    REWARDS_SUBACCOUNT, USER_SUBACCOUNT_CEILING,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AccountMutation;
    use std::collections::BTreeMap;

    use poc_core::{Instrument, PerpMarket, Side};
    use poc_engine::{Command, Engine, EngineConfig, OrderRequest};

    fn engine_with_market() -> Engine {
        let mut e = Engine::new(EngineConfig::default());
        e.register_instrument(Instrument::Perp(PerpMarket::default()));
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: 1_000,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: 1_000 });
        e
    }

    #[test]
    fn full_pipeline_batches_chain_and_validate() {
        let mut e = engine_with_market();

        // Window 0: deposits.
        let before = StateCapture::capture(&e);
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 10_000_000,
        });
        let after = StateCapture::capture(&e);
        let b0 = build_batch(0, SettlementState::empty().root(), &before, &after, 2_000);

        // Window 1: a trade.
        let before1 = after;
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 79_950, 3),
            now: 2_001,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_960, 3),
            now: 2_002,
        });
        let after1 = StateCapture::capture(&e);
        let b1 = build_batch(1, b0.new_root, &before1, &after1, 2_500);

        // Chain linkage holds.
        let final_root = validate_chain(b0.prev_root, &[b0.clone(), b1.clone()]).unwrap();
        assert_eq!(final_root, b1.new_root);

        // Tampering with any header breaks it.
        let mut forged = b1.clone();
        forged.new_root = [9_u8; 32];
        assert!(matches!(
            validate_chain(b0.prev_root, &[b0.clone(), forged]),
            Err(ValidationError::HeaderHashMismatch)
        ));

        // Full replay validation: reconstruct the pre-state (users + venue
        // pools, exactly as committed in batch 0), replay batch 1's
        // mutations, reach the claimed root.
        let mut pre = SettlementState::empty();
        for (sub, leaf) in before1.with_pools() {
            let _ = sub;
            pre.upsert(leaf);
        }
        assert_eq!(pre.root(), b1.prev_root);
        let post = validate_batch(&pre, &b1, 0).unwrap();
        assert_eq!(post.root(), b1.new_root);
    }

    #[test]
    fn unexplained_flow_is_rejected() {
        let mut pre = SettlementState::empty();
        pre.apply(&AccountMutation {
            subaccount: 1,
            cash_delta_quote_minor: 1_000,
            position_deltas: BTreeMap::new(),
        });
        // Batch claims 0 custodial flow but mints 500 into account 1.
        let mut post = pre.clone();
        post.apply(&AccountMutation {
            subaccount: 1,
            cash_delta_quote_minor: 500,
            position_deltas: BTreeMap::new(),
        });
        let batch = SettlementBatch {
            sequence: 0,
            prev_root: pre.root(),
            new_root: post.root(),
            mutations: vec![AccountMutation {
                subaccount: 1,
                cash_delta_quote_minor: 500,
                position_deltas: BTreeMap::new(),
            }],
            custodial_residual: 0,
            ts: 1,
            hash: [0; 32],
        };
        let mut batch = batch;
        batch.hash = batch.compute_hash();
        assert!(matches!(
            validate_batch(&pre, &batch, 0),
            Err(ValidationError::UnexplainedFlow { .. })
        ));
        // Declaring the flow honestly makes it valid.
        let mut honest = batch.clone();
        honest.custodial_residual = -500;
        honest.hash = honest.compute_hash();
        assert!(validate_batch(&pre, &honest, -500).is_ok());
    }
}
