# Options: European & American, Dated & Everlasting

This is the deep design document for the option product family: the
instrument taxonomy, the pricing models, the American early-exercise
machinery, margin treatment, and the everlasting roll. The pricers live
in `poc-margin` (`blackscholes.rs`, `american.rs`); the lifecycle
lives in `poc-engine` (`exercise.rs`, `sweep.rs`).

---

## 1. The instrument taxonomy

Every option market is the cross-product of two orthogonal axes:

| | **Dated** | **Everlasting** |
|---|---|---|
| **European** | settles once, on the 30-min TWAP at expiry (Deribit shape) | funding-roll: longs pay shorts the mark-premium TWAP each interval; no expiry (Paradigm *Everlasting Options*, White & SBF 2021) |
| **American** | early-exercise any time before expiry, TWAP-struck, pro-rata short assignment | the flagship: an everlasting option that *also* carries the early-exercise right (the Everstrike shape) |

In code (`poc-core::instrument`):

```rust
pub struct OptionMarket {
    kind: OptionKind,               // Call | Put
    strike_quote_minor: u128,
    expiry_ts_ms: TimestampMs,     // ignored for Everlasting
    variant: OptionVariant,        // Dated | Everlasting
    exercise_style: ExerciseStyle, // European | American     ← NEW
    american: AmericanParams,      // TWAP window + fee       ← NEW
    everlasting: EverlastingParams,
    ...
}
```

`ExerciseStyle::European` is the struct default, so every existing
market definition, codec frame, and test keeps its exact prior
behavior — flipping a market to American is a two-field change.

### 1.1 Why American, and why it is safe here

American options dominate listed venues (all US equity option
classes; Deribit lists European but OTC crypto flow is predominantly
American). The early-exercise right matters for:

* **deep-ITM puts** (r > 0): exercising banks the strike's time value;
* **carry-bearing underlyings** (staking yield, borrow costs): calls on
  high-carry assets exercise early;
* **perpetual products**: exercise is the mechanism that pins a deep-ITM
  everlasting contract to parity — the long can realize intrinsic at
  any time, so the mark can never durably trade below it.

The classic objection — *early exercise is hard to margin and hard to
price* — is handled head-on below (§4 pricing, §5 margin). And there
is one property that makes the migration unusually safe in this
codebase: under the venue's default zero-rate carry convention, the
American price **equals the European price exactly** (Merton: with
`b = r` and `r = 0`, early exercise is never optimal). The engine test
`american_mark_equals_european_at_zero_rate` asserts it — flipping the
style flag under default config moves no marks.

---

## 2. Pricing models

Three pricers, three jobs, one discipline: f64 analytics only at the
mark/risk boundary; every ledger posting is integer quote-minor.

| Pricer | Domain | Cost | Role |
|---|---|---|---|
| **Black-Scholes-Merton** (`OptionAnalytics::price`) | European, finite τ | 81 ns | European marks, greeks, IV inversion |
| **Barone-Adesi & Whaley** (`AmericanAnalytics::baw_price`) | American, finite τ | 10.7 µs (r>0) / 81 ns (r=0) | American marks, scenario grid, American IV inversion |
| **Merton perpetual** (`AmericanAnalytics::merton_perpetual`) | American, τ = ∞ | 23 ns | the everlasting analytic anchor; the τ→∞ limit BAW converges to |

Plus a **CRR binomial reference** (`AmericanAnalytics::crr_price`,
test/audit-only): the referee that validates BAW. The test suite
prices a grid of American puts and dividend-bearing calls against a
2,000-step binomial tree and requires agreement within 2% of strike —
the same standard a bank's model-validation desk would apply.

### 2.1 The cost-of-carry convention

All American pricers take an explicit carry `b` (`b = r − q`). The
venue's convention is `b = r` (no dividends on crypto underlyings),
which yields Merton's special cases as *theorems the code inherits*:

* `b ≥ r` ⇒ American call ≡ European call (never exercise early);
* `r = 0, b = 0` ⇒ early exercise suboptimal for both flavors ⇒ the
  pricers return the European value **exactly**.

Consequences: (1) the default configuration prices styles
identically — no mark migration risk; (2) a future per-underlying
carry (staking yield, funding-implied borrow) is a parameter, not a
rewrite; (3) American puts acquire their early-exercise premium only
when `r > 0` is configured — exactly when it is economically real.

### 2.2 BAW internals (why our boundary solve is bisection)

BAW adds an early-exercise premium `A·(S/S*)^q` to the European
value, where `q` is a root of `½σ²q² + (b − ½σ²)q − r = 0` and `S*`
is the optimal exercise boundary solved from the smooth-pasting
equation:

```
call:  f(S) = S − K − c(S) − (S/q₂)(1 − e^{(b−r)T} N(d₁(S))) = 0,  S* > K
put:   f(S) = K − S − p(S) + (S/q₁)(1 − e^{(b−r)T} N(−d₁(S))) = 0, S* < K
```

The naive fixed-point iteration (`S ← K ∓ p(S) ± (S/q)(1 − …)`)
**diverges** from a strike seed — the first test run put the boundary
below zero and priced an ATM American put at 0. The shipped
implementation solves the residual by **bracketed bisection**: the
brackets are structural (`f(0⁺) = K(1−e^{−rT}) > 0`, `f(K) < 0` for
puts; geometric expansion to sign-flip for calls), so convergence is
unconditional and the exercise-region kink (where Newton's method
misbehaves) is harmless. This is documented because it is the exact
failure mode a "textbook" implementation ships with.

### 2.3 Merton perpetual — the everlasting anchor

The perpetual American option has a closed form: with β₁ > 1 and
β₂ < 0 the roots of the same quadratic,

```
call:  S* = K·β₁/(β₁−1),   V(S) = (S*−K)·(S/S*)^β₁   for S < S*
put:   S* = K·β₂/(β₂−1),   V(S) = (K−S*)·(S/S*)^β₂   for S > S*
```

with the Merton special cases (call with `b ≥ r` is worth spot and
never exercises; at `r = b = 0` the perpetual put is worth K —
undiscounted recurrence strikes every boundary eventually). The test
suite pins: BAW at τ = 50y is within 2% of Merton; monotonicity;
boundary sides; special cases exact.

**How it is used:** an everlasting American market is *marked* at BAW
over its effective maturity (roll interval × maturity multiple — the
Paradigm concentration horizon), not at Merton. Merton is the limit
that validates the pricer and the sanity bound for heavily-extended
maturity multiples. This keeps one mark discipline (finite-τ
scenario-margined BAW) across dated and everlasting American markets
alike.

### 2.4 Marks, end to end

`build_marks` per option market:

1. **τ** — dated: calendar distance to expiry; everlasting:
   `interval × maturity_multiple`;
2. **σ** — the governed volatility surface (`poc-volsurface`): EWMA
   blend of anchor IV and book-implied IV (inverted through BAW for
   American markets, BSM for European — the style never crosses);
3. **price** — BAW (American) or BSM (European) at (S, K, τ, σ, r, b);
4. bands, staleness fallbacks, and the halt circuit breakers as for
   every other mark.

The vol-surface inversion for American books uses `implied_vol` over
the BAW curve with the American lower bound (undiscounted intrinsic);
at the zero-rate convention both inversions coincide bit-for-bit
(tested), and the style flag exists so a non-zero-rate future cannot
silently mis-invert American quotes.

---

## 3. The American early-exercise lifecycle

### 3.1 Command and queue

```
Command::Exercise { subaccount, symbol, lots, now }
    │  plan_exercise (pure):
    │    · market is an American option            → else "not-an-option"/"european-style"
    │    · dated market not expired
    │    · account holds lots > 0, lots ≤ position
    │    · underlying not halted
    ▼
Event::ExerciseQueued { request_id, settle_at = now + settlement_twap_ms }
```

The requested lots are **not locked**. The holder may keep trading
the position; settlement takes `min(requested, position at settle)`.
This is deliberate: it keeps the matching path untouched (no
reservation accounting threaded through every order check), it can
never over-settle, and the only party a "trade away then settle less"
sequence can hurt is the holder who chose it. Self-limiting beats
self-blocking.

### 3.2 TWAP settlement

At `settle_at` the sweep strikes intrinsic on the underlying oracle's
**time-weighted average over the window ending at `settle_at`** — the
same 30-minute (configurable) defense the dated-expiry settlement
uses. A manipulator must hold the *average* of the whole window, not
catch a single wick. If the TWAP is not computable, the request
**defers one window** (`ExerciseDeferred`) — a fail-safe that never
settles on a degraded price. The step-TWAP holds the last accepted
mark forward through gaps (tested: sparse-oracle windows settle at
the held price, exactly).

### 3.3 Pro-rata short assignment — exact largest remainder

Settlement assigns the exercised lots across every short position
pro-rata by short size:

```
base_i   = floor( |short_i| × settled / total_short )
rem_i    = ( |short_i| × settled ) mod total_short
leftover = settled − Σ base_i            // < number of shorts
```

The `leftover` lots go one each to the shorts with the largest
`rem_i`, ties by subaccount id ascending — a single pass, because
`Σ frac < n` bounds the remainder count. The result is *exact*:
`Σ assignment.lots == settled_lots` always, which is precisely the
condition that the position ledger stays zero-sum through settlement.
The zero-sum open interest invariant (`total_short = total_long ≥
settled`) guarantees the assignment is never short of lots; a
defensive clamp settles only what is assignable if state were ever
inconsistent.

Worked example (from the engine test, bit-exact): three accounts
trade at a $24,000/BTC premium on a $80k-strike call, spot $100k.
The long tenders 3 lots. Window closes, TWAP $100k, intrinsic
$2,000/lot, fee 5 bp:

* long closes 3 lots at intrinsic: realized −$120 vs entry (it burns
  the extrinsic it paid), then −$0.30 fee;
* short A (2 lots) realizes +$80; short B (1 lot) +$40;
* fee routes 60/30/10 house/insurance/buyback (insurance share subject
  to the coverage ladder);
* Σ tracked value unchanged — conservation holds to the minor unit.

### 3.4 Margin interaction

The assignment is never margin-destructive *relative to mark*: a short
assigned at TWAP intrinsic pays `intrinsic ≤ mark` (BAW value) — the
obligation extinguished at or below its mark valuation. The long
receives intrinsic ≥ 0. Assigned shorts that were already marginal
breach maintenance and are handled by the liquidation cascade later
in the same sweep — ordering that matters, and is tested.

---

## 4. Margin: the American treatment

Portfolio margin (SFPM scenario grid, `poc-margin::portfolio`) reprices
every option leg under the spot/vol shock grid. For American legs the
repricing runs through BAW (the early-exercise premium is *in* the
scenario values), and that is the whole charge — deliberately **no
separate assignment add-on**, for a reason worth writing down:

> An assigned short settles at TWAP intrinsic, which is bounded above
> by the American mark. The BAW-valued scenario grid therefore bounds
> assignment loss from above under every grid shock — an extra
> "assignment buffer" would double-charge a risk the grid already
> captures.

Long American options are premium-unpaid (variation-margin convention)
and require no further margin. Shorts carry the short-option minimum
charge (SOMC floor, bps of spot) as for European shorts.

At the venue default (r = 0) American and European margin numbers are
identical — same pricer output. With `risk_free_rate > 0`, American
puts charge strictly more than European (correctly: the short of an
American put faces early exercise and loses the time-value tail).

---

## 5. The everlasting roll (funding) — both styles

For everlasting markets the funding interval settles the mark-premium
TWAP from longs to shorts (Paradigm's mechanism: the funding *is* the
roll; the effective maturity `interval × multiple` is the horizon the
mark, greeks, and margin grid all share — see
`docs/DESIGN_SOURCES.md` §1). American style adds the exercise right
*on top* of the roll; the two mechanisms are complementary:

* **funding** pins the *extrinsic* (premium over parity) to the
  surface;
* **exercise** pins the *intrinsic floor* (a deep-ITM long can realize
  at any time, so the mark cannot durably trade below intrinsic).

Together they make the everlasting American option the
best-behaved-perpetual product in the design space: both ends of the
moneyness curve have an active convergence mechanism. This is exactly
the Everstrike thesis (see `docs/research/ZKLIGHTER.md` §lineage and
the everlasting-options literature).

---

## 6. Fees and economics

* **Exercise fee** — `exercise_fee_bps` of intrinsic proceeds, charged
  to the long, routed through the standard revenue split
  (house/insurance/buyback with the coverage ladder). Default 5 bp.
  It prices the operational cost of forced assignment and deters
  zero-value exercise spam, while never blocking the right itself
  (an OTM exercise settles at zero intrinsic, zero fee, and destroys
  only the holder's own position — their right, their choice).
* **Trading fees** — unchanged: option taker fee is
  `min(rate × underlying notional, 12.5% × premium)` (the Deribit /
  Derive premium cap, F-2), makers pay zero in RFQ/blocks.
* **Settlement at expiry** (dated) — unchanged 30-min TWAP intrinsic
  payout; exercise and expiry share the accounting primitive
  (`apply_fill` at per-base intrinsic against entry), so both paths
  conserve identically.

---

## 7. What changed vs. what didn't (migration notes)

| Concern | Answer |
|---|---|
| Do existing European markets change? | No. `ExerciseStyle::European` is the default; codec, listing templates, marks — all bit-identical. |
| Do marks move when I flip a market to American? | Not under default `risk_free_rate = 0` (BAW ≡ BSM, asserted by test). With `r > 0`: American puts mark higher — correctly. |
| Can exercise break conservation? | No: exact largest-remainder assignment guarantees `Σ assigned == settled`; the fee is routed like every other fee. Tested to the minor unit. |
| Can exercise be gamed via oracle? | The TWAP window is the defense; single-print manipulation moves the average by `print/window`. Halts defer settlement entirely. |
| Does assignment cascade? | Assigned shorts breaching maintenance are liquidated by the same-sweep cascade — by design, and tested. |
| Is exercise replay-deterministic? | Yes: plan/apply/journal, ids monotonic, assignment pro-rata with deterministic tie-break. Bit-for-bit replay test included. |

---

## 8. Test index (options-specific)

`poc-margin` (42 tests): BAW vs 2,000-step CRR binomial (puts and
dividend calls), American ≥ European, zero-rate collapse, deep-ITM
intrinsic convergence, boundary sides, Merton properties and BAW→
Merton τ→∞ convergence, American IV round-trip and bounds, style-aware
leg reprice.

`poc-volsurface` (20): American inversion coincides with European at
zero rate; gating parity.

`poc-engine/tests/american_exercise.rs` (7): full lifecycle with
pro-rata assignment and exact cash economics; European rejection;
position requirements; sparse-oracle settlement; position-churn cap;
bit-for-bit replay; mark-equality migration invariant.

Property suites (`properties.rs`): conservation and book sanity under
random command streams including exercises.
