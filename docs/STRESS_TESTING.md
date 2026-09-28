# Stress Testing

`poc-bench stress` (release build) runs whole-architecture scenarios
against a live engine and asserts the invariants hold under each. All
scenarios are deterministic — a failure is reproducible from the seed
baked into the harness.

## Scenarios

### Flash crash (−50% in 100 ticks)
Oracle falls $80k → $40k in 10 seconds of engine time with resting
liquidity seeded. Asserts: no panics, no crossed books, liquidation
cascade stays deficit-ordered.

### Volatility spike (500 coordinated swings)
Three providers swing the mark ±6% every 50ms. Asserts: the mark
survives every tick (cluster consensus never wedges), no halts.

### Order spam (40k place/cancel)
Adversarial churn against 4 accounts. Asserts: books stay uncrossed,
memory behavior stays bounded, ~620k ops/s sustained.

### Liquidation cascade (20 degens, empty insurance fund)
Twenty thin-margin longs crash into an empty insurance fund with
market-maker liquidity on the other side. Asserts: partial-before-full
ordering, ADL rounds cap per-counterparty closures, final accounting
consistent (I-11).

### Oracle split (1 rogue of 3)
Two honest providers hold $80k; a rogue prints $400k every tick for
300 ticks. Asserts: the mark never moves to the rogue, the venue never
halts while quorum holds (soft quarantine, I-14).

### Crash-recovery determinism
6,004 commands (deposits + orders + oracle setup) written to a WAL,
recovered into a fresh engine. Asserts: account fingerprints identical
(I-3, I-15).

## Latest results (2 vCPU container, release build)

```text
flash-crash (-50% in 100 ticks)     1.3ms   301 events, final spot ~$40,000
vol-spike (500 coordinated swings)  6.8ms   mark survived 500/500 ticks
order-spam (40k place/cancel)     64.5ms   6,957 resting, books sane
liquidation-cascade (20 degens)    5.0ms   223 liquidation events
oracle-split (1 rogue of 3)        1.5ms   mark held 300/300, 0 halts
crash-recovery determinism        13.4ms  6004/6004 commands, state identical
all stress scenarios completed without invariant violations
```

## Interpreting

* Latencies are per-scenario totals, not per-event ceilings; the
  per-operation distribution lives in `docs/BENCHMARKS.md`.
* The liquidation-cascade cash delta (−5.956M minor) is the degens'
  realized losses leaving their accounts to MMs/insurance/ADL
  counterparties — conservation is asserted by the property suite
  (I-5), not re-derived here.
* Numbers are from a shared 2-vCPU container: treat magnitudes, not
  decimals, as the signal. Reproduce with `cargo run --release -p
  poc-bench -- stress`.

## Wave-4 re-run (2026-09)

All six scenarios re-executed after the fourth closure wave (MM tiers,
vault revenue split, quote-interest machinery, FIX transport, PoR) with
the same seeds:

| Scenario | Result |
|---|---|
| flash-crash (−50% in 100 ticks) | 1.4 ms, 301 events, no invariant violations |
| vol-spike (500 coordinated swings) | 7.8 ms, mark survived 500/500 ticks |
| order-spam (40k place/cancel) | 67 ms, 6,957 resting, books sane |
| liquidation-cascade (20 degens) | 5.9 ms, 465 liquidation events |
| oracle-split (1 rogue of 3) | 1.7 ms, mark held 300/300, zero halts |
| crash-recovery determinism | 13.3 ms, 6,004/6,004 commands, state identical |

**No regressions.** The sweep's two new stages (MM review, quote
interest) add no measurable cost to the hot paths at these scales —
see the fourth-wave table in `docs/BENCHMARKS.md` for their isolated
cost.
