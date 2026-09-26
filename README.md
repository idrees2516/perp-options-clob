# perp-options-clob

A **perpetuals + options central limit order book** engine in Rust: the
matching core, portfolio margin, oracle defense, funding economics, and
liquidation cascade of a production venue — as a deterministic, replayable,
event-sourced library.

```
cargo run -p poc-demo     # watch a full session: quoting → trading → funding → crash → liquidation → expiry
cargo test --workspace    # 100+ unit + integration tests, including journal-replay determinism
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

## Documentation

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — the why behind every design decision, mapped to production systems.
- [`docs/GAP_ANALYSIS.md`](docs/GAP_ANALYSIS.md) — the full implementation audit: what is implemented and verified, every unimplemented part of the designed protocol (including the unwired Everlasting Options roll and the missing option fee premium cap), competitor-mandated features absent from the protocol (RFQ, auctions, volatility surface, market data, ...), the completed economic-incentive architecture, and the prioritized 41-item roadmap (P0 / P1 / P2).
