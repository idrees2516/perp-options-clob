# Wiring — how every component connects

This document is the system's **wiring diagram in prose**: how a request
becomes a trade becomes a settlement, and which crate carries each hop.
It is the companion to [`ARCHITECTURE.md`](../ARCHITECTURE.md) (the
design) and [`docs/CLOB_ENGINE.md`](CLOB_ENGINE.md) /
[`docs/OPTIONS.md`](OPTIONS.md) / [`docs/RFQ.md`](RFQ.md) (the
per-subsystem depth).

The whole stack is proven wired by one test:
[`crates/fix/tests/e2e_wiring.rs`](../crates/fix/tests/e2e_wiring.rs) —
"instruments → FIX gateway → CLOB matching → option-spread RFQ → partial
execution → American exercise → settlement + proof-of-reserves →
determinism", twice, bit-identically.

## The component graph

```text
                    ┌──────────────────────────────────────────────────┐
                    │                    clients                        │
                    │  FIX desk · web terminal · native Rust · gateway  │
                    └───────┬───────────────────────────┬──────────────┘
                            │ FIX 4.4 (SOH frames)       │ websocket / REST
                    ┌───────▼─────────────┐   ┌─────────▼──────────┐
                    │ poc-fix             │   │ apps/gateway       │
                    │ wire · session ·    │   │ (TypeScript,        │
                    │ app translation     │   │  socket.io + REST)  │
                    └───────┬─────────────┘   └─────────┬──────────┘
                            │  Command             Command
                    ┌───────▼─────────────────────────▼──────────┐
                    │ poc-engine: Engine                              │
                    │  plan() — pure validation (risk, margin,     │
                    │           bands, dealer program, greeks)      │
                    │  apply() — deterministic state mutation       │
                    │  journal() — the event log (replayable)       │
                    ├───────────────────────────────────────────────┤
                    │  ┌──────────┐ ┌──────────┐ ┌────────────────┐  │
                    │  │orderbook │ │ rfq book │ │ exercise /     │  │
                    │  │(CLOB     │ │(dealer   │ │ TWAP settle    │  │
                    │  │ match)   │ │ network) │ │ (American)      │  │
                    │  └────┬─────┘ └────┬─────┘ └───────┬────────┘  │
                    └───────┼────────────┼───────────────┼───────────┘
                            │            │               │
                    ┌───────▼────────────▼───────────────▼───────────┐
                    │ core types · margin (SFPM+BAW) · risk ·        │
                    │ economics (fees, funding, revenue, incentives) │
                    │ oracle · volsurface · settlement · persist     │
                    └───────────────────────────────────────────────┘
```

## The two execution paths

### A. Continuous path — the CLOB

1. A desk sends `NewOrderSingle (D)` (or the native equivalent:
   `Command::Place`).
2. `poc-fix` translates FIX fields → `OrderRequest` (side, type, price
   ticks, lots, TIF, post-only/reduce-only via ExecInst).
3. `Engine::process(Command::Place)`:
   - **plan** — the pre-trade gates fire in order: price bands vs the
     oracle mark, post-only/reduce-only semantics, position and greeks
     caps, the hypothetical-fill margin gate.
   - **match** — the limit crosses the opposite side of the
     `poc-orderbook` book; price-time priority, self-trade prevention,
     display/iceberg slicing.
   - **apply** — fees (maker rebate / taker tier) route through the
     revenue splitter (60/30/10 house/insurance/buyback), positions
     update VWAP entries, the journal records the events.
4. Both desks receive `ExecutionReport (8)` frames: `ExecType=0` on
   rest, `ExecType=F` per fill, `ExecType=4` on close.

### B. Negotiated path — the RFQ dealer network (G-11)

For size the CLOB cannot absorb without moving the tape:

1. **Request** — the taker publishes `QuoteRequest (R)`:
   single-symbol (55/54/38) or a **structured package** — a
   `NoLegs (555)` repeating group of `LegSymbol (600)` /
   `LegSide (624)` / `LegQty (687)`. The engine turns it into a
   multi-leg RFQ (a call spread, a straddle, a collar...) via
   `Command::RfqCreate`.
2. **Quote** — dealers answer with `Quote (S)`: per-leg
   `LegPrice (686)`s (aligned with the request's leg order), a
   committed working size per leg (`LegOrderQty 685`, the Paradigm
   partial-size discipline), or the classic two-sided single-leg
   `BidPx/OfferPx + sizes`. The venue acks with
   **`QuoteStatusReport (i)`** — accepted (firm while it lives),
   rejected (with the gate's reason), or canceled.
3. **Execute** — the taker lifts with `NewOrderSingle` carrying
   `QuoteID (117)`: full (no OrderQty) or **partial** (`OrderQty` =
   lots *per leg*; the take lands on every wing at once). Execution
   **bypasses the order book and the price band by construction** —
   the price *is* the negotiated quote — but flows through the normal
   fee, margin (hypothetical-fill gate), and journal paths.
   Both desks get ExecutionReports for every leg of their side.
4. **Discipline** — the quote's remainder stays firm until its TTL
   (a dealer pulling a firm quote is suspended by the G-15 ladder);
   the RFQ stays open for its remainder and other quotes keep
   competing. The print joins the **delayed negotiated tape**.

## The wiring guarantees (and where they're enforced)

| Guarantee | Mechanism | Proof |
|---|---|---|
| External ids are deterministic | `RfqCreated`/`RfqQuoted` events carry the engine-assigned `rfq_id`/`quote_id` (predicted at plan from the book's counters — no state scanning) | `e2e_wiring.rs`, `rfq_v2.rs` |
| RFQ fills reach both desks | taker-side **and** maker-side ExecutionReports per leg (`session_of_sub` reverse routing) | `e2e_wiring.rs` |
| Quote acks are explicit | `QuoteStatusReport (i)` on accept/reject/cancel, `QuoteType=1` (tradable/firm) | `e2e_wiring.rs` |
| Multi-leg atomicity | one take vector per leg; the engine settles all legs or rejects the take | `e2e_wiring.rs` |
| RFQ can't dodge margin | the hypothetical-fill margin gate runs on the package (and every leg) before the settle | `rfq_v2.rs` |
| Premium-unpaid convention | option trades move fee cash only; premium rides in marks/settlement (Deribit-style) | `e2e_wiring.rs` |
| American legs are exercisable | `Command::Exercise` on any long option leg, TWAP-window settle, pro-rata assignment | `american_exercise.rs`, `e2e_wiring.rs` |
| Nothing printed from thin air | cash + house/insurance/buyback pools conserve across every path | `e2e_wiring.rs` |
| The whole stack is deterministic | identical frame replay → identical account fingerprint | `e2e_wiring.rs` |

## The FIX field map (options + RFQ)

| FIX tag | Field | Use |
|---|---|---|
| 555 | NoLegs | count of legs in the structure |
| 600 | LegSymbol | instrument per leg (e.g. `BTC-AMER-80000-C`) |
| 624 | LegSide | taker's side on that leg (1=buy, 2=sell) |
| 687 | LegQty | lots per leg |
| 686 | LegPrice | dealer's price per leg, ticks |
| 685 | LegOrderQty | committed working size per leg |
| 131 | QuoteReqID | the desk's request reference |
| 117 | QuoteID | the desk's quote reference; on NewOrderSingle = "hit this quote" |
| 693 | QuoteRespID | the venue's quote ack reference |
| 694 | QuoteRespType | 1 = accept |
| 537 | QuoteType | 1 = tradable (firm) |
| 297 | QuoteStatus | 0 accepted · 5 rejected · 7 canceled |

## Run the proof

```bash
cargo test -p poc-fix --test e2e_wiring   # the whole-system wiring script
cargo run -p poc-demo                      # the narrated session (incl. the spread RFQ)
```
