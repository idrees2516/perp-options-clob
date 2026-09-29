# Research Notes: Trade Paradigm & Paradex

*Two distinct systems the brief names together: **Paradigm**
(@tradeparadigm) — the institutional RFQ/block liquidity network for
options — and **Paradex** — the portfolio-margined perpetuals DEX
from a quant-trading team (their everlasting-options framing appears
in the docs under "perpetual options"). This note separates them,
because they contribute different halves of the same product: where
block liquidity *comes from* (Paradigm) and how a venue *margin-risks*
perpetual options (Paradex).*

---

## 1. Paradigm (the RFQ network)

### 1.1 The model

Paradigm is not an exchange — it is a **quote-request marketplace**
between institutional takers and a curated maker panel: multi-leg
packages (spreads, straddles, combos, boxes), anonymous until
execution, with makers competing on price rather than queue position.
Key mechanics:

* **RFQ lifecycle**: taker publishes a package intent → makers respond
  with firm quotes (size + price + TTL) → taker executes one quote
  atomically → the venue (Deribit, CME, etc.) clears it.
* **Multi-leg atomicity**: the package either fully executes or not.
* **Block economics**: large size, deferred prints, maker-friendly
  fees — the taker pays, makers quote tighter because adverse
  selection is bounded.
* **Grouped fee discounts** for structures (boxes and riskless
  packages are near-free to trade).

### 1.2 What we implemented (G-11/13/14/38)

| Paradigm mechanic | Here |
|---|---|
| Multi-leg RFQ packages | `RfqRequest` with per-leg spec, directed or open counterparty sets |
| Firm quotes with TTL | `Quote` — replace-quote is atomic cancel-and-replace; TTL expiry sweeps |
| Atomic execution | `RfqExecute` settles every leg through the venue path in one command: margin-checked, fee'd, journaled |
| Block trades with delayed prints | `BlockTrade` + `BlockLedger` broadcast delay (15 min default) |
| Grouped discounts | Derive-style ladder: cheapest group free, next two 50%; box-spread detection |
| Maker economics | makers pay zero in RFQ/blocks; taker pays the package fee |

## 2. Paradex (portfolio-margined perps/options venue)

### 2.1 The model

Paradex runs an off-chain-matched, on-chain-settled DEX with a
**risk-engine-first design**: SCAN-style portfolio margin, a funding
mechanism for perpetuals, and (in their option framing) **everlasting
options** — no expiry, risk transferred continuously via a funding/
premium stream rather than expiry settlement. Their docs emphasize:

* a single margin engine across products (perps + options);
* funding as the pin between market price and fair value;
* liquidation as a risk-engine process with partial closes, insurance,
  and deleveraging;
* mark discipline: theoretical marks anchored to a vol surface with
  book input, bounded by sanity bands.

### 2.2 What we implemented

| Paradex concept | Here |
|---|---|
| Everlasting options (funding-roll, no expiry) | `OptionVariant::Everlasting` + `EverlastingParams` (G-01): longs pay shorts the mark-premium TWAP per interval; effective maturity = interval × multiple (the concentration horizon that keeps gamma/vega nonzero) |
| Unified margin engine | SFPM scenario grid across perps and options, per-underlying |
| Risk-engine-first liquidation | deficit-ordered queue, partial-liquidation bisection, insurance with coverage-ladder penalties, iterative ADL (G-19) |
| Governed vol surface | anchor-blend-govern surface (G-04): book touches EWMA into the anchor, clamped moves, staleness decay |
| American-style exercise on a perpetual product | **new**: early exercise with TWAP settlement + pro-rata assignment on everlasting American markets — the Everstrike shape (see `docs/OPTIONS.md` §5 for why funding pins the extrinsic and exercise pins the intrinsic floor) |

## 3. The synthesis this codebase makes

Paradigm answers *"how does size trade without moving the
screen?"* — RFQ packages, atomic, maker-panel priced.
Paradex answers *"how does a venue risk a product that never
expires?"* — portfolio margin + funding + a governed surface.
zkLighter (see `ZKLIGHTER.md`) answers *"how does any of this stay
honest at throughput?"* — bounded, provable matching.

This repository is the three answers composed: a channel-layout CLOB
with batch-auction capability, a Paradigm-shaped RFQ/block lane
settled through the same margin engine, and a Paradex-shaped
everlasting funding roll — now with the American early-exercise right
layered on top, which is the product differentiator.
