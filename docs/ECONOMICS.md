# The Economics

The complete incentive design of the venue: who pays whom, why, under
which caps, and what each mechanism buys. Every rule below is
implemented, integer-exact, and invariant-tested (`docs/INVARIANTS.md`);
this document explains the *reasoning* the code encodes.

---

## 1. The money map

```
                        ┌────────────────────────────────────────┐
                        │              FEE INCOME                │
                        │  CLOB taker fees · auction maker fees  │
                        │  RFQ taker fees · block taker fees     │
                        │  collateral interest · quote interest   │
                        └───────────────────┬────────────────────┘
                                            │  RevenueRouter (exact)
              ┌─────────────────────────────┼─────────────────────────────┐
              ▼                             ▼                             ▼
        60% house                       30% insurance                   10% buyback
      (venue P&L)                          │                                │
                                    ┌──────┴───────┐                        │
                                    ▼              ▼                        │
                            LP vault share     fund balance ──(above 2× coverage)──▶ buyback
                            (G-16, bps     (or overflow to                          │
                             per vault)      buyback, G-40)                         │
                                    │                                           │
                                    ▼                                           ▼
                            LP depositors                              platform buyback
                             (NAV yield)                                (fee switch)

  Separately, by design:
    funding (perps + everlasting roll)   zero-sum longs ⇄ shorts, capped
    liquidity rewards (G-39)             budget faucet → makers, pro-rata by quote score
    liquidation penalties                liquidated account → insurance fund (coverage-scaled)
    referral share (G-36)                taker fee fraction → referrer, floored
```

**The invariant under all of it (I-5):** every unit that leaves a
customer account lands in another customer account, a venue pool, or an
LP vault — never nowhere, never twice. The router floors insurance and
buyback shares and gives the house the remainder, so `Σ allocations ==
gross` by construction; the vault split floors each vault's credit and
gives the fund the remainder, so `Σ vault credits + fund credit ==
insurance share` (I-28).

---

## 2. Fees: the two ladders

### 2.1 The volume ladder (pays for flow)

30-day trailing notional unlocks tiers; takers descend from 4 bps to
0.9 bps, makers reach a **−0.5 bp rebate** at the top. The window is
the exact day-bucketed ledger (G-22) — a volume figure ages out when
its day bucket leaves the horizon, with no decay approximation and no
interval-boundary drift. Rebates are funded by taker fees on the same
trades: the ladder is constructed so the deepest rebate is smaller than
the shallowest taker fee it is paired against — the house never pays
for the privilege of being traded through.

### 2.2 The obligations ladder (pays for depth — G-15)

Volume pays for *flow*; it cannot pay for *depth* — a maker who lifts
and re-quotes all day earns VIP status while contributing nothing a
taker consumes. The MM tier program pays for continuous quoting
instead:

| Obligation | Measured as | Why |
|---|---|---|
| Uptime | qualifying ticks / sampled ticks, per tier | presence, not participation — absence counts against the maker |
| Spread | the **worst** side's distance from mid | a two-sided quote is only as tight as its wider leg |
| Size | the **smaller** side's displayed lots | the touch is only as deep as its thinner side |

The tiers tighten and pay more as they demand more (the default ladder:
MM-1 98% uptime at 50 bps / 5 lots for a 20% fee discount, down to MM-3
90% at 250 bps / 1 lot for 6%). Measurement rides the **randomized
liquidity sampler** (G-39) — the same journaled samples that drive
reward payouts — so a maker cannot grind uptime against a predictable
clock, and one measurement feeds two incentive systems without
double-counting. Reviews close the window monthly, re-derive the tier,
and journal the outcome (`MmTierAdjusted` is the only writer of the
discount map); silence demotes automatically.

**Composition rule.** The tier discount applies *after* the volume
tier, on every fee path (CLOB, auction, RFQ, blocks), rounds in the
payer's favour, is capped at 50%, and cannot flip a fee into a rebate.
The two ladders pay for different behaviours, so they compose
multiplicatively rather than being maxed — the same structure Deribit's
maker program uses against its volume tiers.

### 2.3 The option premium cap (F-2)

An uncapped notional rate is economically broken at the wings: a
far-OTM put priced at 0.1% of notional would pay 4.5 bps of notional
in taker fees — 4.5× the entire premium. Both Deribit and Derive cap
the option fee at a fraction of premium:

```
option_fee = min( bps_rate × underlying_notional , cap_pct × premium )
```

Taker cap 12.5%, maker cap 2.5% (the shared industry numbers). Fees
ceil; rebates floor — the house is owed what it is owed, users are
credited what they are credited.

### 2.4 RFQ / block rails (G-38)

Dealer economics: the taker pays the tier's taker rate per leg under
the premium cap, then Derive's grouped multi-leg ladder applies
(cheapest group free, next two at 50%); makers pay zero — the Paradigm
pattern, where the dealer's compensation is the spread, not the fee.
Blocks settle through the venue's account path at the same rails — a
block that skipped margin or fees would not be a feature, it would be
a hole.

---

## 3. Funding: tethering the synthetic to the real

### 3.1 Perps — premium + interest (BitMEX)

```
rate = clamp( interest + (markTWAP − indexTWAP) / indexTWAP , ±cap )
```

Longs pay shorts when the synthetic trades rich, shorts pay longs when
it trades cheap; the interest term funds the cash-leg divergence.
Computed once per interval from TWAPs (not spot — the last print
cannot move the transfer), applied uniformly, exactly zero-sum, and
capped. The mark samples include impact-notional walks (G-39) so a
thin book cannot drag the premium: `min(impact_bid, impact_ask)` is a
far harder manipulation target than a BBO mid.

### 3.2 Everlasting options — the roll as funding (G-01)

The Paradigm [Everlasting Options](https://www.paradigm.xyz/2021/05/everlasting-options)
mechanism: longs pay shorts the option's mark-premium TWAP each roll
interval, so the position behaves like a continuously renewed option
without expiry management. Effective maturity = roll interval ×
configured multiple, shared by marks, greeks, and the margin grid — the
one number that makes the never-expiring claim coherent across the
whole risk stack. The dated-expiry path remains for calendar structures
(30-minute TWAP settlement, Derive's rule).

---

## 4. Margin: charging risk, not positions

**SFPM portfolio margin** (Derive V3 / CME SPAN lineage): the
requirement is the worst-case loss of the *whole book* under a
standardized scenario grid — spot shocks by fractions of the scan
range, vol shifts up and down, time decay to the horizon. Hedges net
out *inside the grid*: a short call hedged by a long perp (or by held
collateral as a spot leg, G-20) pays the residual, not the sum. Naked
short gamma pays the SOMC tail floor, because wings move further than
any scan range.

**The economics of the scan range:** a tighter grid charges less and
liquidates sooner; a wider one charges more and survives more. The
per-underlying overrides exist so the venue can price BTC's realized
kurtosis differently from a stablecoin pair without touching the
engine.

**Liquidation is a cascade, not an event.** Under-margined accounts are
ranked by deficit; the planner closes the largest risk contributors
first, crossing whatever book liquidity exists at prices *better* than
the penalized mark, with the insurance fund as buyer of last resort at
the penalized price. The penalty is coverage-scaled (G-40): a thin
fund charges up to 1.5× before the stress arrives, a comfortable one
the base rate, a fat one overflows new revenue to the buyback pool.
Only when the fund is exhausted does iterative ADL (G-19) force-close
the most profitable counterparties — the last resort every venue ships
and hopes to never use.

---

## 5. The insurance fund as a market (G-16)

A single backstop balance caps the coverage one treasury is willing to
lock. The underwriter vault converts it into a market anyone can
supply:

* **Epoch processing** — subscriptions and redemptions queue and settle
  at boundaries; no mid-epoch dilution.
* **Exact NAV accounting** — share issuance floored, redemption payouts
  floored against NAV; no unit of account is ever created (I-23).
* **Backstop draws** — when the fund is exhausted, a draw debits vault
  collateral and reduces NAV: LPs lose exactly what the cascade would
  otherwise socialize. Draws are the product; ADL frequency is what
  they buy down.
* **Revenue share** — each vault's configured bps of every routed
  insurance allocation, credited pro-rata at apply time (I-28): the
  yield LPs earn for standing behind the book.
* **Per-shareholder holdings** — the claim ledger that backs
  proof-of-reserves entries and validates redemption ownership.

**The yield stack for an LP:** revenue share (continuous, coverage-
regime-independent) + the option value of buying drawdowns at NAV
discount — the same shape as underwriter pools across DeFi, priced by
the same economics.

---

## 6. Liquidity rewards: renting the cold-start depth

A fixed per-interval budget, distributed pro-rata to a **quote score**
(points = size × (1 − spread/max_spread), two-sided multiplier, uptime
floor, cancel-ratio penalty), sampled at randomized instants so the
score is an unbiased estimator of the quoting time-integral. Dust
carries; nothing mints beyond the budget. The pool is a controllable,
depreciating marketing expense whose ROI — quoted depth per reward
unit — is directly measurable, and it retires itself when the tier
program's obligations and the rebate ladder make paid depth
unnecessary.

**Why score, not fills:** paying for fills pays for wash trading
against yourself. Paying for quoted size at randomized instants pays
for exactly the thing takers consume.

---

## 7. The buyback pool (G-40)

The overflow valve of the whole system: insurance revenue above the
coverage target (2× aggregate maintenance) routes to the buyback pool
instead of hoarding. A fund that never stops absorbing becomes a
subsidy to liquidators at the expense of the platform's owners; a
fund that stops at a target stays solvent through the tail and lets
the surplus return value. The threshold is a coverage ratio, not a
balance — it scales with the book's risk as the venue grows.

---

## 8. Interest: charge utilization, never balances (G-18)

Both interest paths charge what the margin system *uses*, not what the
customer holds:

* non-quote collateral: the utilized portion (min of haircut value and
  maintenance usage), charged in kind at the currency's daily rate;
* quote cash (wave 4): `ceil(min(positive cash, maintenance) × bps)`
  per UTC day.

Idle capital pays nothing — the venue is not a bank, and charging
balances would be a tax on custody rather than a price for margin
capacity. The quote rate defaults to zero: enabling it is exactly the
kind of decision the governance timelock exists to make.

---

## 9. The complete incentive table

| Behaviour | Who pays | Who earns | Cap / bound | Invariant |
|---|---|---|---|---|
| Taking liquidity | taker | house / insurance→vaults / buyback | premium cap for options; 12.5% taker | I-5 |
| Providing liquidity | — | maker (rebate at top tier) | rebate < paired taker fee | — |
| Continuous two-sided quoting | — | MM tier discount | 50%; composed after ladder | I-27 |
| Quoting at the touch | reward budget | makers, pro-rata by score | budget + carried dust | — |
| Backstop provision | — | LP vaults (revenue share + draw option) | share ≤ allocation | I-23, I-28 |
| Rich synthetic | longs | shorts | ±cap, TWAP-based | — |
| Everlasting roll | longs | shorts | mark-premium TWAP | — |
| Margin capacity (foreign currency) | holder | revenue router | utilization-bounded | — |
| Margin capacity (quote) | holder | revenue router | utilization-bounded, 1/day | I-31 |
| Under-margination | liquidated account | insurance fund (→ vaults draw) | coverage-scaled ≤ 1.5× | — |
| Fund exhaustion | ADL counterparties | — | iterative, ranked | — |
| Surplus insurance revenue | — | buyback pool | above 2× coverage | — |
| Referral | — | referrer | floored share of taker fee | — |

Every row rounds in a defined direction (house owed ceils, user
credits floor), every transfer is integer-exact, and the property
suite re-derives the whole table's conservation under randomized
command streams (`third_wave_commands_hold_invariants`).
