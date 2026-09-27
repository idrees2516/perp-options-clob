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

## Reproducing

```bash
cargo run --release -p poc-bench -- micro
cargo run --release -p poc-bench -- stress   # docs/STRESS_TESTING.md
```

Run on a quiet machine; the container's p99s include scheduler noise
(the p50s are the trustworthy signal).
