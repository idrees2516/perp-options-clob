# Research Notes: zkLighter

*Sources: lighter.xyz documentation and whitepaper ("zkLighter: A
Provable Order Book on a zkRollup", 2024), the Lighter v2 batch
auction design, and public engineering discussion of their
orderbook. Distilled to what we adopted, what we adapted, and what we
deliberately deferred.*

---

## 1. What zkLighter is

Lighter is a zk-rollup DEX whose thesis is that a **central limit
order book — the product institutions actually want — can run on a
rollup if matching is made cheap to prove**. Instead of proving each
trade, the sequencer matches off-chain and proves a bounded state
transition per block. Their orderbook architecture is the interesting
part:

1. **Channels.** Each price level groups resting orders into
   fixed-size "channels" (up to 8 orders each). Matching consumes
   whole channels; the per-block circuit work is proportional to
   channels touched, not raw order count, and the fixed channel size
   keeps the proving cost per level constant.
2. **Level-granular matching.** All orders in a channel share a
   price, so crossing a level is one bounded operation; the engine
   pops channels from the level's head.
3. **Batch auctions (v2).** Orders accumulate into discrete blocks
   cleared at one uniform price — no intra-block ordering to attack,
   matching compressed to a single price-print per block, and fair by
   construction (everyone in the batch crosses at the same price).
4. **Provable state.** State transitions are zk-provable; the book
   layout is the enabling constraint (bounded, deterministic work).

## 2. What we adopted

| zkLighter idea | Where it lives here | Measured effect |
|---|---|---|
| Channel layout (8-slot groups + cached aggregates) | `poc-orderbook`: `Channel`, `ChannelQueue` under every price level | touch-size/depth queries O(orders) → O(1)/O(levels); cancels tombstone 8-slot groups |
| Bounded per-step matching work | the channel walk in `match_taker` | structurally capped probes; deep-book takers no slower than shallow |
| Lazy, early-terminating match walk | `match_taker` pulls through channel chains, stops when filled | 2,000-order deep-book taker **485 ns p50** vs 506 ns for the old eager 200-order walk |
| Uniform-price batch clearing | `uncross` (G-12 auctions) upgraded to prefix-sum curves | 10k orders / 121 levels uncross in 9.1 ms, O(L log L) vs O(L²) |
| Provable book state | `poc-settlement::BookCommitment` — SHA-256 hash chain over the resting orders, canonical order | 1,000-order book committed in 1.2 ms; replayed engines agree bit-for-bit |

## 3. What we adapted

**Tombstones instead of compaction.** zkLighter pops channels from a
level head; we additionally tombstone mid-channel cancels so the FIFO
positions of survivors never move (price-time priority must be stable
under churn for *our* replay/audit guarantees, not just provability).

**Arrival-order packing rule.** Our `Channel::push` appends strictly
after the last live slot — filling an earlier tombstone gap would let
a resliced iceberg leapfrog live orders. This rule is subtle enough
that it is called out in the channel tests by name.

**A BTreeMap venue map, not an arena.** A production rollup uses slab
allocation for provability of the *circuit*; we keep deterministic
`BTreeMap` iteration because our equivalent of "provable" is the
journal replay + book commitment, which needs canonical order more
than it needs compact memory.

## 4. What we deferred (and why)

* **Actual zk circuits.** Our provability story is hash-chained
  commitments over a deterministic engine (replay == live,
  book root published per settlement batch). That is zk-*ready*: the
  circuit would prove the same deterministic transition function, but
  writing it is a project of its own and changes no engine decision.
* **Discrete per-block batching as the only mode.** We run continuous
  matching (institutional UX) with auctions at listing and as a
  circuit-breaker recovery mode. Lighter v2's "everything is a batch"
  is a stronger MEV stance but trades off immediacy; our hybrid keeps
  the option (see `docs/CLOB_ENGINE.md` §4, `docs/MEV.md` if present).
* **Sharded/provable sequencer internals.** Single-writer engine; the
  throughput headroom measured in `BENCHMARKS.md` is per-instrument
  and the instrument-sharding story is architectural, not built.

## 5. The takeaway we kept

The single most transferable idea is not any one structure — it is
that **matching-engine design should be driven by what must be
verifiable, not only by what must be fast**. Channels make the work
bounded; laziness makes it proportional to use; commitments make it
checkable. Speed followed verifiability, not the other way round.
