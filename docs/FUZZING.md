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

## Fourth wave: the third command stream

`third_wave_commands_hold_invariants` (10 seeds × 200 commands) mixes
the second-wave generator with MM tier enrollment, a live vault revenue
share, quote interest, and a mid-run UTC day crossing. It asserts:

* **I-5 conservation** with vault collateral in the tracked universe,
  through fee routing, vault revenue splits, quote interest, funding,
  and liquidations;
* **I-3 replay determinism** across every new command;
* **I-27 tier legality** — a nonzero discount implies enrollment and
  respects the cap, checked after *every* command.

Findings: **none** — the wave-4 code paths survived their first
property run. (The wave-3 credit for catching real defects belongs to
the suite's predecessors; the bar this wave had to clear was not
regressing them.)

## Implied matching (`fuzz_implied_matching`)

The combined-order fuzz target drives random command sequences —
plain orders (GTC/IOC/FOK), combined orders (1–2 legs, random
structures/ratios/sides/limits/tifs), cancels, cancel-alls, oracle
moves, ticks, and quotes/executions against auto-published RFQs —
through an option-wing venue with implied matching enabled. The same
generator runs as a seeded property suite
(`crates/engine/tests/implied_properties.rs`, 32 seeds × 96 commands)
in the stable CI profile, so continuous fuzzing and CI check the same
invariants (I-62..I-68):

* **conservation** of the entry-anchored conserved quantity (options
  and perps are both premium-unpaid in this venue, so the position
  entry term covers every instrument) up to per-lot rounding dust;
* **no self-trades** across every fill;
* **position-model agreement** — a shadow model recomputed from
  journaled trades (plus the non-trade movers: liquidations, ADL,
  expiry, migration) must equal the engine's positions, which catches
  lot-ratio drift, double-fills, and phantom fills;
* **book sanity** — leg books never cross at rest; the combo book's
  indexes stay consistent; every open combo holds a reservation;
* **replay determinism** — a shadow engine fed identical commands
  lands on the identical fingerprint.

**Track record:** the first run of the property suite caught a real
defect — the combo-vs-combo opponent scan did not exclude the taker's
own resting combos, allowing a self-trade (I-63). Fixed with the
owner filter in `opponents_in_priority_order`; the suite keeps the
regression. The initial conservation checker also had to learn the
venue's premium-unpaid option model — the corrected law (entry terms
for ALL instruments) is now the property's conserved quantity.

Measured coverage: 16,765 runs / 210 s clean (libFuzzer, ASAN);
sibling targets re-verified after the combo integration:
`fuzz_matching` 20,616 runs / 60 s, `fuzz_fees` 3.49 M runs / 46 s,
`fuzz_oracle` 19,314 runs / 31 s.
