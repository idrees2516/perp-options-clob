# perp-options-clob

A **perpetuals + options central limit order book** engine in Rust: the
matching core, portfolio margin, oracle defense, funding economics, and
liquidation cascade of a production venue — as a deterministic, replayable,
event-sourced library.

```
cargo run -p poc-demo               # watch a full session: quoting → trading → funding → crash → liquidation → expiry
cargo test --workspace              # 268 unit + integration + property tests, journal-replay determinism included
cargo run --release -p poc-bench -- micro    # benchmarks (docs/BENCHMARKS.md)
cargo run --release -p poc-bench -- stress    # architecture stress scenarios (docs/STRESS_TESTING.md)
```

## Why this exists

Crypto derivatives venues are converging on one architecture: an off-chain
matching engine whose every decision is a journaled event, margin computed
over the whole portfolio rather than per position, prices defended by
oracle redundancy, and liquidations backstopped by an insurance fund
rather than socialized losses. This workspace implements that stack
end-to-end, with the design choices that dominate production — and the
reasons for each — documented where they are made.

## The crates

| Crate | Role | The one design decision that defines it |
|---|---|---|
| [`poc-core`](crates/core) | Domain primitives | **Money is exact**: every amount is a `u128` minor unit through checked `mul_div` with an explicit rounding direction; `f64` never touches a ledger |
| [`poc-orderbook`](crates/orderbook) | CLOB matching | **Pure match, separate apply** — [`match_taker`](crates/orderbook/src/lib.rs) is side-effect-free, so live trading and event replay provably execute the same fills (flood-rs / dYdX v4 model) |
| [`poc-oracle`](crates/oracle) | Price defense | **Median + staleness + deviation quarantine + quorum halt**: one compromised feed cannot move a mark, and no mark at all beats a wrong mark |
| [`poc-economics`](crates/economics) | The business model | Volume-tiered fees with maker **rebates**, BitMEX-style **premium + interest** funding, a **60/30/10 revenue router** (house / insurance / buyback) with exact conservation, and a **budgeted liquidity-reward pool** scored on two-sided, tight quoting |
| [`poc-margin`](crates/margin) | Capital efficiency | **SFPM portfolio margin** (Derive V3 / CME SPAN): worst-case loss of the *whole book* under a standardized scenario grid — hedges net out; short options pay a **SOMC** tail floor |
| [`poc-risk`](crates/risk) | The safety net | Pre-trade gates that **simulate the worst-case fill before accepting it**; liquidation that is **partial first**, penalized into the **insurance fund**, with **ADL** as the last resort |
| [`poc-engine`](crates/engine) | The sequencer | **Commands in → events out → state applied**: one `apply_event` mutator, so the journal *is* the state machine — replay is bit-exact (verified in tests) |
| [`poc-volsurface`](crates/volsurface) | Option marks | **Anchor → blend → govern**: configured IV anchors the mark, the book's own quotes blend in inside a sanity band, and per-sweep clamps + staleness fallback keep it honest (G-04) |
| [`poc-rfq`](crates/rfq) | Dealer liquidity | Multi-leg **RFQ packages** with competitive quotes, atomic execution through the venue's margin/fee path, block trades with delayed broadcast, Derive's grouped multi-leg fee discounts (G-11/13/14/38) |
| [`poc-settlement`](crates/settlement) | On-chain layer | **State-diff settlement** (Lighter/dYdX pattern): merkleized account commitments, hash-chained batches, a conservation validator, and the withdrawal escape hatch (G-30) |
| [`poc-persist`](crates/persist) | Durability | A **framed, CRC'd, chain-hashed command WAL** with segment rotation, atomic checkpoints, and torn-tail-safe crash recovery (G-24) |
| [`poc-api`](crates/api) | Gateway protocol | Snapshot+delta market-data sessions with gap detection, **per-key monotonic nonces**, token-bucket rate limits, and the time-locked withdrawal pipeline (G-25/27/32) |
| [`poc-governance`](crates/governance) | Parameter safety | **Weighted multisig → timelock → grace** with an instant guardian veto; every transition is a journalable event (G-33) |
| [`poc-bench`](crates/bench) | Evidence | Deterministic benchmarks + whole-architecture stress scenarios (flash crash, vol spike, spam, cascade, oracle split, crash recovery) |
| [`poc-demo`](crates/demo) | Proof of life | A scripted session exercising every subsystem, ending with a journal-replay audit |

## Architecture in one picture

```
            Command (place / cancel / oracle / deposit / tick)
                              │
                    ┌─────────▼─────────┐
                    │      Engine       │   plan(&self)   → Vec<Event>   (pure decisions)
                    │  (poc-engine)     │   apply(&mut)   → state        (single mutator)
                    └─────────┬─────────┘
        ┌──────────┬──────────┼───────────┬────────────┬─────────────┐
        ▼          ▼          ▼           ▼            ▼             ▼
   orderbook    oracle     margin      risk        economics     accounts
   (match/     (median/   (SFPM      (pre-trade,  (fees, funding, (positions,
    apply)      quarantine) scenarios)  liquidation)  routing,       cash, PnL)
                                                       rewards)
        └──────────┴──────────┴───────────┴────────────┴─────────────┘
                              │
                    ┌─────────▼─────────┐
                    │   Event journal   │  TradeExecuted · FundingPaid · Liquidation ·
                    │ (append-only)     │  OptionExpiry · RewardPaid · MarketHalted …
                    └─────────┬─────────┘
                              │  replay() reconstructs state bit-for-bit
```

## The economics, briefly

**Fees pay for liquidity, not the other way round.** Takers pay 4.5bps down
to 0.9bps by 30-day volume; makers reach a **rebate** at the top tier. Of
every unit of fee income, 60% is house revenue, 30% feeds the insurance
fund (buying deleveraging headroom = lower safe maintenance margins), and
10% feeds a buyback pool.

**Funding tethers the synthetic to the real.** Perp funding is the BitMEX
premium + interest model: `rate = clamp(interest + clamp((markTWAP −
indexTWAP)/index), ±cap)`, longs pay shorts, computed once per interval
and applied uniformly so the system is exactly zero-sum. For options, the
[Paradigm "Everlasting Options"](https://www.paradigm.xyz/2021/05/everlasting-options)
mechanism — the roll paid as premium TWAP — is implemented alongside the
dated-expiry path as the framework for never-expiring structures.

**Portfolio margin charges risk, not positions.** A short call hedged by a
long perp holds offsetting exposure; the SFPM scenario grid (spot ±
fractions of the scan range × vol shifts) nets them and charges the
residual. Naked short gamma pays the SOMC floor because wings move
further than any scan range.

**Liquidation is a cascade, not an event.** Under-margined accounts are
ranked by deficit; the planner closes the largest risk contributors
first, crossing whatever book liquidity exists at prices *better* than
the penalized mark, with the insurance fund as buyer of last resort at the
penalized price. The penalty funds the backstop. Only when the fund is
exhausted does ADL force-close the most profitable counterparties at the
bankruptcy price — the last resort every venue ships and hopes to never
use.

## Marks never listen to the book

The perp mark is the oracle spot; the option mark is Black-Scholes at the
oracle spot with a configured IV. A book-derived mark would let one large
order move every account's margin — circularity and a manipulation
vector. The order book influences the system only through the funding
premium (BBO-mid TWAP vs index TWAP), where clamps bound its power.

## Determinism

The same command stream always produces the same event journal, and
replaying that journal into a fresh engine reproduces the state exactly —
books, positions, balances, statistics. This is the property that makes
off-chain matching auditable, and it is asserted by test:

- `engine::tests::determinism_and_replay` — run twice, replay once, all
  three must agree.
- `engine::tests::conservation_across_trades_and_funding` — cash leaves
  the system only through routed fees; funding is zero-sum.

## Status

Everything above is implemented and tested in-memory. The natural next
layers — persistence (journal → disk), a gRPC/REST + WebSocket API in
front of the sequencer, and on-chain settlement of the journal — are out
of scope for this repository and are where the design deliberately stops:
every boundary they would touch (commands, events, marks) is already a
typed, versionable interface.

## License

MIT.

## Enabling CI

The GitHub Actions workflow lives at [`docs/ci.yml`](docs/ci.yml) (formatting,
clippy, a stable/beta test matrix, and a demo-run job). The push token used
for this repository lacks the `workflow` scope, so the file could not be
placed directly under `.github/workflows/`. To activate CI, either:

```bash
mkdir -p .github/workflows && mv docs/ci.yml .github/workflows/ci.yml && git commit -am "ci: activate workflow" && git push
```

…or create `.github/workflows/ci.yml` with the file's contents through the
GitHub web UI, or push with a token that has the `workflow` scope.

## September 2026 — Gap-Closure Release (v0.2)

This release closes the highest-priority items from the audit's 41-item gap
register, with design choices taken from live competitor documentation
(see `docs/DESIGN_SOURCES.md` for the extracted rules and their sources):

- **The everlasting roll is live (F-1/G-01)** — `OptionVariant::Everlasting`
  markets settle the Paradigm Everlasting-Options funding each interval:
  longs pay shorts the mark-premium TWAP per lot; the claim never expires.
  Effective maturity = roll interval × configured multiple, shared by marks,
  greeks, and the margin grid.
- **Live governed volatility surface (G-04)** — new `poc-volsurface` crate:
  anchor → blend → govern. Book touches invert through the implied-vol
  solver inside a sanity band, EWMA-blend into the mark IV, per-sweep move
  clamps and staleness fallback to the anchor. Every observation is
  journaled (`SurfaceObserved`), so replay reproduces the surface exactly.
- **Option fee premium caps (F-2)** — `min(rate × underlying notional,
  12.5% × premium)` for takers, `2.5%` for makers (the Deribit/Derive rule).
- **RFQ system (G-11/G-14/G-38)** — new `poc-rfq` crate + engine wiring:
  multi-leg packages, private directed counterparties, firm maker quotes
  with TTL, atomic execution through the full margin + fee + journal path,
  Derive's grouped multi-leg fee discounts (cheapest group free, next two
  at 50%), maker-pays-zero dealer economics.
- **Block trades with delayed broadcast (G-13)** — venue-cleared negotiated
  packages that print to the public tape after a 15-minute delay.
- **Market-maker protection (MMP)** — per (subaccount, currency) rolling
  windows on cumulative fill size and net delta; trips cancel resting
  orders and freeze the currency for the configured window.
- **Cancel-on-disconnect (G-37)** — persisted setting; a dropped session
  pulls the subaccount's resting orders.
- **Exact time-indexed fee-volume ledger (G-22)** — day-bucketed trailing
  30-day window replaces the decay approximation.
- **Insurance coverage policy (G-40)** — liquidation-penalty ladder that
  boosts when coverage thins; above target, insurance fee share overflows
  to the buyback pool.
- **Circuit breakers (G-21)** — sustained BBO/oracle dislocation trips a
  per-instrument cooldown.
- **Internal transfers (G-31)** and **position greeks view (G-28)**.

Test suite: **181 passing** (was 123). Replay determinism extends to the new
subsystems — see `new_features_replay_bit_for_bit`.

## Documentation

| Doc | Contents |
|---|---|
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | The full system: determinism model, order lifecycle, marks, margin, liquidations, funding, fees, settlement, persistence, gateway, governance |
| [`docs/INVARIANTS.md`](docs/INVARIANTS.md) | The invariant catalog (I-1..I-20) with enforcement points and the tests that guard each |
| [`docs/FUZZING.md`](docs/FUZZING.md) | Property + fuzz strategy, and the three real defects it found (oracle liveness, ADL inversion, fee-routing leak) |
| [`docs/STRESS_TESTING.md`](docs/STRESS_TESTING.md) | Flash crash, vol spike, spam, cascade, oracle split, crash recovery — results |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | Micro-benchmark methodology and numbers |
| [`docs/EFFICIENCY.md`](docs/EFFICIENCY.md) | Ideas adopted from Lighter (state-diff settlement, per-market cores) and deliberate divergences |
| [`docs/GAP_ANALYSIS.md`](https://github.com/idrees2516/perp-options-clob/blob/main/docs/GAP_ANALYSIS.md) | The original audit + 41-item gap register this roadmap closes |
| [`docs/DESIGN_SOURCES.md`](docs/DESIGN_SOURCES.md) | Design choices extracted from Derive V3, Paradex, Paradigm, and the venue benchmark set |

## License

MIT.
