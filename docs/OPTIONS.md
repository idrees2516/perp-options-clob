# Options on the Perpetuals CLOB — Complete Specification

This is the complete specification of the options product family: the
instrument model, the everlasting settlement mechanism, the governed
volatility surface, American-style exit economics, portfolio margin,
fees, liquidation, and lifecycle. Implementation: `crates/core`
(instruments), `crates/volsurface` (marks), `crates/engine/src/sweep.rs`
(settlement), `crates/margin` (risk).

---

## 1. The instrument model

An option market is `Instrument::Option(OptionMarket)`:

* **kind** — call or put; **strike** in quote-minor per base;
* **variant** — `Dated` (expires, settles at expiry) or
  `Everlasting` (never expires, settles by roll — §2);
* **numeric spec** — tick size, lot size, base decimals, shared with
  the CLOB's integer discipline (prices in ticks, quantities in
  lots, all money u128 minor units);
* **Dated**: `expiry_ts_ms`, and expiry settlement is a 30-minute
  TWAP of the underlying (manipulation-resistant cash-out — a single
  print cannot decide where the strip settles);
* **Everlasting**: `interval_ms` + `maturity_multiple` — the claim's
  *economic* maturity is `interval × multiple` (Paradigm Everlasting
  Options convention), and that same number is what the vol surface
  and the margin scenario grid consume, so an hourly-rolled contract
  is margined like the ~1-day option it economically is.

The instrument exposes `tte_ms` (time-to-expiry for dated; economic
maturity for everlasting — one convention, both variants),
`option_intrinsic_quote_minor` (parity settlement), and the standard
notional/price conversions.

## 2. Everlasting settlement — the roll (G-01)

The everlasting variant never expires and never needs exercise.
Instead, on every `roll_interval_ms`, the position stream settles:

```
per-lot roll payment = TWAP(premium mark) × lot_size / 10^base_decimals
longs pay shorts (shorts receive)
```

* The **premium TWAP** is sampled from the book over the interval
  (the same sampling machinery as perp funding, G-39 impact-notional
  walks), with a fail-safe fallback to the current model mark when
  the book never printed.
* The payment is exact integer per-lot (`mul_div` half-up) — no
  floats touch money.
* Conservation is tested: the long side's aggregate payment equals
  the short side's aggregate receipt, to the minor unit.

**Why this design beats exercise:** an option position that settles
mark-premium continuously *is* continuously exercised and re-opened
at the new strike — the holder is never locked into a stale strike
and never needs to time an exercise decision. The economic
equivalent of American-style continuous exercise falls out of the
roll (§4).

## 3. The governed volatility surface (G-04)

Option marks are Black-Scholes at the oracle spot with an IV that is
**governed, not configured**: `VolSurface` (anchor → blend → govern):

1. **Anchor** — a reference IV per market (the listing seed).
2. **Blend** — every usable two-sided book observation (non-zero
   prices, minimum touch sizes both sides, uncrossed, in a sanity band
   of the anchor) inverts the *book's* mid to an implied vol
   (Newton on the dependency-free BSM in `poc-margin`), and EWMAs
   into the mark.
3. **Govern** — the sweep chases the blended target but is clamped
   to `max_move_bps` per sweep of the current mark, and **staleness
   overrides the chase**: if the book feed goes stale, the mark
   decays back toward the anchor instead of freezing at an
   untrustworthy target.

The surface mutations are journaled (`SurfaceObserved` /
`SurfaceSwept`) — replay reconstructs the exact IV path. Marks feed:
the roll (§2), scenario margin (§5), fee caps (§6), liquidation
triggers, and the analytics API.

## 4. American-style economics — continuous exit

The question *"is this an American or European option?"* dissolves in
the perpetual context, and the answer this protocol implements is
**American semantics throughout**:

* **The book is always live.** There is no exercise window, no
  settlement-only period, no lock-up before expiry. A holder can
  exit at intrinsic + time value any instant the book is open —
  which is strictly *more* optionality than a European claim.
* **The roll keeps the exit at parity.** Because every interval
  settles the mark premium, a holder who *holds* is continuously
  paid the time value the market ascribes — nobody can trap value
  inside a claim the holder cannot reach.
* **RFQ gives size an exit the book cannot quote.** A large long can
  request a bid on the whole package (multi-leg structures priced as
  one atomic block) and receive firm dealer answers — the American
  right to exit at size, without moving the book.
* **Dated expiry settles at parity, not at a lock.** The 30-minute
  TWAP cash-out is the terminal American exercise: intrinsic value,
  averaged against manipulation.

The one deliberate restriction: **no single-holder unilateral
physical exercise against the venue** — because there is no
counterparty inventory to deliver into. Every exit is a *trade*
(book, RFQ, or block) or a *settlement* (roll or expiry TWAP),
each settling through margin. This is the Deribit/Derive model: the
"exercise" is liquidity, and liquidity is always on.

## 5. Portfolio margin for options (`crates/margin`)

One margin engine spans perps and options — the SFPM-style scenario
grid:

* per underlying, a **scenario grid** of spot moves × vol moves;
  option positions are re-priced (BSM at the governed IV) under each
  scenario, perps stress linearly;
* the worst-case loss across the grid + the **SOMC floor** (short
  option minimum charge — a far-OTM short that prices at ≈0 still
  posts the tail it truly is) is the maintenance requirement;
* the **initial requirement** is maintenance × the per-underlying
  multiplier (derivatives portfolio offsets: a hedged book is
  cheap, a naked wing is not);
* risk-free rate and dividends default to zero (the crypto venue
  convention) but the pricer carries them;
* the same grid consumes **non-quote collateral** (G-17): each
  currency enters equity at a haircut weight and its price is
  stressed jointly with the portfolio.

 Greeks exposure is capped pre-trade (G-41): portfolio vega and
 gamma limits — the option-desk blowup mode a margin grid alone
 cannot see (a book that is long vega everywhere is fine in *spot*
 scenarios and dead in *vol* ones).

## 6. Fees — capped, structured, dealer-friendly (F-2)

Option taker fee = `min(tier_bps × notional, 12.5% × premium)`; maker
= `min(tier_bps × notional, 2.5% × premium)`. The premium cap is the
rule that makes the entire surface tradeable: wings and far strikes
premium-priced near zero pay fees near zero, never more than the
option cost. Multi-leg RFQ packages take the Derive grouped discount
ladder (largest fee-class group pays full, cheapest group free, next
two 50%); RFQ makers (dealers) pay **zero** — the Paradigm network
economics. All fees route through the conserved revenue router.

## 7. Lifecycle

1. **Listing** — auto-listing (G-34) maintains a strike grid around
   the mark, delists far-empty strikes, rebases everlasting strikes
   when the underlying drifts out of the band.
2. **Trading** — full order-type surface: limit, market, stop-market,
   stop-limit, trailing stops, icebergs, post-only, reduce-only,
   STP, batches, amend-in-place; plus RFQ and blocks for size.
3. **Marking** — the governed surface (§3), journal-exact.
4. **Settlement** — the roll (everlasting, §2) or the expiry TWAP
   (dated); positions cash out, instruments delist.
5. **Liquidation** — the §5 maintenance check; shortfall accounts
   enter the deficit-ordered cascade (collateral → book → fund →
   iterative ADL).

## 8. Where to read the code

| Concern | Site |
|---|---|
| Instrument + numeric conventions | `crates/core/src/instrument.rs` |
| Roll settlement | `crates/engine/src/sweep.rs` (`Everlasting options: the roll IS the funding`) |
| Vol surface | `crates/volsurface/src/lib.rs` |
| BSM + implied vol | `crates/margin/src/blackscholes.rs` |
| Scenario margin | `crates/margin/src/portfolio.rs` |
| Greeks caps | `crates/risk/src/pretrade.rs` |
| Fee caps | `crates/economics/src/fees.rs` |
| Auto-listing | `crates/engine/src/listing.rs` |
| Options invariants | `docs/INVARIANTS.md` |
