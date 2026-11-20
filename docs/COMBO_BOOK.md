# The Combo Book — Combined Orders & Implied Matching (G-11 depth)

A **combo order** trades a package of instruments as one atomic unit:
a call spread, a straddle, a collar, a futures-vs-options synthetic.
One signed number — the **net price** — governs the whole package; one
signature — the sorted leg structure — decides which orders can trade
against each other. The combo book is the venue's answer to two
structural liquidity problems:

* **Thin orderbooks.** Long-tail strikes cannot sustain resting
  two-sided depth. Combined orders aggregate the wings into one
  tradeable object, and the **thin-book handoff** routes any IOC/FOK
  shortfall into the RFQ dealer network automatically.
* **Multi-leg fragmentation.** Executing a spread leg-by-leg bears
  legging risk (one wing fills, the other doesn't). The combo book
  executes the package atomically in all three matching regimes.

## The order model

```text
ComboLeg { symbol, side, ratio, tick_size, lot_size, base_decimals }
ComboOrder { id, owner, legs[1..=max_legs], side, limit, units, tif }
```

* Legs are **buyer-convention**: `side` is the side the *package
  buyer* takes on that instrument. A sell order takes the mirrored
  side on every leg.
* Legs are **canonical**: sorted by symbol, no duplicates. The
  structure signature (symbol, side, ratio per leg) is the matching
  key — buy and sell orders of the same structure share a book.
* `units` is the package size; leg `j` trades `ratio_j × units` lots.

## The one signed net algebra

All leg money is expressed in the buyer-cost convention: `flow_j > 0`
means the package buyer pays, `flow_j < 0` means the buyer receives.
The **net** `C = Σ flow_j` (per unit, quote minor) is the package's
price, and the single stored `limit` covers debits, credits, and
shorts:

| side | stored limit | meaning | constraint |
|------|--------------|---------|------------|
| buy  | `L`          | max acceptable net cost | `C ≤ L` |
| sell | `m`          | min acceptable net proceeds | `C ≥ m` |

`L = 8.00` caps a debit; `L = -2.00` demands a credit; `m = -2.50`
caps the credit a seller will pay. Two orders are executable when
`m ≤ L` (the ordinary `ask ≤ bid` relation) and **trade at the resting
order's limit**, exactly like a CLOB prints at the maker's price.

## The three matching regimes

### 1. Combo-vs-combo

An incoming combo first matches opposing resting combos of the same
signature: best price first, then time. The trade prints at the
resting limit; the per-leg prices come from `distribute_net` — the
mark-anchored on-grid split:

* every leg anchors at its mark (`floor(mark / tick)` ticks);
* the **absorber leg** (finest tick granularity, ties by canonical
  order) carries the residual so the split lands on the target net;
* the **effective net is recomputed from the final integer ticks** —
  the number that actually trades — and both limits are re-verified.

Fees follow the negotiated rails (taker pays per-leg fees with the
grouped multi-leg ladder, maker pays zero), routed through the
standard revenue path. Bilateral margin: both owners must afford
their mirrored packages (maintenance rule).

### 2. Implied-in

A newly placed combo takes liquidity directly from the per-leg books
when the package is affordable there. `plan_implied_in` walks the
facing depth **level-synchronously**: at stratum `k` the marginal
per-unit net is `Σ flow_j(k-th level price_j)`; because deeper levels
are never better for the taker, the per-unit net is monotone and the
first violating stratum ends the walk. The walk returns units plus
the worst tick reached per leg.

Execution runs as **synthetic IOC takers** on each leg bounded at the
walk's worst tick — real price-time priority and STP per leg, real
makers earning maker fees — with a **two-pass correction**: pass 1
discovers STP-adjusted availability, pass 2 re-matches every leg at
`units' × ratio` lots so the lot ratios stay exact. The taker's net is
verified from the actual fills; the package margin gate (maintenance
rule) runs on the VWAPs.

### 3. Implied-out

A plain order whose remainder would rest (or cancel) can fill into a
resting combo at the combo's **implied leg price** while the combo
hedges its other legs at the touch. Given the hedge flows `H` (the
combo's buyer-convention flows on its other legs at the BBO), the
combo's own limit pins the aggressed leg:

| combo | leg | constraint | bound |
|-------|-----|------------|-------|
| buy | Bid | `+V + H ≤ L` | `V ≤ L − H` — max-pay, floored |
| buy | Ask | `−V + H ≤ L` | `V ≥ H − L` — min-receive, ceiled |
| sell | Bid | `+V + H ≥ m` | `V ≥ m − H` — min-receive, ceiled |
| sell | Ask | `−V + H ≥ m` | `V ≤ H − m` — max-pay, floored |

Flooring/ceiling is conservative against the *combo's* constraint.
The aggressor fills only at prices that satisfy its own limit —
improvements through the limit, never worse. The combo owner is the
maker on the aggressed leg and the taker on the hedges (touch-bounded
IOC, real makers).

## Gates

* **Package margin gate** (placement): the owner's own limit is their
  worst case — `distribute_net` at the limit gives the boundary leg
  prices, a cloned account takes the full package at those prices,
  and equity must clear initial margin. The IM-delta reservation is
  computed in the same simulation pass and scales to the resting
  remainder.
* **Execution gates** (all regimes): bilateral maintenance checks on
  simulated post-trade accounts.
* **FOK**: upfront executability — compatible opposing combos plus
  the implied-in walk must cover the request, or the placement
  rejects whole.

## The thin-book handoff

Every IOC/FOK remainder — single-leg sweeps and combos alike —
auto-publishes an RFQ for the shortfall, collared by the taker's
limit *where the non-negative bound system can represent it*:

* a **buyer's cap** (positive limit) becomes `max_total_cost` on the
  RFQ;
* a **seller's floor** (and negative buyer limits) are not
  representable in the u128 cost bounds — those RFQs publish
  uncollared, and **the taker's explicit execution remains the
  control** (`RfqExecute` never fires automatically);
* GTC/GTD remainders never publish.

The handoff is what makes the hybrid venue work: the CLOB gives
immediacy where depth exists, the combo book assembles packages, and
the RFQ network manufactures liquidity for everything else.

## The best-execution router (`RfqExecuteBest`)

For any RFQ the taker can ask the venue to route: the router prices
the best live quote that can cover the full remaining size (via
`plan_take`, so only executable quotes count) **and** the composite
book route — per-leg market sweeps of the full remaining, all-or-nothing,
taker fees included, margin-gated, collared by the RFQ's own bounds
(the signed generalization of `cost_within_bounds`) — then executes
the cheaper one **net of fees**, journaling a `BestExecutionReport`
with both prices. The book route settles as ordinary `TradeExecuted`
events against real resting makers followed by `RfqBookSwept`, which
consumes the RFQ's remaining exactly like a quote execution. Partial
book coverage falls back to the (possibly partial) quote; margin
failures fall back to the quote or reject.

## Commands, events, and views

```text
Command::PlaceCombo  { owner, legs, side, limit, units, tif }
Command::CancelCombo { owner, combo_id }
Event::ComboPlaced / ComboTraded / ComboClosed / ComboRejected
Event::BestExecutionReport / RfqBookSwept
Engine::combos_view() / combo_reservations_view() / composite_bbo(legs)
```

Combo ids live in their own space (never order ids); reservations live
in their own map. Codec tags 35–38 (persistence). FIX desks place
combos with `NewOrderMultileg (AB)` — see `docs/FIX_GATEWAY.md`.

## Deliberate v1 boundaries

* **No implied display**: resting combos do not quote synthetic
  leg-book levels to the market data feed; implied liquidity is
  consumed, not displayed.
* **Implied-out hedges at the touch only** — no deeper hedge sweeps;
  capacity is bounded by the touch size on every hedge leg.
* **No time-priority queues for implied-out** (candidates sort by
  implied price, then combo id).
* **No combo GTD/expiry sweep** — resting combos are GTC until
  canceled (CoD and cancel-all cover them).
* **Margin-blocked opponents stop the combo-vs-combo loop** in v1
  (documented; the shortfall still rests or hands off to RFQ).
* **FOK upfront check** counts compatible opposing units and walk
  units without simulating maker margins; a margin-blocked opponent
  can in principle shrink an FOK fill — the shortfall still hands off
  to RFQ rather than resting.

## Provenance

The design follows the complex-order architectures of CBOE/CME listed
spreads (net pricing, implied matching), Deribit's combo books, and
Paradigm's multi-leg blocks; the thin-book handoff mirrors Derive's
RFQ routing. The implied-in walk's monotonicity argument is the
standard "first violating stratum ends the walk" of level-synchronous
complex-order matching.
