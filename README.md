# perp-options-clob

A **perpetuals + American-style options central limit order book** engine in
Rust (zkLighter-style channel matching): the matching core, portfolio margin,
oracle defense, funding economics, and liquidation cascade of a production
venue — as a **deterministic, replayable, event-sourced** library. Plus, on
[`feat/0dte-ns-clob-platform`](rust/), the **nanosecond CLOB**: a unified
0DTE options trading platform measured at **53 ns per taker fill** and
**20M+ orders/sec** on two vCPUs.

The architecture has two design goals that shape every layer: **nanosecond
matching efficiency** and **provable execution on zkVMs** — the proof systems
being [Lattice Jolt](https://a16zcrypto.com/posts/article/lattice-snarks-jolt-post-quantum-faster)
(a16z crypto) with [Akita](https://github.com/LayerZero-Labs/akita)
(LayerZero Labs) as its lattice commitment backend, and
[Magnetar](https://www.microsoft.com/en-us/research/blog/proof-based-general-purpose-computing-with-magnetar)
(Srinath Setty, Microsoft Research), the hash-based post-quantum zkVM. Both
are detailed in
[Why this architecture is zkVM-provable](#why-this-architecture-is-zkvm-provable).

```
cargo run -p poc-demo               # watch a full session: quoting → trading → funding → crash → liquidation → expiry → MM tiers → vault revenue → proof-of-reserves
cargo test --workspace               # 420 unit + integration + property tests, journal-replay determinism included
cargo run --release -p poc-bench -- micro    # benchmarks (docs/BENCHMARKS.md)
cargo run --release -p poc-bench -- stress    # architecture stress scenarios (docs/STRESS_TESTING.md)

git checkout feat/0dte-ns-clob-platform && cargo run --release --bin ns-venue -- --demo
                                     # the nanosecond 0DTE venue: HTTP+WS on :8080, keys printed to stderr
```

## Why this exists

Crypto derivatives venues are converging on one architecture: an off-chain
matching engine whose every decision is a journaled event, margin computed
over the whole portfolio rather than per position, prices defended by oracle
redundancy, and liquidations backstopped by an insurance fund rather than
socialized losses. This workspace implements that stack end-to-end, with the
design choices that dominate production — and the reasons for each —
documented where they are made.

---

## Architecture

### The determinism model

Every state mutation flows through a single function:
`Engine::apply_event(&Event)`. Commands are **planned** against an immutable
snapshot into a `Vec<Event>` (`plan(&self)` — pure, no side effects), then
**applied** one by one (`apply(&mut self)`) — and the same vector is appended
to the journal. The consequences:

- **The journal *is* the state machine.** `Engine::replay(config, journal)`
  rebuilds books, positions, balances, statistics and hashes bit-for-bit.
  The same command stream always produces the same event journal, and
  replaying that journal into a fresh engine reproduces the state exactly —
  asserted by test (`engine::tests::determinism_and_replay`).
- **Live trading and event replay provably execute the same fills.** The
  matching core is side-effect-free (`match_taker` plans fills without
  mutating the book — the flood-rs / dYdX v4 `plan/apply` discipline), so
  the audit path and the trading path are the same code.
- **Every subsystem is driven by events, not shared state.** Settlement
  batches, governance, the read replica (ns venue), the terminal UI — all
  consume the journal. One source of truth.

```
            Command (place / cancel / oracle / deposit / tick)
                              │
                    ┌─────────▼─────────┐
                    │      Engine       │   plan(&self)   → Vec<Event>   (pure decisions)
                    │  (poc-engine)     │   apply(&mut)   → state        (single mutator)
                    └─────────┬─────────┘
        ┌──────────┬──────────┼───────────┬────────────┬─────────────┐
        ▼          ▼          ▼           ▼            ▼             ▼
   orderbook    oracle     margin      risk        economics     accounts
   (match/     (median/   (SFPM      (pre-trade,  (fees, funding, (positions,
    apply)      quarantine) scenarios)  liquidation)  routing,       cash, PnL)
                                                       rewards)
        └──────────┴──────────┴───────────┴────────────┴─────────────┘
                              │
                    ┌─────────▼─────────┐
                    │   Event journal   │  TradeExecuted · FundingPaid · Liquidation ·
                    │ (append-only)     │  OptionExpiry · RewardPaid · MarketHalted …
                    └─────────┬─────────┘
                              │  replay() reconstructs state bit-for-bit
                ┌─────────────┼─────────────────┐
                ▼             ▼                 ▼
        settlement diffs   read replica     trading terminal
        (merkleized,      (REST queries)   (sim + live gateway)
         on-chain)
```

### Design sources

| System | What was taken |
|---|---|
| [flood-rs](https://github.com/paradigmxyz/flood-rs) (Paradigm) | BTreeMap price-level layout with FIFO queues; pure-match/apply separation |
| [Everlasting Options](https://www.paradigm.xyz/2021/05/everlasting-options) (Paradigm) | The roll-payment mechanism for never-expiring options; the funding framing of option markets |
| Derive V3 (formerly Lyra) | Subaccount-level **portfolio margin**; SFPM-style scenario margin anchored to the base futures margin; premium-unpaid option accounting; 30-minute TWAP expiry settlement |
| dYdX v4 | Off-chain **deterministic** matching + oracle-based margining; the plan/apply discipline; tiered fee ladder |
| BitMEX | The premium + interest funding model with clamps; the ADL concept |
| Deribit | Liquidation as a cascade (book → insurance); the liquidation penalty funding the insurance backstop; subaccount margining; the DVOL-style vol index |
| CME SPAN | The scenario-grid concept; the short-option minimum charge |
| zkLighter / Lighter.xyz | Channel orderbook; state-diff settlement with merkleized commitments ([docs/research/ZKLIGHTER.md](docs/research/ZKLIGHTER.md)) |
| Paradigm / Derive RFQ | Multi-dealer request-for-quote flow, block trades, delayed tape |

### The two stacks

| | `crates/` (main) | `rust/` ([`feat/0dte-ns-clob-platform`](#the-nanosecond-clob--feat0dte-ns-clob-platform)) |
|---|---|---|
| Purpose | The **reference engine**: complete venue semantics, audit-grade determinism, 420 tests | The **nanosecond 0DTE platform**: one crate, zero runtime dependencies, production venue |
| Domain | Perps + European/American/everlasting options | Same-day-expiry (0DTE) options markets |
| Time | Venue ticks (discrete sweeps) | Logical **nanoseconds** |
| Throughput class | Audit-first | **53 ns taker fill · 20M+ orders/s on 2 vCPU** |
| Persistence | Framed CRC'd chain-hashed WAL + checkpoints (G-24) | CRC32-framed WAL, fsync batches, torn-tail crash recovery |
| Networking | Gateway protocol library (`poc-api`) + FIX 4.4 acceptor (`poc-fix`) | In-crate hardened HTTP/1.1 + WebSocket venue with read replica |
| Terminals | [perp-options-clob-frontend](https://github.com/idrees2516/perp-options-clob-frontend) (submodule) | [ns-venue-terminal](https://github.com/idrees2516/ns-venue-terminal) |

---

## The crates — complete reference

16 workspace crates, each with a single defining decision, no dependencies
between them that isn't an explicit domain boundary. Workspace-wide:
`unsafe_code = "forbid"`, `missing_docs = "warn"`, `unwrap/expect/panic =
warn`.

### `poc-core` — domain primitives

The vocabulary everything else speaks. **Money is exact**: every amount is
a `u128` minor unit through checked `mul_div` with an explicit rounding
direction; `f64` never touches a ledger (Black-Scholes analytics exist, but
they are analytics — marks only, never posted). Inside:

- Identifiers (account, order, market, instrument) as newtypes.
- Order semantics: `Side`, `Tif` (GTC/IOC/FOK), order kinds (limit, market,
  stop-market, stop-limit, trailing-market, trailing-limit, iceberg with
  display qty, post-only, reduce-only).
- Instrument definitions: perp markets, dated European/American options
  (strike, expiry, exercise style), everlasting options.
- The numeric spec per market: tick size, lot size, base decimals — the
  `tick × lot / 10^base_decimals` convention every crate shares.

### `poc-orderbook` — CLOB matching

Price-time priority central limit order book (Paradigm/Derive/dYdX v4/
Hyperliquid standard). **Pure match, separate apply**: `match_taker` is
side-effect-free — it plans fills against an immutable book snapshot, so
live trading and event replay provably execute the same fills. Includes
batch auctions (Dutch open/close), self-trade prevention (STP
DecrementAndCancel), and the channel-orderbook layout for off-chain
matching with on-demand provability (zkLighter lineage).

### `poc-oracle` — price defense

Multi-provider **median** aggregation, **staleness** windows, **deviation
quarantine** (an outlier feed is isolated, not averaged in), and a **quorum
halt** — no mark at all beats a wrong mark. Time-weighted average prices for
funding indices and option settlement. The engine's marks never listen to
the book (see [Marks](#marks-never-listen-to-the-book)).

### `poc-economics` — the business model

Every rule encodes an explicit incentive. Inside:

- **Volume-tiered fees with maker rebates** — takers pay 4.5 bps down to
  0.9 bps by 30-day volume; top-tier makers reach a −1 bps rebate. The
  ladder that pays MMs to run two-sided books from day zero.
- **Premium + interest funding** (BitMEX model) with clamps: `rate =
  clamp(interest + clamp((markTWAP − indexTWAP)/index, ±cap))`, exactly
  zero-sum per interval.
- **The 60/30/10 revenue router** — 60% house, 30% insurance fund, 10%
  buyback pool — with exact conservation at every hop.
- **Budgeted liquidity-reward pool** — scored on two-sided, tight quoting
  from randomized samples (you cannot game a sample you can't predict).
- **LP underwriter vaults** (G-16) — epoch-settled subscriptions,
  NAV-exact share accounting, per-shareholder claim proofs.
- **MM tier program** (G-15) — obligations (uptime, worst-side spread,
  smaller-side size) measured from randomized samples, earning bounded
  fee discounts that compose with the volume ladder.

### `poc-margin` — capital efficiency

SFPM portfolio margin in the Derive V3 / CME SPAN tradition:
worst-case loss of the *whole book* under a standardized scenario grid
(spot ± fractions of the scan range × vol shifts) — hedges net out,
including **held collateral as a spot-hedge leg** (G-20), so a short call
covered by held BTC and a short call hedged with a long perp both scan
cheaper than the naked wing. Short options pay the **SOMC** tail floor.
Option analytics: Black-Scholes-Merton (norm-CDF via Abramowitz–Stegun
7.1.26, \|ε\| < 1.5e-7, IV solver), American pricing through the
**Barone-Adesi & Whaley (1987)** quadratic approximation validated against
a **Cox-Ross-Rubinstein binomial referee** (< 0.3% of spot across the
trading region), **Merton (1973)** perpetual closed form as the τ→∞ anchor
for everlasting marks.

### `poc-risk` — the safety net

Pre-trade gates that **simulate the worst-case fill before accepting it**
(position limits, delta/gamma caps, notional bands — a margin-clean book
can still carry vega the venue doesn't want, G-41). Liquidation that is
**partial first**, penalized into the **insurance fund**, crossing only
liquidity better than the penalized mark, with **ADL** as the explicit last
resort.

### `poc-engine` — the sequencer

**Commands in → events out → state applied.** One `apply_event` mutator; the
journal is the state machine. Also owns: OCO brackets (G-08), TWAP parent
orders (G-10), batch/amend, Dutch auctions, auto-listing + everlasting
rebase, multi-collateral, insurance inventory with edge-aware rebalancing
(G-23), collateral interest, the DVOL-shaped vol index (G-03), circuit
breakers with reopening auctions. 28 `Command` types in, 57 `Event` types
out.

### `poc-volsurface` — option marks

**Anchor → blend → govern** (G-04): a configured IV anchors the mark, the
book's own quotes blend in inside a sanity band, per-sweep clamps +
staleness fallback keep it honest. Mark IVs are the risk truth — they are
never a raw function of one trade.

### `poc-rfq` — dealer liquidity

Multi-leg **RFQ packages** with competitive quotes, atomic execution through
the venue's margin/fee path, partial fills, private counterparty lists,
block trades with delayed broadcast, Derive's grouped multi-leg fee
discounts (G-11/13/14/38).

### `poc-settlement` — the on-chain layer

**State-diff settlement** (Lighter/dYdX v4 pattern, G-30): merkleized
account commitments, hash-chained batches, a conservation validator, and
the withdrawal escape hatch — users can always force an exit through the
on-chain merkle root even if the operator disappears. Plus
**proof-of-reserves** (G-35): nonce-bound liability trees over cash +
collateral + vault claims, per-account inclusion proofs, a monotonic
publication ledger, and a pluggable `ReserveAttestor` integration point.

### `poc-persist` — durability

A **framed, CRC'd, chain-hashed command WAL** with segment rotation, atomic
checkpoints, and torn-tail-safe crash recovery (G-24). Determinism means
recovery is replay, not a special code path.

### `poc-api` — the gateway protocol

Snapshot+delta market-data sessions with **sequence gap detection and
resync** (G-27), HMAC-SHA256 request signing
(`key|nonce|method|path|body_hash`) with per-key monotonic nonces (replays
→ 409), token-bucket rate limits, and the time-locked withdrawal pipeline
(G-25/32). The embeddable protocol layer a gateway host drives.

### `poc-fix` — the legacy desk bridge

A complete **FIX 4.4 acceptor**: strict `tag=value` codec with
BodyLength/CheckSum, the session protocol (logon, heartbeats, sequence
gaps → ResendRequest, store-and-forward with PossDup + GapFill), and the
application layer — order routing, ExecutionReports to **both**
counterparties, market data, and the **RFQ dealer rails**: `QuoteRequest`
with multi-leg `NoLegs` structures (spreads, straddles, collars) →
competitive firm quotes with per-leg prices and committed working sizes →
`QuoteStatusReport` acks → partial lifts via `NewOrderSingle(117)`. The
whole-system wiring proof ([`e2e_wiring.rs`](crates/fix/tests/e2e_wiring.rs))
lives here.

### `poc-governance` — parameter safety

**Weighted multisig → timelock → grace** with an instant guardian veto
(Aave/dYdX pattern, G-33); every transition is a journalable event.

### `poc-bench` — evidence

Deterministic micro-benchmarks (ns/op with percentiles: placement,
cancellation, random-cross matching, oracle updates, margin summaries, WAL
appends + recovery, settlement root computation) and whole-architecture
stress scenarios: flash crash, volatility spike, order spam, liquidation
cascade, oracle split, crash-recovery determinism.

### `poc-demo` — proof of life

A scripted session exercising every subsystem — quoting → trading →
funding → crash → liquidation → expiry → MM tiers → vault revenue →
proof-of-reserves — ending with a journal-replay audit.

### `fuzz/` — five cargo-fuzz targets

`fuzz_matching`, `fuzz_fees`, `fuzz_merkle`, `fuzz_oracle`, `fuzz_wal` —
parsers and invariants fed garbage until they behave (see
[docs/FUZZING.md](docs/FUZZING.md)).

---

## The nanosecond CLOB — `feat/0dte-ns-clob-platform`

The [`feat/0dte-ns-clob-platform`](https://github.com/idrees2516/perp-options-clob/tree/feat/0dte-ns-clob-platform)
branch carries **ns-clob v2**: a unified nanosecond 0DTE (same-day-expiry)
options trading platform — one crate, **zero runtime dependencies**,
`#![forbid(unsafe_code)]` — the engine *plus* the production-hardened
network venue, with **exact-integer accounting enforced after every single
command**.

```bash
git checkout feat/0dte-ns-clob-platform
cargo run --release --bin ns-venue -- --demo    # keys + session ladder to stderr, HTTP+WS on :8080
cargo test                                    # 116/116 green
cargo run --release --bin ns-tps              # the throughput harness
```

Docs on the branch: the API contract
([`docs/0DTE_API.md`](https://github.com/idrees2516/perp-options-clob/blob/feat/0dte-ns-clob-platform/docs/0DTE_API.md)),
the threat model
([`docs/0DTE_PRODUCTION.md`](https://github.com/idrees2516/perp-options-clob/blob/feat/0dte-ns-clob-platform/docs/0DTE_PRODUCTION.md)),
and the engine README
([`rust/README.md`](https://github.com/idrees2516/perp-options-clob/blob/feat/0dte-ns-clob-platform/rust/README.md)).
The terminal is a standalone Next.js app:
[idrees2516/ns-venue-terminal](https://github.com/idrees2516/ns-venue-terminal).

### Architecture — why it's nanosecond-class

- **Shard per underlying.** Every option series on one underlying (all
  strikes, all expiries) lives in one `engine::Shard` together with that
  underlying's account sub-ledgers, margin engine and circuit breaker. Every
  pre-trade check is local — **no cross-shard locks anywhere on the hot
  path**, which is what makes linear TPS scaling possible. Global symbol
  ids pack `(shard, local)` into a `u32`.
- **Allocation-free steady state.** Books, slabs, the event buffer and the
  journal batch are pre-reserved and recycled; `execute` runs
  `rate-limit → phase/band checks → position/delta/margin gates → FOK
  pre-scan → match loop (STP-aware) → per-trade settlement → commitment
  bookkeeping → remainder resting` without touching the allocator.
- **One cache line per order.** The book is a slab of 64-byte orders with an
  intrusive free list; levels are intrusive doubly-linked chains with the
  best quote at the head (identical code both sides); quantities aggregate
  per level so a sweep touches one cache line per price level.
  **O(1) cancel** (pointer surgery), O(1) best-quote read, and
  generation-guarded public ids (`(slot, uid)` packed in a `u64` — slot
  recycling can never alias a live order).
- **Per-trade settlement.** Premium, fees, position and margin settle on
  *every fill* — no deferred batch window — while staying nanosecond-class
  because settlement is integer double-entry: `sum(account.cash) + house ==
  deposits` after **every** command.
- **Logical nanoseconds.** Time is a `u64` of nanos; the expiry scheduler is
  a minute-resolution timing wheel; replaying the journal reproduces
  `state_hash` bit-for-bit.

### The numbers (measured, 2-vCPU shared sandbox)

Micro-latency (`cargo bench`, journal-discard mode):

| op | mean | p50 | p99 | p99.9 |
|---|---:|---:|---:|---:|
| taker full pipeline (match + settle + journal + events) | **53 ns** | 53 ns | 64 ns | 72 ns |
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

Scaling is by construction: shards share nothing, so N shards scale
linearly to core count and memory bandwidth — 4 shards on 4 cores is 50M+
class. The `ns-tps` harness extrapolates nothing; it counts every command
(each a real engine command) and audits conservation at the end.

### The 0DTE feature set

Premium-rule margin for longs, stress-scenario margin for shorts with a
pin-risk surcharge; per-option price bands (±20%, ±5% fast-market) with
hysteresis; single-price MOC closing auctions (max volume, tie → reference,
price-time allocation); minute-wheel expiry with auto-exercise (cash-settled
European), settlement fee, close-out and margin release; session ladders
(listing every strike at each 30-minute expiry); circuit breaker → halt →
reopening auction; admission-time buying-power commitments for the full
order quantity, released exactly per filled/cancelled amount.

### The production venue (in the same crate)

- **`gateway.rs` / `http.rs` / `ws.rs`** — hand-rolled HTTP/1.1 (anti-
  smuggling: TE rejected, CL required, pipelining refused, slowloris
  deadlines) and an RFC 6455 WebSocket codec (masked client frames, control
  frame rules, fragment-bomb caps), per-IP and global connection caps,
  bounded outboxes that drop slow consumers instead of blocking the venue.
- **`auth.rs` / `crypto.rs`** — HMAC-SHA256 request signing over
  `key_id|ts|nonce|METHOD|path|body_hash`, a 4096-slot sliding anti-replay
  nonce window per key, ±5 s timestamps, scopes (`read < trade < admin`),
  per-key rate buckets, 8-hex key ids with 256-bit `/dev/urandom` secrets;
  constant-time MAC comparison; the account id is bound to the key at
  minting and **never accepted from the client**.
- **`venue.rs`** — shard workers with event fan-out, REST-await correlation,
  the venue clock, WAL sinks with fsync + torn-tail replay, graceful drain
  (`/admin/shutdown` exits non-zero if conservation breaks).
- **`replica.rs`** — an event-derived read replica serving REST queries,
  with engine-truth balance snapshots.
- **`journal.rs`** — CRC32-framed WAL, flushed per batch, fsynced every 64
  flushes; crash recovery replays the log, tolerates a torn tail, restores
  accounts/positions/resting orders bit-exactly and verifies conservation
  before re-opening.
- **`json.rs`** — strict integer-only JSON (floats and exponents rejected
  — trading is integers), depth ≤ 16, 40 000 fuzz iterations without a
  panic.

Every defense is **enforced by code and verified by a test over real TCP**
(`rust/tests/gateway_e2e.rs`, 22 scenarios; `rust/tests/e2e_day.rs` walks a
full trading day with exact cash assertions).

---

## Why this architecture is zkVM-provable

One of the reasons this architecture looks the way it does — beyond
nanosecond efficiency — is **provable execution on zkVMs**: the venue's
correctness should be verifiable by anyone, without trusting the operator.
This is the zkLighter/Lighter → dYdX v4 → Hyperliquid lineage: an
off-chain deterministic sequencer whose output lands on-chain as
cryptographic commitments, with proofs on demand
([docs/research/ZKLIGHTER.md](docs/research/ZKLIGHTER.md)).

### The design constraints and what they buy in a proof system

| Architecture choice (shipped today) | zkVM consequence |
|---|---|
| **Command journal + single-mutator replay** (`Engine::apply_event`; ns-venue `state_hash`) | The zk guest program is a pure function: `replay(journal) == state_hash`. No I/O, no wall-clock, no network inside the trace — the exact shape zkVMs want. The public input is one hash; the verifier checks one equality instead of re-running the venue. |
| **Exact-integer money** (`u128` minor units, checked `mul_div`, floats never touch a ledger) | Integer-only RISC-V instructions. Floating point is the cycle hog of every zkVM; an engine with no floats in the state path compresses the trace by design. |
| **Pure matching core** (`match_taker` side-effect-free; plan/apply separation) | Fill computation is provable independently from settlement application — smaller proofs, composability, and the live path and the audit path are the same code. |
| **Standard-hash commitments everywhere** — chain-hashed WAL (G-24), merkleized settlement diffs (G-30), PoR liability trees (G-35), domain-separated SHA-256 | No custom arithmetization (no Poseidon, no SNARK-friendly hashes required) — the audit path is built from the hashes hash-based zkVMs prove natively. |
| **State-diff settlement with merkle roots** (G-30) | On-chain verification is a constant-size commitment check regardless of trading volume; the withdrawal escape hatch verifies against the root, never the operator. |
| **Allocation-free, O(1) hot path** (ns-clob: one cache line per order, slab books, no locks across shards) | Proof cost is proportional to executed cycles. Nanosecond efficiency is also **prover economics**: fewer cycles per fill = cheaper proofs per fill. |
| **`#![forbid(unsafe_code)]` + bounded, fuzz-exercised parsers** | No undefined behavior inside the proven trace — the program the prover runs is the program you read. |

### The target proof systems

**[Lattice Jolt](https://a16zcrypto.com/posts/article/lattice-snarks-jolt-post-quantum-faster)**
(a16z crypto). Jolt is a RISC-V zkVM built around sumcheck and a single
lookup table ([github.com/a16z/jolt](https://github.com/a16z/jolt)) — the
simplest and most extensible general-purpose zkVM, fast on CPU. The
September 2026 Lattice Jolt release rebuilt Jolt's proof system on the
lattice-SNARK line (LaBRADOR → Hachi): **post-quantum, transparent, faster
and more compact** than the elliptic-curve original. Because this engine is
dependency-free Rust with no syscalls on the matching path, it compiles as a
Jolt guest with minimal cycle bloat: the journal replay *is* the guest
program.

**[Akita](https://github.com/LayerZero-Labs/akita)**
(LayerZero Labs). Akita is the first production-ready **lattice-based
polynomial commitment scheme** — transparent setup, post-quantum security,
2–3× faster proving and 2–8× smaller proofs (65–80 KB) than hash-based
commitments, and its first deployment is as a commitment backend inside
Jolt. For a venue, small proofs are an on-chain cost decision: a
settlement batch proof that fits in a calldata-friendly size is a
settlement batch that gets verified. Our merkleized state-diff batches
(G-30) are the natural object to commit under a lattice PCS.

**[Magnetar](https://www.microsoft.com/en-us/research/blog/proof-based-general-purpose-computing-with-magnetar)**
(Srinath Setty, Microsoft Research). Magnetar is the hash-based
post-quantum zkVM built **from standard hashes alone** — no elliptic
curves, no lattices, no Poseidon — general-purpose (RV64), transparent,
built for recursion, and measured at 7.2–8.7 MHz proving on a laptop
(10–39× faster than existing zkVMs). It is the exact match for this
engine's audit spine: the WAL chain-hash, the settlement merkle trees and
the PoR proofs are already standard-SHA-256 constructions, so proving
"this journal replays to this state hash" is expressing, in a zkVM, what
the in-repo tests already assert in the clear. The recursion support is
what makes *sharded* proofs (one per underlying, ns-clob's design) composable
into a single on-chain verification.

### What is proven today, and what the zkVM path adds

Shipped now: every settlement batch carries a merkle root and a
conservation validator; proof-of-reserves produces per-account inclusion
proofs verified **in the browser** against a domain-separated SHA-256 tree;
the ns venue computes `state_hash` after every command and verifies
conservation continuously. The zkVM path — the design intent — is to wrap
the same assertions (`replay(journal) == state_hash`, `Σ cash + house ==
deposits`, `merkle_root(batch) == on-chain root`) in a Jolt/Akita or
Magnetar proof so that *anyone* can check them without running the venue.

---

## The economics, briefly

**Fees pay for liquidity, not the other way round.** Takers pay 4.5 bps
down to 0.9 bps by 30-day volume; makers reach a **rebate** at the top
tier. Of every unit of fee income, 60% is house revenue, 30% feeds the
insurance fund (buying deleveraging headroom = lower safe maintenance
margins), and 10% feeds a buyback pool.

**Funding tethers the synthetic to the real.** Perp funding is the BitMEX
premium + interest model: `rate = clamp(interest + clamp((markTWAP −
indexTWAP)/index), ±cap)`, longs pay shorts, computed once per interval and
applied uniformly so the system is exactly zero-sum. For options, the
[Paradigm "Everlasting Options"](https://www.paradigm.xyz/2021/05/everlasting-options)
mechanism — the roll paid as premium TWAP — is implemented alongside the
dated-expiry path as the framework for never-expiring structures.

**Portfolio margin charges risk, not positions.** A short call hedged by a
long perp holds offsetting exposure; the SFPM scenario grid (spot ±
fractions of the scan range × vol shifts) nets them and charges the
residual. Naked short gamma pays the SOMC floor because wings move further
than any scan range.

**Liquidation is a cascade, not an event.** Under-margined accounts are
ranked by deficit; the planner closes the largest risk contributors first,
crossing whatever book liquidity exists at prices *better* than the
penalized mark, with the insurance fund as buyer of last resort at the
penalized price. The penalty funds the backstop. Only when the fund is
exhausted does ADL force-close the most profitable counterparties at the
bankruptcy price — the last resort every venue ships and hopes to never
use.

## Marks never listen to the book

The perp mark is the oracle spot; the option mark is the theoretical value
(Black-Scholes for European, Barone-Adesi-Whaley for American) at the
oracle spot with the governed surface's IV. A book-derived mark would let
one large order move every account's margin — circularity and a
manipulation vector. The order book influences the system only through the
funding premium (BBO-mid TWAP vs index TWAP), where clamps bound its power.

## American options

Every option market carries an explicit exercise style. **American** markets
add the early-exercise right: the holder tenders any portion of a long
position (`Command::Exercise`), the request parks for a TWAP window
(30-minute default), settles at the window's TWAP intrinsic minus a 5 bp
fee, and matching short positions are assigned **pro-rata with exact
largest-remainder distribution** — `Σ assigned == settled`, so the position
ledger stays zero-sum through settlement. American marks price through BAW;
at the venue's default zero-rate convention they equal European marks
exactly (asserted by test). Deep dive:
[`docs/OPTIONS.md`](docs/OPTIONS.md).

## Determinism

The same command stream always produces the same event journal, and
replaying that journal into a fresh engine reproduces the state exactly —
books, positions, balances, statistics. This is the property that makes
off-chain matching auditable, and it is asserted by test:

- `engine::tests::determinism_and_replay` — run twice, replay once, all
  three must agree.
- `engine::tests::conservation_across_trades_and_funding` — cash leaves the
  system only through routed fees; funding is zero-sum.

---

## Proof of work — options venue engineering, end to end

This repo doubles as a **portfolio piece**: the full stack an on-chain
options and structured-products venue needs — pricing models, liquidity
bootstrapping, hedging-aware margin, market risk, and the terminal traders
actually use — implemented, tested, and runnable from one command. The
three hard problems of that domain, and where each lives here:

**1 · Pricing & models** — Black-Scholes-Merton analytics
([`crates/margin/src/blackscholes.rs`](crates/margin/src/blackscholes.rs)),
American early exercise via BAW validated against a CRR binomial referee
([`crates/margin/src/american.rs`](crates/margin/src/american.rs)),
discrete-time TWAP marks for funding and exercise, pre-trade vega/gamma
caps ([`crates/risk/src/greeks_limits.rs`](crates/risk/src/greeks_limits.rs)).

**2 · Liquidity bootstrapping** — the maker-rebate fee ladder
([`crates/economics/src/fees.rs`](crates/economics/src/fees.rs)), the MM
tier program + LP vaults with the 60/30/10 revenue router
([`crates/economics/src/vaults.rs`](crates/economics/src/vaults.rs)),
multi-dealer RFQ ([`crates/rfq/`](crates/rfq)), the channel orderbook
([`crates/orderbook/`](crates/orderbook)).

**3 · Delta-neutral hedging, recognized inside the margin grid** — G-20
spot-hedge-aware collateral: hedges net, only residual risk is capitalized
([`crates/margin/src/portfolio.rs`](crates/margin/src/portfolio.rs)); the
scenario grid reprices every leg under shocked states
([`crates/margin/src/lib.rs`](crates/margin/src/lib.rs)); insurance
inventory rebalancing (G-23) ([`crates/engine/src/sweep.rs`](crates/engine/src/sweep.rs)).

Feature history, PR by PR: v0.6 (American options + channel book) ·
gap-closure waves 3–4 (OCO, TWAP parents, MM tiers, vault revenue) · the
trading terminal · the live venue gateway · RFQ v2 (partials, privacy,
delayed tape) · v0.7 (FIX 4.4 wiring, multi-leg structures, whole-stack
e2e proof) · the nanosecond 0DTE platform (branch).

## Web frontend — the trading terminal

The **production-grade trading terminal** lives in its own repository,
connected to this one as a git submodule mounted at `frontend/`:

> **[idrees2516/perp-options-clob-frontend](https://github.com/idrees2516/perp-options-clob-frontend)**
> Next.js 16 + Turborepo monorepo — the 12-view terminal (`apps/web`), the
> socket.io venue gateway with G-25 signed REST (`apps/gateway`), and the
> TypeScript packages that mirror this engine's domain 1:1: `@perp/types`
> (all 28 `Command`s / all 57 `Event`s, exact `bigint` money),
> `@perp/sim-engine` (the deterministic venue simulator) and
> `@perp/api-client` (the one-contract-two-transports connection layer).

```bash
# clone everything (engine + terminal)
git clone --recurse-submodules https://github.com/idrees2516/perp-options-clob.git

# or, from inside an existing checkout
git submodule update --init --recursive

# run the terminal (simulation mode by default — zero backend)
cd frontend && bun install && bun run dev    # -> http://localhost:3000
```

The terminal runs standalone in simulation mode and can also drive a live
venue through the gateway (embedded in-process, or a dedicated container via
the frontend repo's `docker-compose.yml`). Deep dives:
[`frontend/apps/web/README.md`](frontend/apps/web/README.md) -
[`frontend/apps/gateway/README.md`](frontend/apps/gateway/README.md) -
[Vercel deployment](https://github.com/idrees2516/perp-options-clob-frontend/blob/main/docs/DEPLOY-VERCEL.md).

## Results

- **420 Rust tests** on `main` (unit + integration + property —
  journal-replay determinism included) and **116/116** on the
  nanosecond-CLOB branch; plus **19 TypeScript tests** across the venue
  simulator economics and the gateway contract in the
  [frontend repo](https://github.com/idrees2516/perp-options-clob-frontend).
- **5 cargo-fuzz targets** (`fuzz_matching`, `fuzz_fees`, `fuzz_merkle`,
  `fuzz_oracle`, `fuzz_wal`) and **40 000-iteration** JSON fuzzing in the
  ns venue.
- Benchmarks: [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) · stress
  scenarios: [`docs/STRESS_TESTING.md`](docs/STRESS_TESTING.md) · ns-venue
  numbers in [The nanosecond CLOB](#the-nanosecond-clob--feat0dte-ns-clob-platform).
- The 41-item gap register from the original architecture audit is **fully
  closed** (see [`docs/GAP_ANALYSIS.md`](docs/GAP_ANALYSIS.md), Appendix C).

## Documentation

| Doc | Contents |
|---|---|
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | The *why* behind every major design decision |
| [`docs/CLOB_ENGINE.md`](docs/CLOB_ENGINE.md) | Matching engine internals: channel layout, lazy matching, auctions, provability |
| [`docs/OPTIONS.md`](docs/OPTIONS.md) | The option product family: pricing models, exercise lifecycle, assignment |
| [`docs/ECONOMICS.md`](docs/ECONOMICS.md) | Fees, funding, revenue routing, incentives |
| [`docs/INVARIANTS.md`](docs/INVARIANTS.md) | The invariant list the test suite enforces |
| [`docs/GAP_ANALYSIS.md`](docs/GAP_ANALYSIS.md) | The original audit and how all 41 gaps closed |
| [`docs/FIX_GATEWAY.md`](docs/FIX_GATEWAY.md) | The FIX 4.4 acceptor |
| [`docs/RFQ.md`](docs/RFQ.md) | The dealer RFQ system |
| [`docs/OPERATIONS.md`](docs/OPERATIONS.md) | Running the venue |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) / [`docs/STRESS_TESTING.md`](docs/STRESS_TESTING.md) | Measurement and stress scenarios |
| [`docs/FUZZING.md`](docs/FUZZING.md) / [`docs/EFFICIENCY.md`](docs/EFFICIENCY.md) | Fuzz targets; efficiency notes |
| [`docs/WIRING.md`](docs/WIRING.md) / [`docs/DESIGN_SOURCES.md`](docs/DESIGN_SOURCES.md) | The v0.7 whole-stack wiring; production patterns distilled |
| [`docs/research/`](docs/research/) | ZKLIGHTER.md · DERIVE_V3.md · TRADE_PARADIGM_PARADEX.md — the venue-pattern source studies |
| Branch docs | [`docs/0DTE_API.md`](https://github.com/idrees2516/perp-options-clob/blob/feat/0dte-ns-clob-platform/docs/0DTE_API.md) · [`docs/0DTE_PRODUCTION.md`](https://github.com/idrees2516/perp-options-clob/blob/feat/0dte-ns-clob-platform/docs/0DTE_PRODUCTION.md) · [`rust/README.md`](https://github.com/idrees2516/perp-options-clob/blob/feat/0dte-ns-clob-platform/rust/README.md) |

## October 2026 — The Wiring Release (v0.7)

The components became **one system**:

- **Event ids wired out**: `RfqCreated`/`RfqQuoted` now carry the
  engine-assigned `rfq_id`/`quote_id` (predicted at plan from the book's
  counters) — external consumers map their references deterministically,
  no state scanning.
- **Multi-leg option structures over FIX**: `QuoteRequest` accepts a
  `NoLegs (555)` repeating group (`LegSymbol/LegSide/LegQty`) — a spread,
  a straddle, a collar is ONE request; dealers answer with per-leg
  `LegPrice (686)`s and committed `LegOrderQty (685)` working sizes;
  partial lifts take `qty` lots on **every wing at once**.
- **Quote acks**: every quote lands a `QuoteStatusReport (i)` — accepted
  (firm), rejected (with the gate's reason), canceled.
- **Fills reach both desks**: RFQ executions emit ExecutionReports to the
  taker's *and* the quoting dealer's sessions, per leg.
- **The whole-stack wiring proof**: `crates/fix/tests/e2e_wiring.rs`
  walks instruments → CLOB cross over FIX → spread RFQ → partial execution
  → American exercise → settlement book + proof-of-reserves → conservation,
  **twice, bit-identically**.

## Status

Everything above is implemented and tested. `main` carries the reference
engine (in-memory, audit-first); the
[`feat/0dte-ns-clob-platform`](https://github.com/idrees2516/perp-options-clob/tree/feat/0dte-ns-clob-platform)
branch carries the nanosecond 0DTE platform (production venue, WAL
recovery, hardened HTTP/WS gateway). What remains outside the repository
is deployment plumbing — process supervision, TLS termination, key storage,
and the chain itself — every boundary they would touch (commands, events,
marks, settlements) is already a typed, versionable interface.

## License

MIT.

## CI

Formatting, clippy, a stable/beta test matrix and a demo-run job:
[`.github/workflows/ci.yml`](.github/workflows/ci.yml).
