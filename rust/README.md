# ns-clob v2 — unified nanosecond 0DTE options trading stack

One Rust crate. Zero runtime dependencies. `#![forbid(unsafe_code)]`.

A complete venue-side engine for **0DTE (same-day expiry) options markets**:
central limit order book, pre-trade risk, options margin, per-trade
settlement, closing auctions, expiry exercise, circuit breaker, WAL journal,
multi-shard routing — with **exact-integer accounting enforced after every
single command** and **20M+ orders/sec aggregate throughput** on two vCPUs.

## What it is

| Layer | Module | Highlights |
|---|---|---|
| Order book | `book.rs` | Price-time CLOB; slab + intrusive levels; O(1) cancel; one cache line per order; generation-guarded ids |
| Shard engine | `engine.rs` | One underlying's full market: gates → match → settle → bookkeeping in one call |
| Risk & margin | `risk params` in `margin.rs`, gates in `engine.rs` | Premium rule for longs, stress-scenario margin for shorts, pin-risk surcharge, delta-notional limits, price bands, fast-market, circuit breaker, rate limiting |
| Settlement | `engine.rs` | **Per-trade** premium/fee/position/margin settlement — no deferred batch window |
| Auctions | `auction.rs` | Single-price MOC clearing (max volume, tie → reference), price-time allocation |
| Expiry | `expiry.rs` | Minute-resolution timing wheel; auto-exercise (cash-settled European), settlement fee, position close-out, margin release |
| Journal | `journal.rs` | CRC32-framed WAL (memory / file / discard sinks), deterministic replay, `state_hash` |
| Market data | `marketdata.rs` | BBO/L2/OHLC/VWAP derived off the critical path |
| Platform | `platform.rs` | Shard-per-underlying router, batched queues, worker threads |
| Simulation | `sim.rs` | Deterministic agents for tests and throughput runs |

## The numbers (measured on a 2-vCPU shared sandbox)

Micro-latency (`cargo bench`, batch-timed, journal discard mode):

| op | mean | p50 | p99 | p99.9 |
|---|---:|---:|---:|---:|
| taker full pipeline (match+settle+journal+events) | **53 ns** | 53 ns | 64 ns | 72 ns |
| maker placement near touch | 84 ns | 77 ns | 157 ns | 260 ns |
| cancel + requote cycle | 163 ns | 151 ns | 274 ns | 287 ns |
| FOK pre-scan (8 levels) | 21 ns | 20 ns | 32 ns | 33 ns |
| BBO query | **2.1 ns** | | | |
| depth top-10 query | 4.3 ns | | | |
| short margin per contract | 0.9 ns | | | |
| auction clearing (32 levels) | 551 ns | | | |
| expiry settlement (8 positions) | 637 ns | | | |
| state hash (full audit) | 11.5 µs | | | |
| conservation check | 3.4 ns | | | |

Throughput (`ns-tps`, mixed 0DTE flow — 60% MM requotes, 30% takers, 10%
pulls, inventory-aware mean-reverting agents):

| configuration | orders/sec | trades/sec settled | notes |
|---|---:|---:|---|
| 1 shard | 13.4M | 2.65M | per-op p50 71 ns |
| **2 shards (2 vCPU)** | **19.5–20.1M** (best 20.08M) | **3.95M** | 0 rejects, exact conservation, 2.5 MB RSS |
| 2 shards + file WAL | 7.6–9.8M | ~2M | CRC-framed journal to disk, ~260–325 MB/s per shard |

Scaling is by construction: shards share **nothing** (no locks, no atomics on
the order path), so N shards scale linearly up to core count and memory
bandwidth. The 2-vCPU sandbox shows ~75% dual-core scaling efficiency (the
shared-vCPU tax); on dedicated cores 2×13.4M = 26.8M, and 4 shards on 4
cores = 53M+ class. The `ns-tps` harness extrapolates nothing — it counts
every command processed (each one a real engine command) and audits
conservation at the end.

## Correctness model

* **Exact-integer double-entry**: `sum(account.cash) + house == deposits`
  after *every* command — verified after every operation in randomized
  suites, and at the end of every throughput run.
* **Deterministic replay**: the WAL (command log) replayed into a fresh
  shard reproduces state **bit-for-bit** (`state_hash` equality is a test).
* **Buying-power commitments**: buy orders hold cash, sell orders hold
  margin at admission; released *exactly* per filled/cancelled quantity —
  no double-spend of resting order funding.
* **Margin consistency**: `account.margin_required == Σ position.margin`
  is an audited invariant.
* 51 tests: 44 unit + 7 integration (full trading day with exact cash
  assertions, randomized adversarial suites × 3 seeds × 20k commands,
  replay determinism, circuit-breaker/auction lifecycle, cancel-replace,
  withdrawal bounds).

## 0DTE feature set

* Hourly expiries (10:00–16:00 ladder) with 10-minute MOC auction windows.
* Cash settlement at expiry using the underlying settlement mark;
  auto-exercise of ITM (≥ 1 tick intrinsic), exercise fee, OTM expires
  worthless; pin-risk margin surcharge in the final hour near the strike.
* Short-option stress margin with TTL-scaled bands (3% ≤ 30 min, 2% ≤ 2 h,
  1.5% beyond), fee reserve, oracle-driven incremental refresh (budgeted
  round-robin sweeps).
* Circuit breaker: ±7% underlying move halts the shard; reopening auctions
  per symbol; fast-market band tightening at ±3% (hysteresis).
* Order types: limit GTC/IOC/FOK, market-with-protection, post-only,
  reduce-only, OCO pairs, atomic cancel-replace.
* STP DecrementAndCancel; price bands around the option's own mark; rate
  limiting (token bucket); per-series position and delta-notional limits.
* Fees: maker rebate / taker charge per contract with volume tiers.

## Layout

```
rust/
  Cargo.toml          # workspace = this crate, zero deps, forbid(unsafe)
  src/
    types.rs          # ids (packed order ids), sides, TIF, flags
    symbol.rs         # 0DTE listing codec (u64 key), intrinsic, delta
    fees.rs           # maker/taker/settle/exercise schedule + tiers
    accounts.rs       # accounts, positions, commitments, ledger, buckets
    book.rs           # the CLOB
    margin.rs         # options margin math
    journal.rs        # events, CRC32 WAL framing, sinks, replay support
    engine.rs         # Shard: the whole venue for one underlying
    expiry.rs         # timing wheel
    auction.rs        # single-price clearing
    marketdata.rs     # derived feed state
    platform.rs       # multi-shard router + workers
    sim.rs            # deterministic market simulator
    bin/tps.rs        # the throughput harness
  benches/latency.rs  # micro-latency suite
  tests/e2e_day.rs    # full-day + randomized + replay integration tests
```

## Running it

```
cd rust
cargo test                          # 50 tests
cargo bench                         # micro-latency table
cargo run --release --bin ns-tps -- --shards 2 --seconds 10
cargo run --release --bin ns-tps -- --shards 2 --seconds 5 --wal /tmp/wal   # durability mode
```

## Honest caveats

* Throughput numbers are **engine capacity** of the full in-process
  pipeline (risk → match → settle → journal serialize → events). Network
  ingress (kernel-bypass gateways) is out of scope for this crate.
* WAL mode is measured on the sandbox's storage; on NVMe class devices the
  325 MB/s/shard journal write rate is the ceiling to watch.
* The throughput harness uses non-binding risk limits (institutional scale)
  so capacity is measured rather than risk saturation; binding-limit
  behavior is covered by tests.
* The delta used for delta-notional limits is a documented strike-based
  heuristic refreshed on oracle ticks — not a full pricing-model greek.
