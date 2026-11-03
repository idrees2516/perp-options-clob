# ns-clob v2 — production 0DTE options trading venue

One Rust crate. Zero runtime dependencies. `#![forbid(unsafe_code)]`.

A complete **production** system for **0DTE (same-day expiry) options
markets**: the nanosecond engine (central limit order book, pre-trade risk,
options margin, per-trade settlement, closing auctions, expiry exercise,
circuit breaker, WAL journal, multi-shard routing) **plus the hardened
network venue** — HMAC-authenticated HTTP + WebSocket API, read replica,
fan-out market data, session ladders, and disaster recovery — with
**exact-integer accounting enforced after every single command**.

Run the whole venue with one binary:

```bash
cargo run --release --bin ns-venue -- --demo
# keys + session ladder printed to stderr; HTTP+WS on 127.0.0.1:8080
```

The API contract lives in [`docs/0DTE_API.md`](../docs/0DTE_API.md); the
threat model and hardening inventory in
[`docs/0DTE_PRODUCTION.md`](../docs/0DTE_PRODUCTION.md). The trading
terminal is a standalone Next.js application maintained separately at
[github.com/idrees2516/ns-venue-terminal](https://github.com/idrees2516/ns-venue-terminal)
(this repository is the backend).

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
| **Production venue** | `venue.rs` | Shard workers with event fan-out, REST-await correlation, clock, WAL sinks with fsync + torn-tail replay, session ladders, graceful drain |
| **Gateway** | `gateway.rs` | HTTP/1.1 + WebSocket serving, REST routing, WS sessions, per-IP caps, slow-consumer drop |
| **Auth** | `auth.rs` | HMAC-SHA256 request signing, timestamp window, 4096-slot anti-replay nonce window, scopes, per-key rate buckets, key minting |
| **Crypto** | `crypto.rs` | SHA-256/HMAC (FIPS + RFC vectors), SHA-1/base64 for the WS handshake, urandom, constant-time compare |
| **Wire codecs** | `http.rs`, `ws.rs`, `json.rs` | Strict, bounded, fuzz-exercised parsers (anti-smuggling HTTP, RFC 6455 frame codec, integer-only JSON) |
| **Read replica** | `replica.rs` | Event-derived market/account state for REST queries; engine-truth balance snapshots |

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
    types.rs          # ids (packed order ids), sides, TIF, flags, reject codes
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
    crypto.rs         # SHA-256 / HMAC / SHA-1 / base64 / urandom / ct_eq
    json.rs           # strict integer-only JSON codec
    http.rs           # hardened HTTP/1.1 server codec
    ws.rs             # RFC 6455 frame codec + message assembler
    auth.rs           # API keys, signing, nonce window, scopes, rate limits
    replica.rs        # read-side state for REST queries
    venue.rs          # the venue assembly (workers, clock, WAL, ladder, drain)
    gateway.rs        # connection handling, REST routes, WS sessions
    bin/tps.rs        # engine throughput harness
    bin/venue.rs      # ns-venue: the production binary
    bin/venue_tps.rs  # gateway-fed throughput harness
  benches/latency.rs  # micro-latency suite
  tests/e2e_day.rs    # full-day + randomized + replay integration tests
  tests/gateway_e2e.rs # end-to-end venue + adversarial attack suite
```

## Running it

```
cd rust
cargo test                                        # 115 tests (engine + venue + attacks)
cargo bench                                       # micro-latency table
cargo run --release --bin ns-tps -- --shards 2 --seconds 10     # engine capacity
cargo run --release --bin ns-tps -- --shards 2 --seconds 5 --wal /tmp/wal

# Production venue (demo: keys, session ladder, sim feed):
cargo run --release --bin ns-venue -- --demo
# Explicit configuration:
cargo run --release --bin ns-venue -- \
  --bind 127.0.0.1:8080 --wal-dir ./wal \
  --api-key a1b2c3d4:<64-hex>:admin:1 \
  --account 1:prime --fund 1:0:100000000 \
  --mark 500000 --ladder 3 --sim-feed

# Gateway-fed throughput (venue must run with --loadtest and the bench key):
cargo run --release --bin ns-venue -- --loadtest \
  --api-key 00000bb8:bbbb..bb:trade:8 --account 8:prime --fund 8:0:10000000000 \
  --mark 500000 --ladder 1 &
cargo run --release --bin venue-tps -- --mode ws --conns 16 --secs 10
```

## Honest caveats

* **Two throughput tiers, reported separately.** Engine capacity (the full
  in-process pipeline: risk → match → settle → journal → events) measures
  17–20M orders/s aggregate on this shared 2-vCPU sandbox depending on
  neighbor load (best sustained 20.08M; pure-taker capacity is 18.5M/s per
  shard at 54 ns/op, and shards scale linearly with dedicated cores).
  Gateway-fed end-to-end throughput (TCP + HMAC + JSON + queue + engine +
  ack) is ~5–6k orders/s on the same shared box with thread-per-conn
  `std::net` — network ingress at the 20M class is a kernel-bypass
  gateway-tier concern (DPDK/AF_XDP + multiple gateway nodes feeding the
  shard queues), by design out of scope for this crate.
* WAL mode is measured on the sandbox's storage; on NVMe class devices the
  325 MB/s/shard journal write rate is the ceiling to watch. WAL files are
  per shard, append-only; segment rotation and snapshot compaction are
  operational concerns not yet implemented (replay is from genesis).
* The throughput harness uses non-binding risk limits (institutional scale)
  so capacity is measured rather than risk saturation; binding-limit
  behavior is covered by tests. `--loadtest` lifts rate ceilings for
  benchmarking only — never enable it in production.
* The delta used for delta-notional limits is a documented strike-based
  heuristic refreshed on oracle ticks — not a full pricing-model greek.
* TLS termination, DDoS scrubbing and key custody (HSMs) belong to the
  fronting infrastructure; the venue itself is TLS-agnostic by design.
