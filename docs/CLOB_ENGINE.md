# The CLOB Matching Engine — Complete Architecture

This is the complete specification of the central limit order book
engine: the data structures, the matching algorithm, the
deterministic sequencer that wraps it, and every order-type and
protection layer above it. Implementation: `crates/orderbook` (the
pure book), `crates/engine` (the sequencer + venue).

---

## 1. The book (`crates/orderbook`)

### 1.1 Data layout — the flood-rs lineage

```
asks:        BTreeMap<price_ticks, VecDeque<OrderId>>   // ascending: first = best ask
bids_inverted: BTreeMap<INVERT - price, VecDeque<OrderId>> // first = best bid
orders:      BTreeMap<OrderId, RestingOrder>
```

Two decisions define the hot path:

* **Price levels aggregate into queues.** Every resting order at one
  price shares one `VecDeque` — a price level is O(1) to find and the
  queue *is* the time priority. This is the [flood-rs](https://
  github.com/paradigmxyz/flood-rs) layout (Paradigm's own Rust CLOB),
  chosen over order-sorted structures because matching walks levels,
  not individual orders.
* **Bids are keyed inverted** (`INVERT - price`) so the best bid is
  the *first* entry of the map — no `.rev()` iteration, no
  `range(..).rev()` cost on every match. Lighter's cache-friendly
  philosophy: the hottest iteration (best levels first) is the
  cheapest one.

Consequences: match takes the best ask by `asks.iter().next()` and
the best bid by `bids_inverted.iter().next()`; insertion into a level
is `push_back` (O(1)); priority within a level is FIFO by `OrderId`
which is monotonic in arrival — **price-time priority** falls out of
the data structure, it is not enforced by code.

### 1.2 The matching algorithm

An incoming order walks the opposite side level by level:

1. Take the counterparty's best level; cross while
   `incoming price ≥ best ask` (buy side) or
   `incoming/trigger price ≤ best bid` (sell side).
2. Fill against the level's queue head-to-tail — the FIFO is the
   time priority. Each fill emits a `Trade` with a global `seq`
   (monotonic, journal-ordered).
3. Stop conditions: incoming exhausted (it fully filled),
   book empty, or the crossing price fails — remaining quantity
   rests as a new order at the back of its level's queue
   (limit/GTC), or cancels (IOC remainder), or never existed (FOK
   that could not fill completely — atomic pre-check).

Market orders sweep with no price bound; stop orders live **off the
book** in a parked map until the mark crosses the trigger, then
re-enter as market/limit (so stops never add resting liquidity that
would trip themselves); trailing stops recompute their trigger from
the running extreme on every mark update (journaled as its own event
— replay stays exact).

### 1.3 Auctions (G-12)

The book has an **auction mode**: orders accumulate *without
matching* and may cross; `uncross` clears them at a **uniform
clearing price** with price-time priority pairing — the standard
opening/relisting auction. Used at listing and by auto-listing when
new strikes need a fair first print.

### 1.4 The iceberg discipline (G-06)

An iceberg displays a slice and rests the remainder hidden. The
book stores the full quantity but exposes the display size; matching
consumes the visible slice first, then reveals the next — the
workhorse for dealers working size on the public book without
advertising it.

---

## 2. The deterministic sequencer (`crates/engine`)

### 2.1 Commands → events, plan → apply

The engine is a **pure sequencer in the dYdX v4 discipline**:

```
Command ──plan(pure, &self)──> Vec<Event> ──apply(&mut self)──> state
```

* `plan_*` is pure over the pre-command snapshot: validation,
  margin simulation, fee pricing — no mutation, no clock reads.
* `apply` is dumb: it mutates exactly as planned, mirrors RFQ/
  program state, and journals the event.
* The journal **is** the state: `recover` = replay commands through
  the same sequencer. Replay is bit-for-bit (tested: `properties.rs`
  runs 12 seeds × 140 random commands; `rfq_v2_replays_bit_for_bit`
  covers the dealer machinery).

This is why the engine can be embedded anywhere (the FIX gateway
hosts it directly) — it is a library, not a service.

### 2.2 The command surface (the venue's entire API)

| Group | Commands |
|---|---|
| Account | `Deposit`, `Withdraw`, `Transfer` (free-equity gated) |
| Orders | `Place`, `Cancel`, `CancelAll`, `Amend` (queue-preserving reduce / replace), `PlaceBatch`, `CancelBatch` |
| Market data / marks | `OracleUpdate` (per provider), `Tick` (the deterministic heartbeat: funding, rolls, expiries, rewards, liquidation sweeps, tape prints, breakers) |
| RFQ network | `RfqCreate`, `RfqQuote`, `RfqQuoteSized`, `RfqExecute`, `RfqExecutePartial`, `RfqCancel`, `BlockTrade` |
| Dealer program | `DealerEnroll`, `DealerSetTier` |
| MM protections | `SetMmp`, `SetCod`, `SessionDropped` |
| Auction | `BeginAuction` |
| Collateral (G-17) | `DepositCollateral`, `WithdrawCollateral`, `ConvertCollateral` |

### 2.3 The pre-trade wall (risk, `crates/risk`)

Every order passes, in order: **price bands** (limit deviating
beyond N bps of the oracle mark rejects — stale-quote protection),
**post-only** (would-cross rejects), **reduce-only** (would-open
rejects), **position caps**, **order-count caps**, **hypothetical-fill
initial margin** (the order must be fundable at its *full* size at
its limit price), **greeks caps** (G-41: portfolio vega/gamma — the
option-desk risk a single account cannot warehouse), **MMP freeze**
gates, **dealer program** gates (RFQ), **breaker cooldowns** (G-21:
sustained BBO-vs-oracle dislocation suspends the instrument; cascade
velocity suspends liquidations).

### 2.4 Settlement — every fill through one path

Every fill — book trade, RFQ execution, block, auction uncross —
flows through the account path: `apply_fill` (position + VWAP +
realized PnL in per-lot space), `apply_fee`, volume ledger
(30-day rolling, exact day buckets, G-22), `RevenueRouter`
(house/insurance/buyback with exact conservation — every debited fee
unit lands in a pool; the buyback overflow rule is G-40's coverage
ladder), incentive observation, MMP fill recording.

### 2.5 Liquidation — the deficit-ordered cascade

Maintenance shortfall accounts are closed in deficit order:
collateral converted (G-17) → book swept at market → insurance fund
pays the remaining gap → **iterative ADL** (G-19): profitable counterparties
ranked by profit ratio close the remainder in bounded rounds, each
round capped by `adl_round_bps` of each counterparty's position.
The insurance fund's losses are socialized by the coverage ladder;
ADL is the backstop that makes the fund's insolvency impossible to
reach in practice.

---

## 3. Performance

Release-build microbenchmarks (`poc-bench micro`):

| Operation | p50 | p99 |
|---|---|---|
| place/cancel (book churn) | 538 ns | 14.5 µs |
| crossing taker (vs 200 resting) | 506 ns | 15.1 µs |
| RFQ round trip (create+quote+execute, margin-checked) | 3.9 µs | 10.4 µs |
| FIX gateway on_frame (full stack) | 3.9 µs | 8.8 µs |
| WAL append | 431 ns | 635 ns |
| WAL full recovery | 7.7M cmds/s | — |

The full stress suite (flash crash, vol spike, order spam,
liquidation cascade, oracle split, crash-recovery determinism) lives
in `docs/STRESS_TESTING.md`; the invariant register in
`docs/INVARIANTS.md` (I-1..I-26) maps every guarantee to its
enforcement site and test.

---

## 4. What this engine is not

* It is not I/O: no sockets, no async runtime, no threads — the
  FIX/REST/WS surfaces are hosts around it.
* It is not probabilistic: integer money (u128 minor units) end to
  end; `f64` appears only inside the option-pricing solver and the
  vol-surface inversion, both behind validated integer boundaries.
* It is not trust-required: the settlement crate (G-30) hashes the
  engine's state into merkle roots with inclusion proofs, and every
  command is replayable against them.
