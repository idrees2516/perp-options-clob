# Design Sources — Choices Extracted from Derive V3, Paradex, and the Options-Venume Benchmark Set

> This document records the concrete design choices extracted from live competitor documentation
> (September 2026) and maps each to the gap-register item (`G-nn`) it informs. Where two venues
> disagree, the tie is broken by "most suitable for this system" — the decision and its reason
> are recorded per item. Sources: docs.derive.xyz (RFQ, fees, portfolio margin, liquidations,
> perp funding, MMP, cancel-on-disconnect, order types), docs.paradex.trade (funding
> mechanism, portfolio margin, mark price, dated options margin/fees, liquidations).

## 1. RFQ — Multi-Dealer Competitive Quotes (G-11, G-14, G-38)

**From Derive V3 (`/trading/rfq`):**

- An RFQ is a **package of legs**; each leg = instrument + amount + direction (taker side).
  Multi-leg packages price spreads, straddles, and delta-hedged structures as one atomic
  all-or-nothing block at a single `total_cost`.
- Lifecycle: `send_rfq` (an **unsigned intent** — cannot move funds) → makers submit signed
  quotes (`send_quote`, priced per leg, with `max_fee`) → taker executes one quote
  (`execute_quote`) → engine settles the block **atomically**.
- Request fields: `label`, `counterparties` (empty = open to all makers; list = private
  direction), `min_total_cost` / `max_total_cost` (bounds), `partial_fill_step`.
- RFQ quotable window: `valid_until`, **default 10 minutes**.
- `replace_quote` = atomic cancel-and-resubmit by `quote_id_to_cancel` or `nonce_to_cancel`.
- Channels: `{wallet}.rfqs` (maker discovery), `{subaccount}.quotes` (own quotes),
  `{subaccount}.best.quotes` (taker's best incoming quote).

**Decision (adopted):** legs-based packages, unsigned intent, per-leg priced quotes, atomic
execution, private counterparty lists, TTL window (we fix 10 min default, config),
replace-quote atomicity. Execution goes through the **normal margin + fee + journal path**.

## 2. Fee Architecture (G-02, G-38, F-2)

**From Derive V3 (`/integrators/trading/trading-fees`):**

| Instrument | Taker | Maker |
|---|---|---|
| Option | `base + min(rate × notional, 12.5% × premium)` | `min(rate × notional, 12.5% × premium)` |
| Perp | `base + rate × notional` | `rate × notional` |

- The **12.5%-of-premium cap** is the canonical options fee rule (Deribit's shape).
- RFQ trades: **taker fee rate charged on both counterparties** plus base fee on taker.
- Multi-leg RFQ discounts (the standout design): legs grouped into `long calls`, `long puts`,
  `short calls`, `short puts`, `perps`; full fee on the most expensive group, **100% discount
  on the cheapest group, 50% on the second and third cheapest**. Two-leg spreads therefore
  pay zero on the cheaper leg.
- **Box spreads** (long call + short put at K1, short call + long put at K2, same expiry) are
  recognized as synthetic bonds and charged a yield-spread fee
  `notional × 0.5% × years_to_expiry` to both sides + base fee to taker.
- Liquidation fee: **10% of liquidated portfolio value** (marked to market).

**Decision (adopted):** option fee = `min(bps × notional, cap% × premium)` with
taker cap 12.5% / maker cap 2.5% (we keep the tighter maker cap so maker economics never
exceed premium fractions that would stop wing quoting; Deribit uses a similar tighter maker
cap). RFQ/block fee rails: taker-only fees on the package, capped by premium for option legs,
with Derive's grouped multi-leg discount ladder (100%/50%) and box-spread yield fee.

## 3. Portfolio Margin (G-04, G-41 context)

**From Derive V3 (`/portfolio-margin`):**

- `MM = MtM + Σ_currency [ 0.8 × maxLoss + contingencies ]`,
  `IM = MtM + Σ_currency [ 1.0 × maxLoss + contingencies ]` (loss factors: MM 0.8, IM 1.0).
- `maxLoss` from **four loss families**: Regular (23 forward+vol scenarios), Tail (large
  shocks, dampened), Skew (surface rotation/widening), Forward (basis move).
- Contingencies per currency: risk-cancelling collateral, perp contingency,
  **naked-short-option contingency**, oracle contingency (IM-only).
- Cross-currency universes; losses do not cross universe boundaries.
- Options marked with Black-76 **with discounting**: `C = e^{-rT}(F·N(d1) − K·N(d2))`.

**From Paradex (`/risk/portfolio-margin`):**

- `IMR = max(SCAN Risk, Minimum Delta Requirement) + Funding Provision + Fee Provision`;
  `MMR = 50% × net portfolio IMR + provisions`.
- **SCAN**: 24 scenarios shocking spot **and** IV simultaneously; vol shock is DTE-scaled:
  `shocked_IV = IV × (1 + vol_shock × (30 / max(1, DTE))^0.3)` — short-dated options get
  larger relative vol shocks (the term-structure scaling our SFPM grid adopts).

**Decision (adopted):** keep our SFPM scenario grid, add Paradex's DTE-scaled vol-shock term
structure, Derive's 0.8 MM / 1.0 IM loss factors on the scenario worst-case, and the
naked-short-option + oracle contingencies. Funding/fee provisions cover one settlement
interval of expected payments.

## 4. Option Marks: Live Surface (G-04)

**From Paradex (`/trading/dated-options/mark-price`):**

- Black-76 off a **synthetic forward** `F = S × e^{fT}` (f = externally calibrated forward
  rate), Mark IV fit from a **reference exchange** (BTC/ETH → Deribit; HYPE → Derive), r for
  discounting only.

**Decision (adopted — anchor/blend/govern):** stage 1 anchor = configured IV; stage 2 blend =
invert our own book's weighted bid/ask through the implied-vol solver inside a sanity band,
TWAP-smoothed; stage 3 govern = per-interval move clamps + staleness fallback to anchor.
The blend weight ramps by book liquidity (Paradex's liquidity-weight pattern), so a thin book
reverts to the anchor rather than quoting noise.

## 5. Funding (perp + everlasting roll) (G-01)

**From Derive V3 (`/perp-funding`):**

- Impact prices from walking a fixed impact notional (INA = 4000) into the book.
- `Premium = (max(0, IBP − S) − max(0, S − IAP)) / S`.
- Hourly rate = `Premium / convergence(8) + clamp(baseline − Premium/8, ±0.00625%)`,
  baseline 0.00125%/hr; caps ±0.057%/hr majors.

**From Paradex (`/risk/funding-mechanism`):**

- **Multi-venue impact premium, weighted median** across venues (robust to one thin venue);
  insufficient-depth sides drop to 0.
- Raw rate pulled to baseline by clamped step; **EWMA-smoothed** published rate;
  continuous funding via a **funding index** (cumulative accrual per unit notional) —
  payments settle as `position × Δindex`, exact and gapless.

**Decision (adopted):** keep our TWAP premium + interest baseline (BitMEX shape) but add
(1) impact-notional bid/ask sampling from our own book (Derive INA pattern), (2) EWMA
smoothing of the published rate (Paradex), and for the **everlasting roll** keep the
Everlasting-Options payment = interval premium TWAP per lot, signed longs-pay-shorts —
our existing `EverlastingRoll`.

## 6. Liquidations (G-19, G-21, G-23)

**From Derive V3 (`/liquidations`):**

- **Dutch auctions**: solvent phase price factor 0.98 → 0.80 linearly over 100 s; insolvent
  phase offer descends from MtM toward MM.
- Buffer margin `= MtM + 1.2 × (MM − MtM)`; liquidation ends when buffer = 0.
- Liquidation fee 2% charged on the zero-discount liquidatable fraction (rounded up to 1%).

**From Paradex (`/risk/liquidations`):**

- **Partial liquidation**: all positions cut by the same **Liquidation Share** (multiples of
  20%), sized to bring margin ratio below 90%; penalty = `share × 70% × MMR`.

**Decision (adopted):** keep our book-first IOC cascade (it is faster than an auction in an
off-chain engine), adopt Paradex's **uniform-share partial liquidation** planner refinement,
Derive's **coverage-driven penalty** thinking generalized: penalty percentage steps up when
insurance coverage thins (G-40). Add the **liquidation-velocity breaker** (BitMEX lineage):
when closures per interval exceed a threshold, the sweep slows (longer inter-round delay,
smaller per-round size) — journaled, deterministic.

## 7. Market-Maker Protection & Sessions (MMP, CoD — G-37)

**From Derive V3:**

- **MMP**: per subaccount per currency; only orders/quotes tagged `mmp` count; rolling
  window `mmp_interval`; trip when cumulative |amount| > `mmp_amount_limit` OR |net delta| >
  `mmp_delta_limit`; on trip, cancel all MMP-tagged resting orders and quotes, reject new
  trades for `mmp_frozen_time` (0 = until manual reset).
- **Cancel-on-disconnect**: persisted account setting; on WS drop, cancel resting orders,
  quotes, and trigger orders.

**Decision (adopted):** implement both verbatim (they are the institutional desk's must-haves).
Our engine adds `MmpConfig`, `MmpTripped`, `SessionDropped` events; deterministic replay
required, so the session registry is journaled.

## 8. Order Types (G-06…G-09)

**From Derive V3 (`/trading/order-types`):** limit/market; TIF = gtc / post_only / fok / ioc;
`reject_post_only` flag (reject vs reprice-to-touch); reduce-only (fills capped at position
size, remainder cancelled); `max_fee` per-unit fee cap the signer accepts; `label` for
cancel-by-label; mmp flag.

**From Paradex:** scaled orders, TWAP algo orders, TP/SL, VWAP price protection for market
orders (reject when slippage vs mark exceeds a bound).

**Decision (adopted):** add iceberg (display slice + hidden remainder, Deribit shape),
trailing stops (trigger follows running extreme by offset), batch place/cancel, and
amend-in-place with explicit queue rules (price-widening keeps priority at old price level
placement time? — no: **price-improving amendments lose queue priority; size increases go to
the back of the level**; identical-price size reductions keep priority), plus a
`max_fee` guard on the order path.

## 9. Volume Tiers & Discounts (G-22)

**From Paradex (`/trading/trading-fees`):** 14-day volume tiers; additive discounts
(staking + token-payment + flow-type) with a floor on the total discount (taker never below
1.75 bps); retail/pro profiles.

**Decision (adopted):** replace our decay-approximated 30-day window with an exact
time-bucketed ledger (dYdX v4 pattern); keep our tier ladder; discount floor concept adopted
as "maker rebate can never exceed taker fee on the same trade" (already invariant) plus a
configurable total-discount floor.

## 10. Strike Listing (G-34)

**From Deribit (via benchmark) and Paradex (`/trading/dated-options/expiries-and-listing`):**
list new strikes automatically as spot moves; maintain a fixed grid of deltas/strikes around
the live index per expiry tenor.

**Decision (adopted):** auto-listing ladder: when |index − nearest listed strike| exceeds a
band relative to strike spacing, the sweep lists the missing strikes (journaled
`MarketListed` events), and for **everlasting** options, the daily strike-rebase ladder
(G-03, Everstrike pattern) keeps contracts near the money.
