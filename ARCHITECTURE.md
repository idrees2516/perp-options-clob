# Architecture

This document is the *why* behind the code: every major design decision,
the alternatives that existed, and the reason the winner was chosen. The
mapping to production systems is explicit — this engine is a distillation
of patterns proven at scale, not invention for its own sake.

## Table of contents

1. [Design sources](#design-sources)
2. [The determinism model](#the-determinism-model)
3. [Numerics](#numerics)
4. [Order lifecycle](#order-lifecycle)
5. [Marks](#marks)
6. [Margin](#margin)
7. [Risk & liquidation](#risk--liquidation)
8. [Funding](#funding)
9. [Fees, revenue, incentives](#fees-revenue-incentives)
10. [Oracle defense](#oracle-defense)
11. [Settlement layer (on-chain)](#settlement-layer-on-chain)
12. [Persistence (WAL + checkpoints)](#persistence-wal--checkpoints)
13. [Gateway (API, sessions, withdrawals)](#gateway-api-sessions-withdrawals)
14. [Governance](#governance)
15. [Known simplifications](#known-simplifications)

## Design sources

| System | What was taken |
|---|---|
| [flood-rs](https://github.com/paradigmxyz/flood-rs) (Paradigm) | BTreeMap price-level layout with FIFO queues; pure-match/apply separation |
| [Everlasting Options](https://www.paradigm.xyz/2021/05/everlasting-options) (Paradigm, White & SBF) | The roll-payment mechanism for never-expiring options; the funding framing of this engine's option markets |
| Derive V3 (formerly Lyra) | Subaccount-level **portfolio margin**; SFPM-style scenario margin anchored to the base futures margin; premium-unpaid option accounting; 30-minute TWAP expiry settlement |
| dYdX v4 | Off-chain **deterministic** matching + oracle-based margining; the plan/apply discipline; tiered fee ladder |
| BitMEX | The premium + interest funding model with clamps; the ADL concept |
| Deribit | Liquidation as a cascade (book → insurance); the liquidation penalty funding the insurance backstop; subaccount margining |
| CME SPAN | The scenario-grid concept; the short-option minimum charge |

## The determinism model

**Decision.** Every state mutation flows through a single function:
`Engine::apply_event(&Event)`. Commands are *planned* against an immutable
snapshot into a `Vec<Event>`, then applied one by one; the same vector is
appended to the journal. `Engine::replay(config, journal)` feeds the
journal back through `apply_event` and reproduces the state bit-for-bit.

**Alternatives.** (a) Mutate during matching and journal a *description*
of what happened — the industry's common shortcut; replay then depends on
a second implementation of every rule, and the two drift. (b)
CRDT/event-sourcing frameworks — heavyweight for a single-writer
sequencer, and determinism needs *discipline*, not machinery.

**Consequence.** The plan stage computes everything: fill fees, margin
reservations, settlement amounts, liquidation absorption. Numbers travel
*inside* events, so the apply stage never re-derives them. The trade-off is
that plan-stage helpers sometimes clone accounts to simulate (`poc-risk`
pre-trade checks, the liquidation planner, the reward settlement) — an
O(positions) cost per decision that is negligible against correctness.

**Verification.** `determinism_and_replay` runs a mixed session (trading,
funding, expiry, halts), runs it again, replays the journal, and asserts
all three engines agree on every account view and every statistic.

## Numerics

**Decision.** All money is `u128` quote-minor units. Every product and
quotient goes through `poc_core::mul_div` with an explicit `Rounding`
mode; checked arithmetic returns `Option` and degraded inputs saturate
rather than panic. `f64` exists only inside Black-Scholes and the SFPM
scenario scanner — analytics that produce *risk figures*, never ledger
entries.

**Rounding policy** (mirrors house practice at major CLOBs):

| Direction | Rule |
|---|---|
| Owed to the house (fees) | round **up** |
| Credited to a user (rebates, rewards) | round **down** |
| Mid-flight risk figures | half-up, and never posted without a final directional decision |

**Prices** are integer ticks on the instrument's grid; **quantities** are
integer lots. `Instrument::notional_quote_minor` is the single place the
tick × lot → money conversion happens.

## Order lifecycle

```
Command::Place
  ├─ instrument/account validation          (typed Rejection)
  ├─ reduce-only cap at current position    (poc_risk::reduce_only_cap)
  ├─ pre-trade risk gate                    (poc_risk::check_order)
  │    ├─ price band around the mark
  │    ├─ post-only crossing check
  │    ├─ position / open-order caps
  │    └─ margin gate: simulate full fill at limit price + worst-case fee
  ├─ plan_match(&order)                     (pure)
  │    ├─ book.match_taker(...)             (pure, no mutation)
  │    ├─ Trade events w/ fees per tier
  │    ├─ STP effects → StpCancels
  │    └─ remainder: rest (GTC/GTD limit) or close (IOC/FOK)
  └─ OrderResting w/ margin reservation (increment computed in plan)
```

- **Fills execute at the maker's price** — takers get price improvement;
  the universal CLOB convention.
- **Four STP modes** (CancelNewest default, CancelOldest, CancelBoth,
  DecrementAndCancel) resolved inside `match_taker`, so replay is exact.
- **Stop orders** park off-book at placement (after passing the same risk
  gate), and arm when the *oracle-anchored* mark crosses the trigger —
  never the book, which would let one print fire every stop.
- **Order margin** is reserved at rest (the incremental initial margin of
  the hypothetical full fill) and scaled down as fills consume the order.
  Withdrawals may only take *free* equity.

## Marks

**Decision.** Perp mark = oracle spot. Option mark = Black-Scholes at the
oracle spot with a configured IV (per-market in `EngineConfig`).

**Alternative rejected.** Book-derived marks (mid of BBO, last trade).
These create circularity — a large resting order or wash prints would move
every account's margin and the liquidation trigger — and a manipulation
vector that exchanges mitigate with exactly the kind of clamping that
defeats the point. dYdX v4 margins on the oracle for the same reason;
Deribit marks options theoretically off its own vol surface.

The book's only influence on pricing is the **funding premium** (BBO-mid
TWAP vs index TWAP), where the premium clamp (±5 bps default) bounds how
hard the book can pull.

## Margin

**Decision.** Cross-margin at the subaccount level, computed by SFPM
(Derive V3 / CME SPAN tradition) in `poc-margin`:

1. The **scanning range** for each underlying defaults to the perp's
   maintenance ratio — the venue's own definition of the liquidating move.
2. A grid of scenarios: spot `{±1.0, ±0.5, ±0.25} × range` crossed with
   vol shifts `{±25% relative, 0}`.
3. *Every* leg (perps and options) is repriced per scenario and the
   **portfolio** PnL is taken — long calls offset short perps; only
   residual risk is capitalized.
4. Maintenance = worst-case portfolio loss + Σ SOMC on net-short option
   legs (wings move further than any scan range).
5. Initial = maintenance × 1.4 (Derive's SFPM uplift).
6. Underlyings are summed without cross-commodity offsets (SPAN treats
   commodities additively).

**Premium-unpaid convention.** Options exchange no cash at trade time;
entries anchor unrealized PnL and cash moves only through funding,
settlement, fees, and liquidation. One identity covers everything:
`equity = cash + Σ (mark − entry) × signed_qty`.

**Why not isolated margin?** It charges a short call and its long perp
hedge separately — paying margin twice for offsetting risk. Every
production derivatives venue with an options complex (Deribit, Derive,
CME) is portfolio-margined for the same reason.

## Risk & liquidation

**Pre-trade** (`poc-risk::pretrade`): every gate runs *before* any
mutation. The margin gate clones the account, applies the full fill at
the order's own limit price plus worst-case taker fee, and asks the
portfolio engine for the summary — "simulate then decide", so live and
replayed decisions cannot diverge. Missing marks fail safe (reject).

**Liquidation** (the cascade):

1. **Detection** on every tick: equity < maintenance → candidate.
   Severity = `maintenance − equity` (negative equity counts fully).
   Most severe first, ties by subaccount id — fully deterministic.
2. **Planning** (`LiquidationPlanner`, pure): rank underlyings by margin
   contribution, close legs largest-notional-first at *penalized* prices
   (`mark × (1 ∓ 1.25%)`), until the account's own remaining maintenance
   (plus a 20% restoration buffer) is covered. A final bisection finds
   the smallest closure of the last leg that still restores — the
   Deribit "partial liquidation first" behaviour that minimizes
   disruption.
3. **Execution**: phase A crosses the book as an IOC taker with the
   penalized price as the limit — the account gets filled at *better*
   prices wherever real liquidity exists. Phase B sends the remainder to
   the insurance fund at the penalized price; the penalty component is
   the fund's compensation for being buyer of last resort.
4. **Bankruptcy**: whatever the executions leave below zero is absorbed
   by the fund, exactly (simulated, not projected).
5. **ADL**: when the fund cannot absorb, the remainder closes against
   the *most profitable* opposite-side counterparties at the bankruptcy
   price — their unrealized profit is the least-cost socialization.
   Residual after ADL leaves the fund negative as explicit venue debt.

Fail-safes: no live mark ⇒ no liquidation (never act on a price nobody
can defend); halt ⇒ no new orders.

## Funding

BitMEX premium + interest (see `poc-economics::funding`):

```text
premium = clamp( (markTWAP − indexTWAP) / indexTWAP,        ±premium_clamp )
rate    = clamp( interest + premium,                        ±rate_cap      )
payment = −signed_lots × per_lot(rate, spot_now)
```

- Positive rate: longs pay shorts (universal convention).
- The payment is computed **once per lot** and applied uniformly to both
  sides, so aggregate funding over a closed position set conserves exactly
  (asserted in tests).
- Rounding happens once (ceil on the per-lot magnitude) — dust cannot
  accumulate into an accounting hole.
- TWAPs everywhere: index TWAP from the oracle's ring buffer, mark TWAP
  from BBO-mid samples ringed by the engine. A single manipulated print
  cannot set the rate.

The **Everlasting Options** roll (Paradigm) is implemented alongside:
longs pay the option's premium TWAP each interval — economically a
continuous re-buy of a fresh option, the mechanism that removes expiry
management entirely.

## Fees, revenue, incentives

- **Ladder** (`fees.rs`): taker 4.5bps → 0.9bps, maker 1bp → **−0.5bps**
  (rebate) by 30-day volume; Hyperliquid/dYdX-shaped. Rebates floor
  (users credited), fees ceil (house owed).
- **Revenue router** (`revenue.rs`): 60% house / 30% insurance / 10%
  buyback, floors with the remainder to the house — Σ allocations ==
  amount always, verified by parameter sweep. The insurance share is
  what buys the venue the right to charge low maintenance margins.
- **Liquidity incentives** (`incentives.rs`): a fixed budget per interval
  (a controllable marketing expense) paid pro-rata by score, where score
  = `size × proximity × two_sided_multiplier` for quotes inside the
  band. Dust carries forward. Paying for *quoting* rather than fills
  kills the wash-trading incentive; two-sidedness is what takers consume.

## Oracle defense

Layered (Chainlink-style), in `poc-oracle`:

1. **Median** across providers — an outlier cannot drag the mark.
2. **Staleness** — silent providers are excluded.
3. **Deviation quarantine** — a provider > 5% off the last accepted mark
   is quarantined until it returns.
4. **Quorum** — below 2 healthy providers the mark goes `None`, the
   engine **halts** the underlying, and liquidations are skipped (never
   act on a price nobody can defend).

TWAP windows are ring-buffered step functions — manipulation requires
sustained capital across the whole window, not one print.

## Settlement layer (on-chain)

**Crate:** `poc-settlement` (G-30). The Lighter.xyz / dYdX v4 pattern:
settle *state diffs*, not trades.

Every settlement window the operator captures the venue state before
and after (`StateCapture`), derives the per-account mutation list
(`diff_to` — O(changed accounts)), and publishes a
`SettlementBatch { prev_root, new_root, ops_root, hash }` where the
header hash chains the batch to its predecessor.

* **Commitments** — one merkle leaf per account (cash + positions +
  collateral), domain-separated SHA-256 hashing (`POC-LEAF`,
  `POC-NODE`, `POC-OP`, `POC-BATCH` tags), duplicate-last rule for odd
  levels. Users verify their leaf with an inclusion proof against the
  committed root.
* **Conservation audit** — `validate_batch` replays mutations against a
  pre-state and rejects `UnexplainedFlow`: tracked value may move only
  by the declared custodial residual (deposits - withdrawals). This is
  the check that caught the buyback fee-routing leak.
* **Escape hatch** — `ExitQueue`: withdrawal intents with merkle
  proofs, a force-settlement window, replay-protected nonces, and a
  `neglected()` report when the operator stalls (proof-of-neglect).

See `docs/EFFICIENCY.md` for the Lighter lineage and the deliberate
divergences (escape-hatch trust model rather than ZK validity proofs
at this stage).

## Persistence (WAL + checkpoints)

**Crate:** `poc-persist` (G-24). Because the engine is deterministic,
durability is a *command log*, not a state snapshot:

```text
frame: magic(2) | chain(8) | len(4) | record | crc32(4)
record: 0x01 Command | 0x02 Instrument (genesis frame)
```

* **Codec** — varint (LEB128), length-prefixed strings/vectors, tagged
  enums; a command costs ~20-60 bytes. Total: every byte string decodes
  to exactly one command or fails.
* **Crash model** — a torn final frame is dropped cleanly (reported);
  mid-segment corruption (CRC, chain, magic) refuses recovery. Tested
  by truncating the log at *every* byte offset.
* **Checkpoints** — atomic manifest (write-temp, fsync, rename) plus
  segment pruning bounds restart replay; recovery measured at
  ~7.7M commands/s.

## Gateway (API, sessions, withdrawals)

**Crate:** `poc-api` (G-25/G-27/G-32) — the protocol layer a
tokio/axum host embeds:

* **JSON codec** (`json.rs`) — deterministic key order, integers never
  round-trip through floats, NaN serializes to `null`.
* **Market data** (`session.rs`) — the snapshot + sequenced-delta
  protocol: gaps desync the session (never silently continue) and
  require an explicit resync.
* **Auth** (`auth.rs`) — API keys with per-key monotonic nonces (replay
  protection), a pluggable `Signer` (HMAC-SHA256 in production), and
  deterministic token-bucket rate limits.
* **Withdrawals** (`withdrawal.rs`) — request validation (balance,
  in-flight caps, daily quota), a settlement time-lock, manual-approval
  tiers for large amounts, cancellation refunds, and a `CustodyAdapter`
  trait for the treasury.

## Governance

**Crate:** `poc-governance` (G-33). The Aave/dYdX parameter-governance
pattern: weighted-multisig approvals queue a proposal, a public
timelock (default 24h) gives users time to react, a grace window
expires unexecuted proposals, and a guardian can veto anything
instantly. Every transition emits a `GovernanceEvent` so the host
journals governance with the same discipline as the engine journal.

## Execution algorithms (second closure wave)

**Crate:**  + 
(G-08/10/16, plus G-05/18/23 sweep stages).

**OCO brackets (G-08).** A pair shares one group id () and
the invariant is enforced by a *cascade scan* run after every planning
pass: a pure function over (pre-apply state, planned events) that finds
terminal members and appends the sibling cancel. Because it runs in
 — before apply — the journal records it as part of the same
command, and replay reproduces it without re-deriving anything.
Bracket legs are same-side by design (a long position's TP sell-limit
and SL sell-stop are both asks); atomicity rides the batch gate.

**TWAP parents (G-10).** The parent is accounting, never book state.
Each tick the slicer emits at most one child per parent — a full order
request journaled inside  — which the engine then runs
through the ordinary place path (risk gates, matching, fees). The
integer split guarantees  exactly. Sequential commit
means two parents slicing on the same tick cannot collide on order
ids.

**Sequential-commit batches (the G-09 fix).**  and
 gate atomically (validity + cumulative order-margin against
the pre-command snapshot), then commit member by member: order ids
advance and later members see earlier fills. The original single-pass
planner gave every sibling the same id and could double-fill one maker
against phantom liquidity — now pinned by test (I-26).

**LP vaults (G-16).** Subscriptions and redemptions queue during an
epoch and settle at its boundary through a single 
event whose per-subscriber flows are the exact cash movements (share
issuance floored, redemption payouts floored against NAV — no unit of
account is ever created). The planner simulates the settlement on a
clone and journals the outcome; apply replays the identical simulation.

**Insurance inventory (G-23).** Phase-B liquidations book the fund's
carried positions at the penalized price; every sweep marks them to the
current marks (PnL lands in the fund balance) and drips inventory back
into the book as a synthetic IOC taker when a resting bid pays the
configured edge over the carrying mark. The fund pays no taker fee on
its own unwinds — a fee nobody paid, routed into pools, would mint
quote.

**Collateral interest (G-18).** At UTC day boundaries the sweep charges
the *utilized* portion of oracle-priced non-quote collateral (min of
haircut value and maintenance usage) in kind, routing the quote value
through the revenue router. Idle collateral pays nothing; fixed-rate
(stablecoin) collateral pays nothing (it moves with no underlying).

**Volatility index (G-05).** A moneyness-weighted variance of the
governed mark IVs inside a band, published in permille through integer
arithmetic only (Newton ) — byte-identical on every platform.
This is the honest DVOL simplification: a full variance-strip
replication needs a liquid strip; a young surface would publish noise.

**Spot-hedge-aware margin (G-20).** Oracle-priced collateral enters the
scenario scan as a linear spot leg (), so BTC held
against a short BTC call nets inside the grid instead of sitting
outside it as an unshocked credit. Equity still credits only the
haircut value; the scanner shocks the full balance — conservative on
both sides.

**FIX (G-26).**  is the deterministic protocol core: the
tag=value wire codec with BodyLength/Checksum validation, the venue's
typed message subset (Logon, NewOrderSingle, Cancel, Status,
MarketDataRequest, ExecutionReport, CancelReject), and the pure
translation onto engine commands. Transport, sequence persistence, and
session recovery are gateway-host concerns, deliberately out of scope.

## Known simplifications

Honest scope boundaries, each isolated behind a typed interface. Items
struck through were simplifications of earlier waves that have since
closed (kept for the record):

- ~~**Single collateral.**~~ Closed by G-17: multi-collateral with
  per-currency haircuts, oracle pricing, conversions, spot-hedge-aware
  scanning (G-20), and utilization interest (G-18).
- ~~**IV is configured, not surfaced.**~~ Closed by G-04: the governed
  anchor-blend surface; G-05 publishes the DVOL-shaped index off it.
- ~~**ADL pricing is single-shot.**~~ Closed by G-19: iterative rounds.
- ~~**Insurance inventory is virtual.**~~ Closed by G-23: carried
  positions, mark-to-market, and book-crossing rebalancing drips.
- ~~**In-memory only.**~~ The persistence seam is no longer an excuse:
  the WAL (G-24), gateway protocol (G-25/27), settlement commitments
  (G-30), and FIX codec (G-26) ship. What remains deliberately out of
  scope for a reference engine: actual network transports (TCP/TLS/
  WebSocket hosts), key custody, and chain integrations — the library
  is the deterministic core those hosts drive.
- **Fee volume decay is fixed-rate.** The 30-day window slides at ~1/90
  per funding interval rather than being recomputed from a time-indexed
  ledger.
- **Vault revenue routing defaults off.** Vault mechanics (G-16) ship;
  the insurance-allocation share that would credit them defaults to
  zero until governance sets it.
- **FIX transport.** The codec and typed subset (G-26) ship; sequence
  persistence and session recovery belong to the gateway host.
