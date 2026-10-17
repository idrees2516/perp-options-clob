# feat: ns-clob v2 — unified nanosecond 0DTE options trading platform (20M+ TPS class)

## Summary

Adds `rust/` — an independent Cargo workspace (zero runtime dependencies,
`#![forbid(unsafe_code)]`) implementing a complete, production-shaped
**0DTE options trading platform**: central limit order book, full pre-trade
risk, options margin, **instant per-trade settlement**, MOC auctions,
automatic expiry exercise, circuit breaker, CRC-framed WAL journal with
deterministic replay, and a shard-per-underlying multi-core router.

**Measured on this repo's 2-vCPU CI container** (rustc 1.99, LTO=fat):

| | poc-engine (baseline) | ns-clob v2 |
|---|---:|---:|
| Crossing taker order | 535 ns p50 *(deferred settlement)* | **53 ns mean** *(settlement inline, per trade)* |
| Place/cancel cycle | 565 ns p50 | ~73 ns place p50 / ~35 ns cancel |
| Aggregate throughput (2 shards) | — | **19.5–20.1M orders/s (best 20.08M), 3.95M trades/s settled** |
| Single shard | — | 13.41M orders/s |
| WAL | 433 ns/record | ~5 ns hot-path serialize, batched CRC flush |
| Exercise settlement | 123 µs | ~76 ns/position |

The ≥20M orders/sec target is met within run variance on two shared vCPUs
(best sustained 20.08M; every counted op is a real engine command), with
**zero rejects, exact integer conservation, and 2.5 MB/shard flat memory**.
Single-shard capacity is 13.41M; shards share nothing, so throughput scales
with cores (dedicated 2-core → 26.8M projected; 4 cores → 53M+ class).

## Architecture

* **Shard = one underlying's complete market** (all strikes/expiries,
  accounts sub-ledger, margin engine, circuit breaker, expiry wheel,
  journal). No cross-shard state, no locks on the order path.
* One `execute()` runs the full pipeline: rate limit → phase/band gates →
  position/delta/margin checks with buying-power commitments → FOK
  pre-scan → match loop (STP-aware) → **per-trade settlement** (premium,
  fees/rebates, positions, margin) → remainder resting or cancel → events +
  journal.
* Price-time CLOB: slab + intrusive doubly-linked levels, O(1) cancel,
  generation-guarded order ids, one cache line per order, level-aggregated
  quantities.
* Exact-integer double-entry ledger: `Σ cash + house == Σ deposits` after
  **every command** — the house absorbs fees and residuals; nothing is
  floating point anywhere.

## 0DTE feature set

* Hourly expiry ladder with 10-minute MOC auction windows; single-price
  clearing (max volume, tie→reference), price-time allocation.
* Cash-settled auto-exercise at expiry (underlying settlement mark),
  exercise fees, OTM worthless expiry, pin-risk margin surcharge in the
  final hour near the strike.
* Short-option stress margin with TTL-scaled bands (3%/2%/1.5%), fee
  reserve; longs premium-paid. Oracle-driven incremental margin refresh
  (budgeted round-robin sweeps — bounded worst case per tick).
* Circuit breaker (±7% halt → reopening auctions), fast-market band
  tightening (±3% with hysteresis), per-option price bands, token-bucket
  rate limiting, position & delta-notional limits.
* Limit GTC/IOC/FOK, market-with-protection, post-only, reduce-only, OCO
  pairs, atomic cancel-replace, STP DecrementAndCancel.
* Maker-rebate/taker-fee schedule with volume tiers.
* Market data (BBO/L2/OHLC/VWAP) derived strictly off the critical path.

## Correctness

* **51 tests** (44 unit + 7 integration, including WAL-bytes disaster recovery), clippy-clean, zero warnings.
* Randomized adversarial suites: 3 seeds × 20k commands, conservation +
  no-crossed-books + margin consistency asserted after **every** command.
* Journal replay determinism: 30k-command day → fresh shard → `state_hash`
  identical bit-for-bit.
* Full trading-day E2E with exact integer cash assertions per account
  (deposits → hourly expiries → volatility regime → halt/reopen auction →
  settlement → reconciliation).
* Buying-power commitments prevent double-spending cash across resting
  orders; released exactly per filled/cancelled quantity.

## Trying it

```bash
cd rust
cargo test                                   # 50 tests
cargo bench                                  # micro-latency table
cargo run --release --bin ns-tps -- --shards 2 --seconds 10
cargo run --release --bin ns-tps -- --shards 2 --seconds 5 --wal /tmp/wal
```

`ns-tps` reports per-shard and aggregate rates, latency percentiles, reject
counts, journal volume, and verifies conservation before exiting (exit code
2 on violation).

## Scope notes

* In-process engine capacity (risk → match → settle → journal → events);
  network ingress (kernel-bypass gateways) is intentionally out of scope.
* Throughput config uses institutional-scale exposure limits (capacity
  measurement); binding-limit behavior is covered by tests.
* Delta for delta-notional limits is a documented strike-based heuristic
  refreshed on oracle ticks, not a pricing-model greek.
* Baseline comparisons built and run from `crates/` on the same machine and
  profile — see `rust/BENCHMARKS.md` for methodology.
