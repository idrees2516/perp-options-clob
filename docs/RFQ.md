# The RFQ System — Multi-Dealer Competitive Quotes

> *"Paradigm's core product is not a venue at all — it is a negotiation network."*

This document is the complete specification of the request-for-quote
system: the economics that justify it, the state machines that
implement it, the dealer program that keeps it honest, the privacy
model that protects it, and the delayed tape that publishes it. The
implementation lives in `crates/rfq` (the pure state machines) and
`crates/engine/src/institutions.rs` (the venue integration).

---

## 1. Why RFQ exists — the economics

Every options venue of consequence runs a hybrid liquidity model:
a CLOB for continuous price discovery and an RFQ network for size.
The split is not historical accident; it is arbitrage between two
different kinds of immediacy cost.

**A block executed against the public book pays the whole book's
immediacy premium.** A 500-lot clip swept through a 20-lot-deep book
walks the ladder, moving the price against itself at every level, and
signals intent to every observer — HFTs feed on the footprint before
the sweep is half done. The taker pays spread + impact + information
leakage.

**A block executed against dealers pays only the dealers' competition
for the flow.** Multiple dealers answer one request, each pricing the
whole clip firm, each knowing the others are quoting too. The winner
is whoever values the flow most; the taker pays the *second-cheapest*
dealer's economics (the competitive floor), not the book's marginal
liquidity. Deribit's BLOCK, Derive's RFQ rails, and Aevo's RFQ all
exist because this is structurally cheaper for size.

The precondition is competition. A dealer network without multiple
firm answers per request is just a slower CLOB with worse prices —
which is why the **dealer program (G-15)** below is not a compliance
feature but the load-bearing wall of the entire product.

---

## 2. The lifecycle

```
 TAKER                     RFQ BOOK                      DEALERS
   |                          |                             |
   |-- RfqCreate (legs, ----->|  Open, remaining=[...]      |
   |   window, bounds,       |                             |
   |   optional direction)    |---- visible to eligible --->|
   |                          |     makers only             |
   |                          |                             |
   |                          |<---- Quote (firm price ----|
   |                          |      per leg, committed     |
   |                          |      size, own TTL)         |
   |                          |                             |
   |<-- quotes (PRIVATE: taker sees all, dealer sees own) --|
   |                          |                             |
   |-- Execute (full) ------>|  Filled + other quotes      |
   |   or ExecutePartial ---->|  cancelled; PARTIAL keeps  |
   |      (take per leg)      |  quote FIRM for remainder   |
   |                          |                             |
   |<======== RfqSettled: margin-checked venue trades =======>|
   |                          |                             |
   |                   [15-min delayed tape print] <--------|
```

### 2.1 `RfqCreate` — the unsigned intent

A taker publishes a **package of legs** (`RfqLeg`: symbol, side,
quantity, instrument numeric spec) priced as one atomic all-or-nothing
block — a risk reversal, a calendar spread, a single BTC-PERP clip
are all "one RFQ". The request carries:

* an optional **counterparty list** — empty means open to all
  eligible makers, non-empty means private direction;
* optional **total-cost bounds** (`min`/`max_total_cost`) — a
  collar the execution must satisfy, so a taker can never be filled
  into a market that moved through their limit while the quotes were
  outstanding;
* a **quoting window** (TTL) — the RFQ expires when it passes;
* creation is an **unsigned intent**: no funds move, nothing is
  reserved, the intent is only recorded. An RFQ cannot be "gamed"
  by publication because publication costs nothing.

### 2.2 `Quote` — the firm answer

A dealer answers with per-leg prices in ticks, a committed size per
leg, and the quote's own TTL (independent of the RFQ's window —
execution requires both to be live). Two disciplines are absolute:

1. **Quotes are firm while they live.** A quote in `Live` state is
   executable at any instant up to `valid_until`. The dealer cannot
   withdraw it without cost — see the dealer program (§4).
2. **Quotes are private to the requesting account.** `quotes_for_
   viewer(rfq, viewer)`: the RFQ's taker sees every live quote (the
   competitive channel); a maker sees only their own; anyone else
   sees nothing. No dealer can quote off another dealer's answer.

A dealer may quote a **working size** below the request
(`RfqQuoteSized`): they commit only 30 of 100 lots. A full execute
then consumes only the commitment, and the RFQ stays open for its
remainder. Multiple quotes per RFQ per dealer are allowed, and
`replace_quote` swaps one atomically (new validation first; on
failure the old quote is untouched).

### 2.3 `Execute` / `ExecutePartial` — the taker accepts

The taker accepts one quote — in full (`execute`) or in part
(`execute_partial`, the "or a partial" of the Paradigm flow):

* the **take vector** says how many lots per leg to lift now, and is
  validated against both the quote's and the RFQ's remaining size;
* the take's **total cost** (`-total_taker_flow`) must satisfy the
  RFQ's bounds — partials are collared too;
* the cost is computed **at the quoted prices for the taken
  quantity**, exactly what the dealer committed.

**A partial does not release the dealer.** The quote's remaining
size stays `Live` and firm until its own TTL — that is what firmness
*means*. The RFQ's remaining size shrinks by the take; the RFQ
closes (`Filled`, other live quotes cancelled) only when every leg's
remaining reaches zero. Other quotes are never cancelled by a
partial — they keep competing for the remainder.

`execute_best` lifts the current best live quote in one call
("the taker executes against the best one": highest
`total_taker_flow` = lowest cost to the taker, ties by earliest
quote id).

### 2.4 `RfqSettled` — the venue path

*"An RFQ trade that does not consume margin checks is not a feature,
it is a hole."* Execution plans through the same venue path as every
other trade:

1. **Plan** (pure): `RfqBook::plan_take` validates and freezes the
   take; `rfq_margin_ok` simulates **both counterparties'** accounts
   with every leg applied and requires each to stay above
   maintenance; `rfq_taker_fees` prices the fee rails (§5).
2. **Journal**: `Event::RfqSettled` carries one `Trade` per leg with
   the take's quantity and the quote's price. The apply stage
   replays the take through `execute_partial` (deterministically
   re-validating), applies fills, fees, volume, and revenue routing
   exactly as a book trade — and registers the delayed print (§6).
3. **Bands**: an RFQ is a *negotiated* trade, not a book crossing —
   price bands and max-order limits **do not apply by construction**.
   The margin check is the safety, and it is bilateral.

---

## 3. State machines

### 3.1 RFQ states

```
             create
               |
               v
   Open ----->Filled   (every leg's remaining reaches 0)
    |  \
    |   \-->Cancelled  (taker cancel)
    \-------->Expired  (window passes; sweep closes)
```

`Open` carries `remaining_lots: Vec<u64>` — the requested size per
leg, decremented by partial executions. The state is deliberately
not "PartiallyFilled": an open RFQ with a reduced remainder is
simply a smaller open RFQ.

### 3.2 Quote states

```
   send_quote_sized
          |
          v
        Live ------> Executed   (fully consumed: every leg's
         | \                      remaining reaches 0)
         |  \-->Cancelled        (dealer pull / replace / RFQ closed)
         \-------> Expired       (quote TTL passes; sweep)
```

`Live` carries `qty_lots` (the commitment) and `filled_lots` (the
consumed part). `remaining_lots()` is the firm remainder.

### 3.3 Determinism

Every collection is a `BTree*`; every id is allocated sequentially;
every mutation follows the plan/apply discipline — the engine's
journal replay reconstructs the book, the program, and the tape bit
for bit (`rfq_v2_replays_bit_for_bit`).

---

## 4. The dealer program (G-15) — firm-quote enforcement

### 4.1 Why a program

The RFQ network's value = competition = multiple firm answers.
A dealer who answers a request, watches where the taker's interest
moves, and then pulls the quote is not providing liquidity — they are
running adverse-selection reconnaissance on request flow at zero
cost. Left unpunished, every rational dealer free-rides, quotes
evaporate, and the network collapses into the CLOB it was supposed
to complement. Paradigm's answer — and ours — is program-level
enforcement: **pull quotes after seeing the request pattern and you
get degraded or dropped from the program, and program membership is
the admission ticket to the flow.**

### 4.2 The ladder

`DealerProgram` (`crates/rfq/src/dealer.rs`) is a deterministic,
event-driven state machine:

* A **pull** = cancelling a `Live` quote on a still-`Open`,
  in-window RFQ (the "pulled after seeing the request" signal).
  Engine-side, `plan_rfq_cancel` detects exactly this shape and
  journals `DealerStruck`.
* **Strikes** accumulate in a rolling window (default 1h). Strikes
  inside the window ≥ threshold (default 3) trip the ladder.
* **Suspension 1** = `suspension_ms` (default 1h). Each subsequent
  suspension is `multiplier ×` the previous (default ×2). Strikes
  reset at each trip, so the dealer serves their time and starts
  clean — but pulls *during* a suspension still count (pulling
  firm quotes while suspended is worse, not better).
* **Drop** = the third suspension (default) removes the dealer
  permanently: `suspended_until = u64::MAX`, `dropped = true`,
  vanishes from `eligible_dealers()` routing. Re-enrollment after a
  drop is an explicit governance act and resets the record.

### 4.3 The score and the tiers

```
score_permille = 1000 × executions
                 / (executions + pulls + (lapse_weight × lapses))
```

* **Lapses** (quotes that simply expired unexecuted) never suspend —
  expiry is a market condition, not bad faith — but they erode the
  score that gates promotion.
* A dealer with no history scores a perfect 1000 (innocent until
  struck); the floor only bites once there is history.
* Tiers are **earned** by executed volume *and* the score floor
  (default 700‰): `Basic → Silver (5,000 lots) → Gold (50,000 lots)`.
* Tiers are **held** until governance demotes: later lapses can
  drop the *earned* tier below the *held* tier, and only an explicit
  `DealerSetTier` event reconciles them.
* **Routing priority** — the real prize: `eligible_dealers()` sorts
  Gold before Silver before Basic, id-tiebroken; directed flow
  surfaces first to the top of that order. The venue's strongest
  incentive costs nothing to grant.

### 4.4 Journal integration

Every program transition is journaled and replay-exact:

| Event | Apply action |
|---|---|
| `DealerEnrolled` | `program.enroll(dealer, tier, ts)` |
| `DealerStruck` | `program.record_pull(dealer, ts)` — **the ladder mutation site** |
| `DealerSuspended` / `DealerDropped` | observer echoes (plan-time `preview_pull` already determined the outcome; replay reproduces it bit for bit) |
| `DealerTierSet` | `program.set_tier` (governance) |
| `RfqSettled` | `program.record_execution(maker, lots)` — the credit |
| `RfqClosed` (expired, quote) | `program.record_lapse(maker)` |

`preview_pull` (pure) and `record_pull` (mutating) share one
evaluation function, so the plan stage can journal exactly the
echoes the apply stage will reproduce — tested at every rung of the
ladder (`preview_matches_record`).

Enforcement is rollout-gated: `EngineConfig::enforce_dealer_program`
(default `false` = open quoting). The program always scores; the
gate decides whether quoting requires membership. This lets a venue
onboard the scoring while dealers learn the rules before the gate
closes.

### 4.5 MMP and CoD — the dealer's protections

The dealer's side of the bargain: rolling-window fill limits
(`SetMmp`) that trip a freeze + cancel resting orders when a
currency's fill flow exceeds the configured amount/delta budget
(the classic market-maker protection against a toxic feed), and
cancel-on-disconnect (`SetCod` + `SessionDropped`) that pulls the
desk's resting orders and quotes when their connection drops
(Paradigm/Derive semantics: never let a dead session quote).

---

## 5. Fee rails (G-38 + the Paradigm economics)

* **Takers pay, makers (dealers) pay zero** — the network economics:
  the dealer network *is* the product; charging dealers to provide
  it would be charging for the privilege of being the inventory.
* The taker pays the tier's taker rate per leg, **capped** at
  `min(rate × notional, cap × premium)` for option legs (F-2 —
  wings and far strikes stay tradeable), then Derive's **grouped
  multi-leg discount** ladder applies: legs grouped by fee class
  (long call / long put / short call / short put / perp); the
  largest group pays full; among the rest, cheapest group free,
  next two at 50%.
* Every fee routes through the conserved `RevenueRouter`
  (house/insurance/buyback) — the same conservation the CLOB's
  fees obey; there is no fee path that does not land in a pool.

---

## 6. The delayed tape (the Transparency pattern)

*"The RFQ tape is broadcast with a delay... so the public market does
not front-run private size."*

RFQ executions do not print to the public book's tape. They register
in the **negotiated-tape ledger** (`BlockLedger`, shared with block
trades, distinguished by `BlockOrigin::Rfq`) at settle time and print
after the broadcast delay (default 15 minutes, Deribit-style).
Consequences, all deliberate:

* dealers can hedge the RFQ flow *before* the market sees the print;
* the public tape eventually records where size actually traded
  (price discovery + surveillance — the delay defers information,
  it does not destroy it);
* every settled RFQ prints **exactly once** — the sweep is
  idempotent, and a partial's remainder prints when *it* settles
  (each execution is its own print with its own delay).

Blocks (`Command::BlockTrade`) ride the same ledger with
`BlockOrigin::Negotiated` — one delayed-tape subsystem, two on-ramps.

---

## 7. Invariants

The RFQ system's invariants (see `docs/INVARIANTS.md` for the full
register with enforcement + tests):

| # | Invariant |
|---|---|
| I-21 | Quote privacy: only the RFQ's taker sees all quotes; a maker sees only their own; others see none. |
| I-22 | Size conservation: every executed lot is ≤ the quote's committed remainder and ≤ the RFQ's remaining size at execution time; the RFQ's remaining never goes negative; the quote's filled never exceeds its commitment. |
| I-23 | Firmness: a `Live` quote with a positive remaining is executable at its quoted price until its own TTL (given an open, in-window RFQ) — the only paths out are execution, expiry, cancel, or RFQ closure. |
| I-24 | The dealer program is deterministic: replaying the journal reproduces every tier, strike, suspension, and drop bit for bit. |
| I-25 | Every settled RFQ execution registers exactly one delayed-tape print that fires exactly once. |
| I-26 | RFQ trades consume the full venue path: bilateral margin, tiered fees, journal — there is no settle path that skips any of them. |

---

## 8. The FIX bridge (legacy connectivity)

The RFQ network is reachable from any legacy FIX desk unchanged —
`QuoteRequest (R)` in, `Quote (S)` back, and a plain
`NewOrderSingle (D)` carrying `117=QuoteID` hits the quote (full or
partial by the order's quantity). Both desks receive
`ExecutionReport (8)` with `ExecType=F` per leg. The complete
mapping lives in `docs/FIX_GATEWAY.md`; the implementation in
`crates/fix`.

---

## 9. Test register

| Suite | What it proves |
|---|---|
| `poc-rfq` unit (40) | lifecycle, directed flow, bounds, best-quote, partial acceptance/validation/privacy, sized quotes, the dealer ladder end-to-end (suspensions, drop, re-enroll, scores, tiers, routing order, preview==record, determinism) |
| `poc-engine` `rfq_v2` (9) | partial execution through the **venue path** (margin/fees/journal + positions), working-size commitments, the program gate under enforcement, pull→suspend→re-admit, execution credits, the delayed tape (before/at/after boundary), privacy views through the engine, bit-for-bit replay, suspension/drop echo events |
| `poc-fix` app (7) | the RFQ flow over FIX end-to-end: request → firm quote → partial lift → full lift → positions/fees in the engine; the delayed print; determinism |
| `poc-bench` | RFQ round trip p50 3.9 µs (create+quote+execute, margin-checked); FIX gateway on_frame p50 3.9 µs |

---

## 10. Design provenance

| Decision | Source | Why |
|---|---|---|
| Multi-dealer competitive quotes, taker lifts best | Paradigm ONE | The network's entire economic point |
| Partial acceptance keeps the quote firm | Paradigm "accept a partial" | Firmness that survives fills |
| Dealer program: strikes → suspension ladder → drop | Paradigm program enforcement, Deribit MM tiers | The incentive that keeps quotes firm |
| Quote privacy per requesting account | Paradigm/Derive RFQ | Dealers must not quote off rivals' answers |
| Delayed RFQ tape (15 min) | Deribit BLOCK print rule | Hedge before the market front-runs size |
| Makers pay zero | Paradigm dealer economics | The network is the product |
| Grouped multi-leg discounts | Derive V3 fee schedule | Structures trade at rational cost |
| MMP + cancel-on-disconnect | Derive V3 MM protections | The dealer's side of the bargain |
| Unsigned intents, event-wise settlement | dYdX v4 journal discipline | Replay determinism for the whole rail |
