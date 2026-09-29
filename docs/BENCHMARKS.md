# Benchmarks

`poc-bench micro` (release build, deterministic inputs, ns/op with
percentiles). No external harness — the binary is dependency-free and
its results reproducible on any machine; numbers below are from the
2-vCPU CI container this repository was developed in, so treat
*magnitudes* as the signal.

## Methodology

* Fixed iteration counts (20k–60k) per benchmark; `Instant`-based
  timing per operation; p50/p99/max/mean reported.
* The engine is a full venue (margin, fees, oracle, journaling) — the
  micro numbers include the complete pre-trade risk path, not a
  stripped order book.
* Release profile: thin LTO, `codegen-units = 1`.

## Results (2 vCPU container, rustc 1.98.1)

| Benchmark | p50 | p99 | Interpretation |
|---|---|---|---|
| place/cancel churn (8 accounts) | 539 ns | 14.6 µs | ~1.0M ops/s sustained incl. risk + journal |
| crossing taker vs 200 resting | 506 ns | 15.1 µs | full match + settle path |
| oracle provider update | 648 ns | 1.0 µs | cluster-consensus recompute |
| tick sweep | 700 ns | 0.9 µs | full planner pipeline at low load |
| settlement merkle root (2,000 accounts) | 1.77 ms | 1.9 ms | full-tree recompute; batch-window cost |
| WAL append (unsynced) | 432 ns | 0.6 µs | framed + chained + CRC |
| WAL full recovery (50k commands) | 6.5 ms total | — | ~7.7M commands/s replay |

## Second closure wave (G-05/08/09/10)

Added with the OCO/TWAP/batch/vol-index closure wave, same container:

| Benchmark | p50 | p99 | Interpretation |
|---|---|---|---|
| OCO bracket place (incl. double gate) | 7.0 µs | 158 µs | pair validity + cumulative margin + link + cascade scan |
| TWAP open + one slice tick | 1.5 µs | 6.2 µs | parent creation + marker + child placement |
| batch of 3 (gate + sequential commit) | 0.67 µs | 2.2 µs | atomic gate + per-member commit |
| tick (incl. vol index + collateral interest) | 1.2 µs | 3.3 µs | the two new sweep stages are ~100 ns of the tick |

The OCO premium over a plain place (~7 µs vs ~0.6 µs) is the *correct*
cost of atomicity: the pair runs the full hypothetical-fill margin gate
twice plus the cumulative-reservation simulation before either leg is
committed. Brackets are a retail-flow feature at human frequency; the
venue charges the microseconds where it buys a guarantee.

## Reading the numbers

* **Matching throughput.** ~1M place/cancel ops/s and sub-microsecond
  takers mean the engine core is not the bottleneck for a single market
  well past 10k active users. In the Lighter/dYdX lineage the scaling
  axis is *markets per process* (one deterministic core per
  instrument), which this design supports directly — see
  `docs/EFFICIENCY.md`.
* **Settlement roots.** The merkle root is O(n) over leaves per
  recompute (1.77 ms at 2k accounts). At batch cadence (one commit per
  block window, not per trade) this is well inside budget; incremental
  root updates are the documented optimization path.
* **WAL.** Append cost is the frame codec + CRC, intentionally
  memcpy-class. Recovery replays at millions of commands per second,
  making checkpoint cadence a storage decision, not a recovery-time
  decision.

## Fourth-wave subsystems (2026-09, same container)

| Operation | p50 | p99 | Notes |
|---|---|---|---|
| MM tier sample + review tick (64 MMs) | 43.0 µs | 92.4 µs | One liquidity-scoring tick + one review-boundary tick: the monthly review of 64 market makers costs less than one settlement merkle root |
| PoR build + prove all (64 accounts) | 2.78 ms | 3.08 ms | Liability-tree construction over 64 accounts plus verification of every account's inclusion proof |
| FIX session lifecycle (logon + order + logout) | 6.2 µs | 11.6 µs | Full session choreography over the in-memory duplex: framing, sequence assignment, checksums, logout exchange |


## v0.6 — American exercise & the channel book (2026-09, same container)

| Benchmark | p50 | p99 | Interpretation |
|---|---|---|---|
| **taker vs 2,000-order deep book (lazy)** | **485 ns** | 637 ns | channel-lazy walk: depth behind the touch is free |
| **auction uncross (10k orders, 121 levels)** | — | — | 9.1 ms total, O(L log L) prefix-sum clearing (incl. journaling) |
| **BAW American mark (put, r=3%)** | 10.7 µs | 15.5 µs | boundary bisection included; **81 ns at the venue's r=0 default** (collapses to European) |
| European BSM mark (reference) | 81 ns | 89 ns | the pricing floor |
| **Merton perpetual American** | 23 ns | 24 ns | closed form — the everlasting τ→∞ anchor |
| **exercise settlement sweep (63 shorts)** | 119 µs | — | TWAP strike + pro-rata assignment + fee routing, once per window |
| **book commitment (1,000 resting orders)** | 1.2 ms | 1.4 ms | provable-book hash chain, batch-window cost |

Reading the two headline rows together: the channel rewrite made depth
free — the deep-book taker (485 ns) is *faster* than the eager walk
against a tenth of the liquidity, because the lazy walk terminates at
the first level that fills it. And American pricing costs nothing under
the venue's zero-rate default (BAW ≡ BSM, asserted by test), engaging
its ~130× premium only when `risk_free_rate > 0` is configured — a full
200-market American mark pass is ~2 ms, batch-window territory.

## Reproducing (v0.6 additions)

```bash
cargo run --release -p poc-bench -- micro
cargo run --release -p poc-bench -- stress   # docs/STRESS_TESTING.md
```

Run on a quiet machine; the container's p99s include scheduler noise
(the p50s are the trustworthy signal).
