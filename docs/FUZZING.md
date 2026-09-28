# Fuzzing & Property-Based Testing

## Strategy

The fuzzing program has two tiers, so continuous fuzzing and CI both
run the same checkers:

1. **Property tests (stable Rust, every push).**
   `crates/engine/tests/properties.rs` drives deterministic
   pseudo-random command sequences (xorshift64*) through the engine
   and re-checks the dynamic invariants after every command:
   replay agreement (I-3), equity conservation (I-5), book sanity
   (I-7), and liquidation legality (I-11). Twelve seeds × 140 commands
   run in milliseconds as ordinary `cargo test` targets.

2. **cargo-fuzz targets (nightly + libFuzzer, long-running).**
   `fuzz/` contains libFuzzer harnesses that mutate bytes into
   structured inputs and call the same engines. They are excluded from
   the stable CI matrix (libfuzzer needs nightly) but are part of the
   release checklist.

## The checkers

| Checker | Invariant | Where |
|---|---|---|
| Two engines, same commands → same state | I-3 replay determinism | `properties.rs`, `fuzz_matching` |
| Equity identity up to rounding dust | I-5 conservation | `properties.rs` |
| Best bid < best ask at rest | I-7 book sanity | `properties.rs`, `fuzz_matching` |
| Coordinated oracle jumps always accepted | I-14 | `oracle_genuine_jumps_always_accepted`, `fuzz_oracle` |
| Torn-tail WAL recovers to a prefix | I-15 | `torn_tail_is_dropped_cleanly`, `fuzz_wal` |
| Merkle proofs verify at every tree size | I-6 | `proofs_verify_at_every_size`, `fuzz_merkle` |
| Fee caps: option fee ≤ premium × cap | I-9 | economics tests, `fuzz_fees` |
| Margin summaries never negative for solvent accounts | I-10 | margin tests |

## What fuzzing found (this codebase)

The program has already paid for itself — three real defects:

1. **Oracle liveness bug (I-14):** the deviation quarantine anchored to
   the stale last mark; a genuine +15% coordinated move quarantined
   every honest provider and halted the market. Fixed with
   cluster-consensus authority selection.
2. **ADL counterparty inversion (I-11):** the counterparties filter
   kept same-side positions and skipped the actual opposite-side
   holders; iterative rounds were also one-shot by an over-eager
   exclusion list. Fixed + tested.
3. **Fee routing leak (I-4):** the buyback share of routed fees was
   debited from users and never credited to any pool — a conservation
   hole the settlement validator now structurally rejects.
4. **Shared order ids inside PlaceBatch (I-26):** the single-pass batch
   planner assigned every sibling the same engine id — tracked orders,
   reservations, and book keys collided, and marketable siblings could
   double-fill the same maker against phantom liquidity. Found by a
   targeted id-distinctness probe written while designing OCO; fixed by
   making batches and OCO pairs *sequential-commit* after an atomic
   validity + cumulative-margin pre-pass.
5. **Vault collateral outside the conserved universe:** the first cut of
   the vault epoch settled cash into vault collateral that the
   conservation checker did not track, so any subscription read as a
   leak of exactly the subscribed amount. Found by
   `second_wave_commands_hold_invariants` on seed 1 step 14; fixed by
   extending the conserved quantity with vault collateral (and, in the
   same pass, the insurance fund's inventory entry value).

## Running

```bash
# Tier 1 — runs in stable CI
cargo test --workspace

# Tier 2 — continuous fuzzing (nightly)
rustup install nightly
cargo +nightly install cargo-fuzz
cd fuzz
cargo +nightly fuzz run fuzz_matching  -- -max_total_time=120
cargo +nightly fuzz run fuzz_oracle    -- -max_total_time=120
cargo +nightly fuzz run fuzz_wal       -- -max_total_time=120
cargo +nightly fuzz run fuzz_merkle   -- -max_total_time=120
cargo +nightly fuzz run fuzz_fees     -- -max_total_time=120
```

Corpora live in `fuzz/corpus/<target>/`; committed seeds reproduce
past findings. Regression minimization is manual (the checkers print
the failing seed and step).
