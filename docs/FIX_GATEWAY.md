# The FIX 4.4 Gateway — Legacy-System Connectivity

> *The buy-side OMS cannot be rewritten for a new venue. The venue must
> speak the desk's dialect — byte for byte.*

This document specifies the FIX 4.4 acceptor: the wire codec, the
session protocol, and the application layer that bridges legacy
OMS/EMS desks onto the engine's CLOB and — critically — onto the
RFQ dealer network. The implementation lives in `crates/fix`
(`wire.rs`, `session.rs`, `app.rs`).

---

## 1. Wire codec (`wire.rs`)

The canonical `tag=value<SOH>` framing (SOH = `0x01`):

```
8=FIX.4.4<SOH>9=0123<SOH>...body...<SOH>10=168<SOH>
```

* **`8` BeginString** must be the first field and exactly `FIX.4.4`.
* **`9` BodyLength** must be the second field and must equal the byte
  count from just after the `9` field's `<SOH>` through the `<SOH>`
  before `10` — decoded frames are rejected whole on disagreement.
* **`10` CheckSum** must be the last field: the sum of every byte up
  to and including that `<SOH>`, modulo 256, three ASCII digits.
  `FixMessage::encode` computes both canonically; `decode` verifies
  both strictly (a corrupted body byte fails the checksum, a tampered
  length fails the structure).

`FixMessage` is an ordered `Vec<(u32, String)>` — transmission order
is preserved (repeating groups are opaque to the codec and
interpreted by the application layer), `get`/`get_all` provide
first-value and repeating-group access, `get_u64` the integer view.
Encoding is deterministic: `decode(encode(m)).encode() ==
encode(m)`.

---

## 2. Session protocol (`session.rs`)

The state machine that makes a FIX connection survive the real world.

### 2.1 Lifecycle

```
Connected ──Logon──> Established ──Logout──> Disconnected
```

* **Logon (`A`)**: only Logon is accepted pre-handshake; anything
  else is `NotLoggedIn`. The ack echoes `EncryptMethod(98)` and the
  negotiated `HeartBtInt(108)`; the interval is stored in ms for the
  liveness poller. `ResetSeqNumFlag(141)` and sequence-file resets are
  handled by the explicit `SequenceReset` path below.
* **Heartbeat (`0`) / TestRequest (`1`)**: `poll(now, last_inbound)`
  issues a `TestRequest` carrying `TestReqID(112)` once the
  heartbeat interval passes without inbound traffic; the desk's
  `TestRequest` is answered with a `Heartbeat` echoing the id (the
  FIX liveness loop, verbatim).
* **Logout (`5`)**: acknowledged and the session closes; further
  traffic is `SessionOver`.

### 2.2 Sequence numbers — the part legacy desks actually break

Both directions carry monotonic `MsgSeqNum(34)`. Inbound handling:

| Arrival | Behavior |
|---|---|
| `seq == expected` | Consume; then auto-drain any queued future frames in order |
| `seq > expected` | Queue the frame (`BTreeMap<u32, _>`), emit `ResendRequest(2)` with `BeginSeqNo(7)=expected`, `EndSeqNo(16)=0` (all) |
| `seq < expected`, `PossDupFlag(43)=Y` | Silent duplicate — ignored |
| `seq < expected`, no PossDup | The desk has a sequence-file problem: reject with the venue's current position |

### 2.3 Store-and-forward resend

When the desk asks for a resend, the session replays its **stored
application messages** (everything but admin) with `PossDupFlag=Y`
**at their original sequence numbers**, and collapses the admin holes
between them into `SequenceReset(4)` frames with `GapFillFlag(123)=Y`
and `NewSeqNo(36)` — exactly as FIX 4.4 §4.3 specifies:

```
stored:    2(ER) 3(ER) [4 = heartbeat, not stored] 5(ER)
resend:    2(ER,PossDup) 3(ER,PossDup) 4(SeqReset GapFill->5) 5(ER,PossDup)
```

Resent frames never consume fresh sequence numbers: outbound
numbering continues where it was (`next_outbound_seq` is untouched by
a resend) — a resend storm cannot desynchronize the stream.

### 2.4 Determinism

No ambient clock, no I/O: `handle(msg, now)` and `poll(now, last)`
take time explicitly and return the bytes to write. The store is a
`BTreeMap` — gap-fill boundaries are iteration order, and iteration
order is deterministic.

---

## 3. Application layer (`app.rs`)

### 3.1 Provisioning and sessions

`FixGateway::new(engine)` + `register_comp_id(comp_id, subaccount)`:
each desk's CompID maps to the subaccount that speaks as them.
`connect(comp_id)` accepts a session; every frame flows through
`on_frame(session_id, bytes, now) -> Vec<Vec<u8>>` — the whole
gateway is one pure function per frame.

### 3.2 Order management

| Inbound | Engine | Outbound |
|---|---|---|
| `NewOrderSingle (D)` | `Place` | `ExecutionReport (8)` |
| `NewOrderSingle (D)` + `117=QuoteID` | `RfqExecute`/`RfqExecutePartial` | `ExecutionReport (8)` fills |
| `OrderCancelRequest (F)` | `Cancel` | `ExecutionReport` / `OrderCancelReject (9)` |
| `OrderCancelReplaceRequest (G)` | `Amend` | `ExecutionReport` |
| `OrderStatusRequest (H)` | — | `ExecutionReport` state |
| `MarketDataRequest (V)` | — | `MarketDataSnapshotFullRefresh (W)` |

The **ClOrdID router** maintains `ClOrdID → {engine order id, cumulative
fills, weighted average price, status}` and `engine id → ClOrdID`
for the reverse direction; fill attribution offers both maker and
taker order ids to the router and only the desk's own lands.

**ExecutionReport semantics**: `ExecType(150)` `0`=ack/resting,
`F`=trade (with `LastPx(31)`/`LastQty(32)`), `4`=canceled, `8`=
rejected; `OrdStatus(39)` `0`/`1`/`2`/`4`/`8` = new / partially
filled / filled / canceled / rejected; `LeavesQty(151)` and
`CumQty(14)` from the router's accounting; `AvgPx(6)` is the
volume-weighted average in ticks, truncated toward zero.

**Market data**: full-book snapshots — `NoMDEntries(268)` with
`MDEntryType(269)` `0`=bid / `1`=ask, `MDEntryPx(270)` in ticks,
`MDEntrySize(271)` in lots, up to 10 levels a side.

### 3.3 The RFQ flow over FIX — the Paradigm dealer rails

The flow that lets a dealer's *existing* FIX infrastructure quote
the RFQ network without a single line of new client code:

```
TAKER DESK                       VENUE                    DEALER DESK
    |-- QuoteRequest (R) -------->|                          |
    |   131=Q-1, 55, 54, 38       |  [RfqCreate]             |
    |                             |---- rfq visible -------->|
    |                             |                          |
    |                             |<-- Quote (S) -------------|
    |                             |   131=Q-1, 117=D-1,      |
    |                             |   133=OfferPx, 135=OfferSize
    |                             |  [RfqQuote(Sized)]        |
    |                             |                          |
    |-- NewOrderSingle (D) ------>|                          |
    |   117=D-1, 38=20 (partial)  |  [RfqExecutePartial]     |
    |                             |  margin+fees+journal+delayed print
    |                             |                          |
    |<====== ExecutionReport (8), ExecType=F, both desks =====>|
```

* The **taker's** `131=` reference maps to the engine's rfq id;
* the **dealer's** `117=` quote id maps to the engine quote id
  (keyed by the dealer's subaccount — quote ids are per-desk);
* the taker **hits** with a plain `NewOrderSingle` carrying `117=` —
  the standard FIX "order against quote" — and the gateway executes
  the quote: fully when the order carries no `OrderQty(38)`, or
  partially (the G-11 partial acceptance) when it does;
* both desks receive fill `ExecutionReport`s per leg, and the
  execution settles through the **full venue path** — bilateral
  margin, tiered taker fees, journal, delayed tape — exactly as a
  native RFQ execution (because it is one);
* `QuoteCancel (Z)` maps to `RfqCancel` for the dealer's own quote —
  and triggers the G-15 strike machinery when the quote was firm on
  an open RFQ.

### 3.4 Conventions (the venue dialect)

* **Integer encoding**: `Price(44)`, `StopPx(99)`, `LastPx(31)`,
  `AvgPx(6)`, `MDEntryPx(270)` carry the venue's **tick count**;
  `OrderQty(38)`, `LastQty(32)`, `LeavesQty(151)`, `CumQty(14)`,
  `MDEntrySize(271)` carry **lots**. (Integer FIX encodings are the
  crypto-venue norm — no floating point anywhere near money.)
* **`ExecInst(18)` venue extensions**: `P` = post-only, `R` =
  reduce-only (combinable: `PR`). Values outside the venue set are
  ignored, per FIX practice.
* **`OrdType(40)`**: `1` market, `2` limit, `3` stop (uses
  `StopPx(99)`), `4` stop-limit (`99` + `44`).
* **`TimeInForce(59)`**: `3` IOC, `4` FOK, everything else GTC.
* **`Side(54)`**: `1` buy, `2` sell.
* Quote privacy is preserved over FIX: a dealer's quote status
  surfaces only their own answers — never a rival's.

---

## 4. Performance

Measured on the release build (`poc-bench micro`):

| Operation | p50 | p99 |
|---|---|---|
| FIX encode + decode round trip | 1.0 µs | 2.1 µs |
| Gateway `on_frame` (NewOrderSingle: wire → session → app → engine → ExecutionReport) | 3.9 µs | 8.8 µs |

The full legacy-stack round trip — including sequence bookkeeping,
order routing, engine placement, and the ExecutionReport emission —
costs about the same as the RFQ negotiation cycle itself.

---

## 5. Test register

| Suite | Count | Proves |
|---|---|---|
| `wire` | 7 | round-trip fidelity, checksum rejection of corruption, body-length agreement, structural rejections (too short / unterminated / trailing junk / field without `=` / bad BeginString), msg-type table, repeating groups, raw unicode values |
| `session` | 10 | logon handshake, pre-logon rejection, gap → ResendRequest + queued auto-drain, duplicate handling (PossDup silent, plain dup → Reject), **store-and-forward resend with PossDup at original seqs + GapFill for admin holes + no fresh-number consumption**, TestRequest answered, liveness poll, logout, inbound SequenceReset advance (backwards ignored), determinism |
| `app` (gateway) | 7 | logon, full order lifecycle (place → rest → cross → fill reports → engine positions → router avg-px), cancel + unknown-order CancelReject, market-data snapshot, **RFQ over FIX end-to-end (request → firm quote → partial lift → full lift → engine state + Paradigm fee economics)**, the delayed print, determinism |

---

## 6. Design provenance

| Decision | Source | Why |
|---|---|---|
| FIX 4.4, acceptor-side | Every institutional desk on earth | The legacy lingua franca |
| Store-and-forward resend with GapFill | FIX 4.4 §4.3 | Resend storms must not replay a day of heartbeats |
| `117=QuoteID` on NewOrderSingle to hit a quote | FIX RFQ practice | The standard "order against quote" shape |
| Integer price/qty encoding | Deribit / Derive FIX practice | No floating point near money |
| `ExecInst` P/R extensions | venue practice | Post-only and reduce-only without protocol surgery |
| One pure `on_frame` function | the engine's plan/apply discipline | The gateway is replayable and testable like the engine |
