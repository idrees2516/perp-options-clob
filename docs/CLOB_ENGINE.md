# The CLOB Matching Engine

This is the deep design document for the exchange's core: the central
limit order book, its data structures, its matching algorithms, its
determinism model, and its performance envelope. The engine is
`poc-orderbook` (the book) composed into `poc-engine` (the venue: risk,
margin, fees, settlement around it).

---

## 1. Position in the architecture

```
            commands (Place / Cancel / Amend / Exercise / RFQ / ...)
                              │
                    ┌─────────▼─────────┐
                    │   poc-engine       │   plan (pure) → events
                    │   risk + margin +  │   apply (only mutator)
                    │   fees + sweep    │   journal (replayable)
                    └─────────┬─────────┘
                              │ match_taker() is pure
                    ┌─────────▼─────────┐
                    │  poc-orderbook     │   BTreeMap levels →
                    │  channel queues    │   channels of 8 orders
                    └─────────┬─────────┘
                              │ fills
                    ┌─────────▼─────────┐
                    │ poc-settlement     │   merkle account root +
                    │                    │   book commitment (provable)
                    └────────────────────┘
```

One rule makes everything else possible: **`match_taker` is pure.**
It reads the book, returns a `MatchOutcome` (fills + self-trade
effects), and mutates nothing. The engine applies those outcomes — or
replays the corresponding journal events — through `apply_fill`. Live
trading and journal replay therefore execute *identical* state
transitions by construction. This is the dYdX v4 / Derive V3
determinism discipline, and it is why the venue can prove its state
(`poc-settlement` book commitments) and replay bit-for-bit after a
crash (`poc-persist` WAL recovery).

---

## 2. Data structures — the zkLighter channel layout

### 2.1 The three layers

A resting order lives in exactly three places:

| Layer | Structure | Purpose |
|---|---|---|
| 1. Venue | `BTreeMap<OrderId, RestingOrder>` | O(log n) lookup, deterministic id-order iteration |
| 2. Level | `BTreeMap<u64 /*price key*/, ChannelQueue>` | best-quote maintenance, depth, bands |
| 3. Channel | `[OrderId; 8]` + cached aggregates | bounded matching work, O(1) aggregate queries |

Asks are keyed by raw price ticks (ascending = best first). Bids are
keyed by `u64::MAX − price` so the *first* entry of the same ascending
map is the *best* (highest) bid — no reversed range scans anywhere.

### 2.2 Channels

A **channel** is a fixed-capacity group of up to eight resting order
ids at one price level, with two cached aggregates: its live order
count and the sum of its live orders' visible quantity:

```rust
pub struct Channel {
    slots: [OrderId; 8],   // slot 0 = tombstone sentinel
    live: u8,              // number of non-tombstone slots
    visible_total: u64,   // Σ visible_lots of live slots
}
```

A `ChannelQueue` chains channels in a `VecDeque` and caches the sums
again at the level (`total_visible`, `live_orders`). This is the
zkLighter orderbook layout, and each element of the design pays for
itself:

**Bounded work.** Matching walks channels, not orders-in-the-abstract.
Any single matching step probes at most 8 slots — a hard structural
bound. In a zk rollup this is what keeps the per-block matching
circuit size independent of level depth (the reason zkLighter
introduced channels); in our engine it is simply a guarantee that no
price level, however crowded, can produce an unbounded match step.

**O(1) aggregates.** `best_touch_sizes()` — the vol-surface gate that
runs once per option market per tick — used to scan every resting
order (O(book)). It now reads the level's cached number: O(1). Same
for `depth(n)` (O(n) levels, not orders) and `available_within`
(O(levels within the limit), the FOK feasibility check).

**Tombstone cancels.** Canceling tombstones the order's slot and
decrements the cached aggregates; a channel that empties is dropped
from the chain. A cancel never compacts or reallocates the survivors.

### 2.3 The arrival-order invariant

`Channel::push` appends **strictly after the last live slot** — never
into an earlier tombstone gap:

```
[ A, 0, C, _ ]  --push(E)-->  [ A, 0, C, E ]
        ^                        (not slot 1!)
```

This is what makes iceberg reslicing correct: when an iceberg's
displayed slice is exhausted, `reduce` removes it from its channel and
pushes it again — landing *behind every live order* — implementing the
Deribit rule "revealing new size joins the queue anew." With naive
first-free-slot packing, a resliced order would resurrect inside its
old tombstone and leapfrog competitors: a priority inversion. The test
suite pins this exact scenario
(`channel_tests::iceberg_requeue_moves_to_channel_back`).

### 2.4 What the invariants say

`invariants_hold()` walks every channel of every level and asserts:

1. the book is not crossed (outside auction mode);
2. no empty level buckets exist;
3. every live slot's order exists, sits at the right level and side,
   has positive open quantity, and a visible slice ≤ open quantity;
4. **every channel's cached `visible_total` equals the sum of its live
   orders' visible lots, and every level's cached totals equal the sum
   of its channels'** — the channel contract;
5. every resting order occupies exactly one slot, and every slot
   belongs to an order (no orphans, no duplicates).

The engine's property suite runs this after every random operation
batch; the churn test interleaves random cancels with partial fills
for 40 steps and re-derives the expected FIFO order from a model after
every step.

---

## 3. Matching — the continuous algorithm

### 3.1 The walk

```
match_taker(taker, limit, fok, stp) -> MatchOutcome    // PURE

for level in opposite_side_levels_ascending_in_price:      // BTreeMap order
    if level.price crosses the taker's limit: stop          // band break
    for order_id in level.channels.lazy_live_slots():      // channel chain
        if taker is done: stop                              // ★ laziness
        maker = orders[order_id]
        if maker.owner == taker.owner: apply STP policy     // §3.3
        fill = min(taker_remaining, maker.visible)
        record fill at maker's price
        taker_remaining -= fill
```

The **★ laziness** is the throughput keystone: the walk terminates the
moment the taker is filled. It never materializes a candidate list
(our previous implementation collected every reachable order before
matching — a top-of-book fill paid for the whole book). The measured
consequence: a taker that fills at the touch of a **2,000-order deep
book costs 485 ns p50 — less than the same fill against a 200-order
book under the old eager walk (506 ns)**. Depth behind the touch is
now literally free until you reach it.

### 3.2 Price-time priority

Within a level, slot order *is* arrival order (§2.3), so the channel
chain is the FIFO queue. Across levels, the BTreeMap iterates best
price first. Fills execute at the **maker's price** — takers receive
price improvement when they cross multiple levels, never worse than
their limit.

### 3.3 Self-trade prevention

Four policies, checked against the resting owner before any fill:

| Policy | Effect |
|---|---|
| `CancelNewest` (default) | the taker dies; makers stay resting |
| `CancelOldest` | the resting maker is canceled; the taker walks on |
| `CancelBoth` | both die |
| `DecrementAndCancel` | the overlap is decremented from both, no trade |

Because matching is pure, STP decisions are computed against the
pre-match state and applied afterwards via `apply_stp` — replay
identical.

### 3.4 FOK feasibility from aggregates

Fill-or-kill checks `available_within(limit)` — a sum of *level*
aggregates over the levels the limit can reach — not a per-order scan.
O(levels), not O(orders).

### 3.5 Order types and lifecycle

The book itself is type-agnostic; the engine wraps it:

* **GTC** — match, rest the remainder;
* **IOC** — match, cancel the remainder;
* **FOK** — all-or-nothing via aggregate feasibility (§3.4);
* **GTD** — parked with an expiry; the sweep cancels expired orders;
* **POST-ONLY** — rejected if it would cross (engine-side check via
  `would_cross`);
* **REDUCE-ONLY** — position-capped by the risk engine;
* **Iceberg** — `display_lots` slices; reslice re-queues at the level's
  back (§2.3);
* **Stop / trailing-stop** — parked off-book until the mark crosses the
  trigger, then released as a taker;
* **Amend** — size decrease keeps queue priority in place; price change
  or size increase is cancel-and-replace (new id, back of level).

---

## 4. Auctions — the uniform-price uncross

Opening auctions (G-12) accumulate orders **without matching** — the
book may cross. At the uncross time the sweep calls `uncross()`, the
classic uniform-price call auction (the NYSE / Deutsche Börse opening
shape, the one Derive V3 opens new markets with):

1. **Candidates** — every resting bid and ask level price.
2. **Curves** — prefix sums of quantity in price-time order per side:
   `bid_prefix[i]` = total bid quantity at prices at-or-above level
   `i`; `ask_prefix[j]` = total ask quantity at-or-below level `j`.
3. **Clearing price** — for each candidate `p`, executable volume is
   `min(bid_qty(p), ask_qty(p))` looked up by binary search on the
   sorted price vectors; the winner maximizes volume, ties break
   toward the indicative mid `(best_bid + best_ask) / 2`, then toward
   the lower price.
4. **Allocation** — price-time priority pairing at the uniform price;
   the later-arriving order reports as taker.

The prefix-sum construction evaluates each candidate in O(log L) — the
whole uncross is O(L log L + N) instead of the naive O(L²) candidate
scan. At 10,000 auction orders across 121 levels the measured uncross
(including full event journaling through the engine tick) is **9.1 ms**.

Uniform-price clearing is also the MEV story for the auction segment:
within a batch there is no ordering to front-run, because every fill
prints at the same price (§6, and `docs/research/ZKLIGHTER.md`).

---

## 5. Determinism and provability

### 5.1 Plan / apply / journal

Every state mutation is an `Event` in an append-only journal. The
command path is `plan (pure) → apply (mutator) → journal`. Replay is
`Engine::replay(config, journal)` — used by the WAL recovery path,
tested for bit-exactness in every integration suite.

### 5.2 Book commitments

`poc_settlement::BookCommitment::capture(&engine)` hash-chains
(SHA-256, domain-separated) over every resting order's projection —
symbol, id, owner, side, price, open and visible quantity — in
canonical order. Two engines that replayed the same journal produce the
identical root; any divergence in matching outcome changes it. This is
the zkLighter *provable order book* property, delivered without a
circuit: a watchdog, auditor, or future zk prover can verify the book
it is shown against the published root. Capturing a 1,000-order book
takes **1.2 ms**.

---

## 6. Performance

Release build, this repository's `poc-bench micro` (single thread,
`Instant`-based, fixed iteration counts):

| Operation | p50 | p99 | Note |
|---|---|---|---|
| place/cancel churn (8 accounts) | 542 ns | 15 µs | full engine path: risk, margin reservation, journal |
| crossing taker vs 200 resting | 509 ns | 16 µs | full match + settle |
| **taker vs 2,000-order deep book (lazy)** | **485 ns** | 637 ns | same cost as the shallow book — depth is free |
| auction uncross, 10k orders / 121 levels | — | — | 9.1 ms total (incl. journaling) |
| oracle provider update | 648 ns | 1.5 µs | cluster-consensus recompute |
| tick sweep (idle) | 701 ns | 4.1 µs | full planner pipeline |
| book commitment (1,000 orders) | 1.2 ms | 1.4 ms | capture + hash chain |

The headline: **~2M book-level match evaluations per second per
instrument** on one core (485 ns), with the full engine path
(risk + margin + journal) around 0.5 µs for a taker — i.e. ~1–2M
orders/s throughput headroom on the hot path before considering
parallelism across instruments. The architecture scales *out* by
sharding instruments (each book is independent; margin aggregation
joins per-underlying), which is the same horizontal strategy the
high-throughput rollup DEXs take.

### 6.1 Where the costs actually are

* The engine path (542 ns place/cancel) is dominated by **risk and
  margin reservation**, not the book. The book operations themselves
  are tens of ns.
* The settlement merkle root (1.76 ms over 2,000 accounts) and the
  book commitment (1.2 ms) are **publication-path** costs, amortized
  once per settlement window, not per trade.
* Auction uncrossing is once per auction, O(L log L + N).

---

## 7. Comparisons and design lineage

| System | Layout | What we took |
|---|---|---|
| **zkLighter** | price-level channels of 8, zk-provable matching, batch auctions | the channel layout (§2), bounded per-step work, book commitments (§5.2), uniform-price auctions (§4) |
| **flood-rs (Paradigm)** | BTreeMap levels + VecDeque FIFO | the level-map skeleton (pre-channels), fills at maker price |
| **dYdX v4** | deterministic off-chain matching, on-chain state | pure match / deterministic apply, risk multipliers, replay discipline |
| **Derive V3** | off-chain CLOB, sub-accounts, portfolio margin | the plan/apply journal shape, auction opens, MMP/CoD protections |
| **Hyperliquid** | in-memory book, exchange-grade throughput | the "book ops must be tens of ns" bar; single-writer simplicity |

Deliberate divergences: we keep a `BTreeMap` for the venue order map
(deterministic iteration for sweeps and settlement) where a
production rollup would use a slab/arena; we do not shard the engine
process (yet) — determinism and verifiability outrank raw throughput
for this reference implementation, and the numbers above show the
matching core is not the constraint.

---

## 8. Testing

* **Unit** (`poc-orderbook`): 27 tests — FIFO priority, price
  improvement, FOK, limit respect, all four STP modes, iceberg
  reslicing, auction crossing, channel packing/cancel/aggregates.
* **Differential**: the prefix-sum uncross is tested against a naive
  candidate-scan reference on 25 randomized auction books (same
  clearing price, same volume).
* **Churn model**: the adversarial cancel/fill churn test re-derives
  FIFO order and aggregates after every step from an independent model.
* **Property** (`poc-engine`): random command streams with invariant
  checks per step, replay determinism (live vs shadow engine).
* **Stress** (`poc-bench stress`): flash crash, vol spike, order spam,
  liquidation cascade, oracle split, crash-recovery determinism.

See `INVARIANTS.md` for the full invariant catalog and `FUZZING.md`
for the fuzzing harness that feeds the book.
