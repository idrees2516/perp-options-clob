# Research Notes: Derive V3 (formerly Lyra)

*Sources: docs.derive.xyz — RFQ, fees, portfolio margin,
liquidations, funding, MMP, cancel-on-disconnect, order types,
sub-accounts; the V3 architecture announcements. Earlier distillations
live in `DESIGN_SOURCES.md` §1; this note focuses on what V3's
architecture contributes to the CLOB/options stack and how it is
reflected here.*

---

## 1. The V3 architecture in one paragraph

Derive V3 is a hybrid-venue design: an **off-chain sequencer/matcher
with on-chain settlement**, sub-accounts sharing unified portfolio
margin, a CLOB for flow and an RFQ network for blocks, with
market-maker protections (MMP, cancel-on-disconnect) treated as
first-class engine features rather than client-side afterthoughts.
The unit of integrity is the *deterministic journal*: match off-chain,
settle state transitions on-chain, prove via commitments.

## 2. What we took, feature by feature

| Derive V3 feature | Here | Notes |
|---|---|---|
| Sub-accounts under one margin umbrella | `MarginAccount` per subaccount, cross margin per underlying in the SFPM scan | single margin engine for perps + options, both styles |
| Portfolio margin (scenario-based) | SFPM grid (`poc-margin::portfolio`): spot×vol shocks, SOMC floor, per-underlying initial multipliers | American legs reprice through BAW (see `docs/OPTIONS.md` §4) |
| RFQ for blocks, CLOB for flow | `poc-rfq`: multi-leg packages, quote replace, group discounts; venue-settled atomically | `docs/DESIGN_SOURCES.md` §2 |
| MMP (market-maker protection) | rolling per-(sub, underlying) amount/delta windows; trip cancels resting orders + freezes | G-37 |
| Cancel-on-disconnect | session-drop command pulls resting orders + quotes | G-37 |
| Fee schedule (tiered, capped) | volume-tier ladder; option fee `min(rate × notional, cap × premium)` | the Deribit/Derive cap rule, F-2 |
| Auction opens | `BeginAuction` + uniform-price uncross (G-12), now prefix-sum | `docs/CLOB_ENGINE.md` §4 |
| Funding model | premium + interest with clamps (BitMEX shape); everlasting roll for options | G-01 |

## 3. What V3 contributes to the *options* design specifically

1. **The exercise-and-margin interaction.** Derive's docs are explicit
   that option exercise, assignment, and liquidation are one risk
   pipeline, not separate subsystems. Our ordering — exercise settles
   *before* the liquidation cascade in the same sweep — is that
   principle made concrete (assigned shorts get cascade-handled in the
   same tick; tested).
2. **TWAP settlement discipline.** Derive settles expiries on a
   30-minute TWAP precisely to kill oracle-print manipulation. We
   reuse it for American *early exercise* — same threat model, same
   defense (`docs/OPTIONS.md` §3.2).
3. **The journal/replay discipline.** Plan → apply → journal with
   bit-exact replay is the V3/dYdX v4 determinism lineage; every
   engine feature added in this codebase (exercise included) has to
   pass the replay test before it merges, which is why it exists for
   exercise too (`exercise_replays_bit_for_bit`).

## 4. Where we diverge

* **Exercise style.** Derive lists European options; our American
  support (BAW marks, early-exercise queue, pro-rata assignment) goes
  beyond their product surface — the design reference there is
  Deribit's listed conventions and the everlasting-American shape.
* **Venue currency.** Derive V3 settles in USDa with a Lyra token
  economic layer; we keep a single quote-currency accounting core
  (collateral rails are separate, G-17).
* **Governance.** Their token-voted parameter changes map here to the
  multisig/timelock governance crate (G-33), intentionally simpler.
