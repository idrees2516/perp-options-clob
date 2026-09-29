# Efficiency & Scale — ideas adopted from Lighter (and where we differ)

[Lighter](https://lighter.xyz) demonstrated that a ZK-rollup venue can
sustain hundreds of thousands of TPS by (1) settling *state diffs*, not
trades, on-chain, and (2) engineering the matching core for throughput
first, with cheap verification as the fallback. This document records
what we adopted, what we adapted, and what we deliberately did not.

## Adopted: state-diff settlement (G-30, `poc-settlement`)

**Lighter's insight:** the L1 does not need trades; it needs
*commitments users can exit against*. Our `SettlementBatch` is the
state-diff pattern exactly:

```text
window: commands → deterministic engine → state before/after
        ↓
StateCapture.diff_to → AccountMutation[]  (O(changed accounts), not O(trades))
        ↓
SettlementBatch { prev_root, new_root, ops_root, hash-chained header }
```

* On-chain footprint is O(changed accounts) per window.
* Merkle inclusion proofs let users verify their leaf against the
  committed root (`state.rs`).
* The exit queue with a force window is the escape hatch
  (`exit.rs`) — proof-of-neglect if the operator stalls.

**Where we differ:** Lighter posts validity proofs (ZK); we follow the
dYdX/Hyperliquid escape-hatch model (fraud-window + forced exits),
which is honest for this stage and keeps the trust assumptions
explicit. A validity-proof adapter slots in behind
`validate_batch`'s interface.

## Adopted: one deterministic core per market

Lighter isolates matching per market in a single-writer core. Our
engine is already a pure command→event machine with **zero shared
mutable state during planning** (`plan(&self)` is read-only; mutations
happen only in `apply_event`). Deployment shape:

```text
             ┌─ engine core (BTC markets) ── WAL-A ─┐
gateway ──▶  router                                 ├──▶ settlement batcher ─▶ L1
             └─ engine core (ETH markets) ── WAL-B ┘
```

Each core has its own WAL and replays independently; cross-market
portfolios settle at the settlement layer (the margin engine stays
per-core with per-underlying risk universes, the Derive V3 model).

## Adopted: throughput-first hot path

Measured results of these choices are in `docs/BENCHMARKS.md`:

* **Integer-only money** — no decimal/float ledger path anywhere
  (f64 confined to Black-Scholes analytics off the hot loop).
* **Pre-allocated planner buffers** — the sweep stages reuse `Vec`s
  across ticks; event batches amortize allocations.
* **Single mutator** — `apply_event` does no allocation-heavy dispatch
  tricks; replay is memcpy-class (7.7M commands/s recovery).
* **Cheap checks first** — the pre-trade gate orders checks by cost
  (symbol → qty → band → greeks → hypothetical margin clone).

## Adapted: snapshots instead of ZK state diffs

Lighter's operators prove state transitions; our WAL + journal gives
the same auditability at process level: any watcher can replay the
public command log and compare roots (I-3 + I-4). The
`poc-settlement` validator is the watcher's toolkit.

## Not adopted (and why)

| Lighter idea | Why not |
|---|---|
| ZK validity proofs per batch | Requires a prover stack; escape-hatch + audit replay achieves the trust goals for this stage |
| Custom database engine | The WAL is intentionally boring (framed appends); boring is a feature in the failure path |
| CLOB-in-circuit matching | Our determinism property (I-3) gives replay-auditability without circuit constraints |

## Efficiency follow-ups

1. **Incremental merkle roots** — recompute is O(n) per batch; an
   append-only frontier makes updates O(log n) at the cost of
   implementation complexity.
2. **Per-lot cost basis** — makes conservation exact (see I-5's
   rounding-dust note).
3. **io_uring WAL writes** — the `File` + `sync_all` path is portable
   but not optimal on Linux.
4. **Sharded settlement captures** — diff computation parallelizes
   trivially per account range.

## v0.3 addendum: the channel layout, realized

The earlier section below ("what we would take from Lighter when the
matching core needs it") is no longer hypothetical — v0.3 took it, with
one addition the original notes undersold:

**Channels (the zkLighter data structure).** Every price level is now
a `VecDeque` of 8-slot `Channel`s, each caching its live order count
and visible-quantity sum, with the sums cached again per level. The
payoffs, measured (`docs/BENCHMARKS.md`):

1. **Aggregate queries dropped a complexity class.** `best_touch_sizes`
   — the vol-surface gate that runs per option market per tick — went
   from O(resting orders) to **O(1)**. `depth(n)` went from O(orders in
   top n levels) to O(n). `available_within` (FOK feasibility) went
   from O(orders within limit) to O(levels within limit).
2. **Matching became lazy.** The old `match_taker` materialized the
   full reachable-candidate list before walking it; the new one pulls
   through the channel chain and stops when the taker fills. A taker
   that fills at the touch of a 2,000-order book now costs **485 ns
   p50 — less than the old eager walk against 200 orders (506 ns)**:
   depth behind the touch is free until you reach it.
3. **Cancels tombstone.** A cancel touches the order's slot and the two
   cached aggregates, then drops the channel if it empties. Survivors
   never move — price-time priority is stable under churn, which is
   exactly what the replay/audit guarantees need.
4. **The uncross went from O(L²) to O(L log L).** Prefix-sum
   supply/demand curves evaluate each candidate price by binary
   search; 10,000 auction orders across 121 levels uncross (with full
   event journaling) in 9.1 ms.

**The rule that makes it correct** (and the bug the first
implementation shipped): arrivals append strictly *after the last live
slot* — never into an earlier tombstone gap. Filling gaps would let an
iceberg reslice resurrect inside its old slot and leapfrog live orders.
The tail-append rule, the aggregate invariants, and the churn
differential test pin this (`docs/INVARIANTS.md` I-26..I-29).

**Provable book state.** zkLighter's deepest idea — that the book
itself should be a provable object — now has a concrete analog:
`poc-settlement::BookCommitment` hash-chains every resting order in
canonical order (1.2 ms per 1,000 orders, batch-window cost), so any
verifier can check the book it is shown against the published root,
and replayed engines must agree bit-for-bit. That is the zk-*ready*
line we chose to stop at: the same deterministic transition function
a circuit would prove, delivered as a hash commitment today.
