# Perpetual Options CLOB Protocol — Implementation Audit, Unimplemented Parts, and Competitive Gap Analysis

> Repository: `idrees2516/perp-options-clob` (Rust, 8 crates, 10,331 LOC, 123 tests)
> Benchmark set: Paradigm · Deribit · Derive V3 · dYdX v4 · Hyperliquid · BitMEX · CME SPAN — September 2026 · Version 1.0


## 1. Executive Summary

This report is a full-depth engineering audit of the perp-options-clob workspace: 10,331 lines of Rust across eight crates, 123 passing tests, a deterministic event-sourced engine, and a documented design lineage that deliberately distills patterns from flood-rs (Paradigm), Everlasting Options (Paradigm), Derive V3, dYdX v4, BitMEX, Deribit, and CME SPAN. The audit had two mandates: first, to verify what the implementation actually does rather than what its documentation claims; and second, to enumerate every unimplemented part of the designed protocol and every capability that the competitor set ships as table stakes but this protocol lacks. Every finding below is traceable to a specific file, function, or test in the repository.

The core verdict is positive. The parts of the system that exist are engineered to a standard that is genuinely competitive with production venues: a pure-match/apply orderbook whose journal replays bit-for-bit, integer-only money arithmetic with explicit rounding directions, SFPM-style portfolio margin with a scenario grid and short-option minimum charge, a layered oracle defense with median aggregation and quorum halts, a complete liquidation cascade from partial closures through an insurance fund to auto-deleveraging, and an economics layer that routes every unit of fee revenue through a conserved 60/30/10 split. These are the subsystems where an error would destroy solvency, and they were built first. That ordering was correct.

The audit also surfaced two critical defects that sit precisely at the protocol's claimed identity, and they dominate the priority register:

- **F-1 (Critical) — The perpetual-options mechanism is not wired into the engine.** The Everlasting Options roll — the funding mechanism that makes options never-expiring and the feature the repository is named after — exists only as a tested formula (EverlastingRoll in poc-economics/src/funding.rs). The engine's funding sweep (poc-engine/src/sweep.rs, plan_funding) explicitly skips every non-perpetual instrument, and every option market in the engine is dated: a fixed expiry settled once over a 30-minute TWAP, then delisted. As it stands, the running system is a perpetuals CLOB with a competent dated-options complex, not a perpetual options venue.
- **F-2 (Critical) — Option fees are charged on notional with no premium cap.** FeeCalculator applies basis points of notional to every instrument equally. For a deep out-of-the-money option whose premium is a small fraction of notional, a 4 bps-of-notional taker fee can exceed the entire premium by an order of magnitude. Every production options venue solves this with a cap: Deribit charges the lesser of a notional rate and a fraction (12.5%) of the option premium. The fix is small, but until it lands the fee schedule is economically unusable for options.

Beyond those two, the audit registers 41 discrete gaps organized into three tiers: 9 items at P0 (safety and correctness, including the two findings above, a live volatility surface, journal persistence, and the API/market-data surface), 22 items at P1 (competitive parity: the RFQ and block-trading layer that defines Paradigm's franchise, iceberg and trailing orders, multi-collateral margin, iterative ADL, Greeks APIs, and parameter governance), and 10 items at P2 (differentiation: auctions, FIX connectivity, LP vaults, and proof-of-reserves tooling). The distribution is healthy in one specific sense: almost no gap sits inside the solvency-critical core, which means the roadmap is primarily an additive build-out rather than a repair campaign.

The recommended sequence, argued in Part V, is: close F-1 and F-2 and stand up the volatility surface (they are one workstream — a perpetual option cannot be marked or funded without a live IV); then persist the journal and expose the API; then build the RFQ layer, which is the single largest competitive gap and the one that most directly borrows Paradigm's institutional playbook; then round out parity features in descending order of user-visible value. The remainder of this document is the evidence for that ranking.


## 2. Audit Scope, Method, and Baseline


### 2.1 What Was Audited

The audited artifact is the complete workspace at the time of audit: all eight crates, their unit and integration tests, the demo binary, and the two design documents (README.md and ARCHITECTURE.md) whose claims were checked against the code. Every crate was read at the source level, not sampled. Line counts and the disposition of each crate are summarized in Table 1. The audit statement for each subsystem in Part I cites the specific functions and tests that substantiate it, so that every claim can be re-verified by a reviewer with the repository open.


**Table 1 — Audited workspace composition**

| Crate | LOC | Role in the system |
|---|---|---|
| poc-core | 1,146 | Domain primitives: u128 money, mul_div rounding, tick/lot grids, instrument model |
| poc-orderbook | 806 | CLOB matching: flood-rs layout, pure match_taker, STP, FOK/IOC, depth snapshots |
| poc-oracle | 345 | Median aggregation, staleness, deviation quarantine, quorum halt, step TWAPs |
| poc-margin | 1,588 | Black-Scholes analytics, SFPM scenario margin, SOMC, account PnL engine |
| poc-risk | 1,422 | Typed pre-trade gates, liquidation queue/planner/cascade, ADL |
| poc-economics | 1,256 | Fee ladder with rebates, perp funding, everlasting roll, revenue router, LP incentives |
| poc-engine | 3,538 | Sequencer: plan/apply discipline, 20-variant journal, replay, sweep pipeline |
| poc-demo | 499 | Scripted end-to-end session with a journal-replay audit |

Two terms used throughout deserve definitions up front. Unimplemented parts (Part II) are components the design itself acknowledges as missing or that the audit found unwired — the distance between the protocol's own specification and its code. Competitor-mandatory features (Part III) are capabilities that every venue in the benchmark set ships but that this protocol's design never scoped at all — the distance between the protocol and the market it intends to enter. The distinction matters because the two categories have different failure modes: the first erodes credibility (the code does not do what it says), while the second erodes competitiveness (venues that have it will take flow the protocol cannot even quote for).


### 2.2 The Benchmark Set

Seven production systems form the benchmark, chosen because each contributes a distinct, load-bearing pattern that a perpetual options CLOB must either adopt or consciously reject. The user's directive named Paradigm explicitly, and its fingerprints are already in the codebase (the flood-rs orderbook layout and the Everlasting Options mechanism); the audit therefore weights Paradigm's institutional liquidity playbook — RFQ, auctions, block trading — most heavily in the gap analysis. Table 2 states what each system contributes to the evaluation and where its influence already appears in the code.


**Table 2 — Benchmark systems and the patterns each contributes**

| System | What it contributes to this evaluation | Already present in code? |
|---|---|---|
| Paradigm (flood-rs, Everlasting Options, RFQ network) | BTreeMap price-level orderbook layout; the roll-payment mechanism for never-expiring options; multi-dealer RFQ and auction liquidity for institutional blocks | Layout and roll formula yes; RFQ/auction layer no |
| Deribit | Options venue gold standard: option fee caps as % of premium, DVOL volatility index, iceberg and advanced orders, insurance-fund discipline, auto strike/expiry listings | Liquidation cascade, penalty-to-insurance pattern; fee cap and the rest no |
| Derive V3 (formerly Lyra) | Subaccount portfolio margin (SFPM), premium-unpaid option accounting, 30-minute TWAP settlements, batch/amend APIs, RFQ rails | SFPM, TWAP settlements yes; API and RFQ no |
| dYdX v4 | Off-chain deterministic matching with on-chain settlement, exact time-indexed fee-volume tiers, governance-parameter machinery, circuit breakers | Determinism model yes; settlement, tiers, breakers no |
| Hyperliquid | Fully on-chain CLOB performance envelope, maker-rebate fee ladder shape, HLP backstop liquidity as insurance-fund alternative, proof-of-reserves culture | Fee ladder shape yes; backstop and PoR no |
| BitMEX | Premium + interest funding model with clamps, ADL concept, liquidation-velocity protections, historical funding-rate bounds | Funding model and ADL skeleton yes; velocity protections no |
| CME SPAN | Scenario-grid margining concept, short-option minimum charge | Both adopted verbatim |


### 2.3 Evaluation Framework

Each subsystem was scored against six dimensions, and every gap in the register carries the dimension it threatens. Correctness asks whether the invariant holds under adversarial input, with determinism and conservation treated as the two non-negotiables. Capital efficiency asks whether margin charges risk rather than positions, which is the difference between a venue professionals can trade on and one they cannot. Liquidity asks whether the mechanism design attracts two-sided depth at launch and keeps it through stress. Economic design asks whether fees, rebates, and incentives are internally consistent and un-gameable. Operational readiness asks whether the system can run unattended: persistence, observability, breakers, and administration. Trust model asks who can cheat, what they can steal, and what detects it. The weighting is deliberately asymmetric: a gap that threatens solvency (correctness, trust) outranks any gap that merely threatens growth, which is why the two Part II findings outrank the (much larger) RFQ build-out in priority.


### 2.4 Method Notes and Limitations

Three limitations bound this audit's claims. First, it is a static and dynamic code review, not a formal verification: the determinism and conservation properties are asserted by tests the audit read and re-ran, not proven exhaustively. Second, competitor fee levels and feature sets cited in Part IV are indicative as of the audit date and are used structurally (the shape of a fee schedule, the existence of a cap) rather than as exact numbers to copy; any production decision should re-verify current published schedules. Third, the effort classes in the roadmap (S under one week, M one to four weeks, L over a month for one experienced Rust engineer) assume the existing codebase's conventions and are planning aids, not commitments. Within those bounds, the findings are stated with the confidence the evidence supports.


## 3. Part I — Implementation Audit: What Exists and What It Proves

This part records what the implementation verifiably does, subsystem by subsystem, with the design decision that defines each and the evidence that it holds. It is written to be read against the source; section references name the files and functions that carry each claim. The summary judgment is deferred to 3.9, but the pattern throughout is consistent: where the code exists, it is the production-shaped version of the pattern, not a sketch of it.


### 3.1 The Determinism Model and Event Journal

The engine's defining discipline is that every state mutation flows through a single function, Engine::apply_event, and that commands never mutate directly: Command::process plans a Vec<Event> against an immutable view, and the same vector is then applied and journaled. Two replay-critical details show the discipline is real rather than cosmetic. First, the journal includes MarketListed (instrument registrations) and ClockAdvanced (tick-driven time), which are exactly the two facts a naive event-sourced engine forgets to journal and then cannot reproduce — the worklog shows both were added specifically to fix a replay divergence found during development. Second, numbers computed during planning (fees, margin reservations, settlement amounts, liquidation absorption amounts) travel inside the events themselves, so the apply stage never re-derives them; replay cannot disagree with live execution because there is nothing left to disagree about.

The property is enforced by test, not by inspection. engine::tests::determinism_and_replay drives a mixed session (trading, funding, expiry, halts), runs it twice, replays the journal into a third engine, and asserts all three agree on every account view and every statistic. This is the property that makes off-chain matching auditable at all — it is what dYdX v4 and Derive V3 sell to their users — and it is genuinely present here. The audit found no state mutation outside apply_event in the engine crate.


### 3.2 Numerics: Integer Money with Explicit Rounding

All money is u128 quote-minor units; prices are integer ticks on the instrument grid; quantities are integer lots. Every product and quotient goes through poc_core::mul_div with one of three Rounding modes (Floor, Ceil, NearestHalfUp), and the policy mapping the mode to the transaction is written down and enforced: fees owed to the house round up, rebates and rewards credited to users round down, and mid-flight risk figures round half-up and are never posted to a ledger without a final directional decision. apply_bps implements the signed-rate case with the same asymmetry. Checked arithmetic returns Option throughout, and degraded inputs saturate rather than panic, so the engine cannot be knocked over by a hostile decimal.

The audit's one substantive observation here is that f64 is admitted in exactly two places — Black-Scholes analytics inside poc-margin and the scenario scanner that consumes them — which produce risk figures (margin requirements) rather than ledger entries. That boundary is the correct one: it is the same boundary Deribit and Derive draw between their pricing analytics and their settlement systems. Keeping the boundary typed and documented (as ARCHITECTURE.md does) is what prevents the slow drift back toward floating-point money that kills most amateur exchanges.


### 3.3 Orderbook and Matching Core

poc-orderbook implements the flood-rs layout: BTreeMap price levels with FIFO VecDeque order queues, and bids keyed by an inverted price so that best-quote maintenance is O(log n) with no reversed iteration on the hot path. The load-bearing decision is that LimitOrderBook::match_taker is pure — it reads book state and returns a MatchOutcome of fills and self-trade effects without mutating — and that the engine applies those results (or replays the corresponding events) through apply_fill and apply_stp. Live trading and journal replay therefore execute the same fills by construction, which is the dYdX v4 determinism argument transplanted into this codebase.

The microstructure is production-shaped. Fills execute at the maker's price, giving takers price improvement, which is the universal CLOB convention. Four self-trade-prevention modes (CancelNewest default, CancelOldest, CancelBoth, DecrementAndCancel) are resolved inside match_taker so replay is exact — the audit verified DecrementAndCancel's taker_consumed_lots accounting, which is the mode most implementations get wrong. FOK feasibility is checked against reachable volume including STP consumption before any fill is emitted, and IOC remainder cancellation is journaled with its own close reason. The no-cross invariant is enforced at insert (insert_resting refuses crossing orders), which is where it should be enforced: an invariant that depends on caller discipline is not an invariant.

What is absent from the orderbook is the set of order types institutional flow expects: iceberg/hidden size, trailing stops, one-cancels-other pairs, batch place/cancel, and amend-in-place. Stop orders exist and park off-book until the oracle-anchored mark crosses their trigger — the right choice, since triggering off the book would let one print fire every stop — but they are plain stop-market and stop-limit only. These absences are registered as G-06 through G-10 and analyzed in Part III.


### 3.4 Oracle Defense

poc-oracle aggregates per underlying across named providers with a four-layer defense: median (so no single feed can drag the mark), staleness exclusion (silent providers stop counting), deviation quarantine (a provider more than 5% from the last accepted mark is quarantined until it returns to range), and quorum (below two healthy providers the mark goes None, the engine halts the underlying, and — the detail that matters — liquidations are skipped, because the engine will not act on a price nobody can defend). TWAP windows are ring-buffered step functions, so moving a funding index requires sustained capital across the whole window rather than one manipulated print.

The quarantine logic deserves its note in the audit because it is the layer most implementations omit: without it, a provider that is neither stale nor honest — a compromised feed printing plausible-but-wrong prices — passes median defense in a two-provider world by defining the median. With it, deviation is measured against the last accepted mark and the rogue feed is fenced. The remaining oracle gaps are about scope, not correctness: there is no volatility oracle (marks use a configured per-market IV), no push-model low-latency design (updates arrive as commands), and no secondary confirmation channel. Those are G-04 and its dependencies, and they sit at P0 because a perpetual options venue lives and dies by its vol marking.


### 3.5 Margin: SFPM Portfolio Margin with SOMC

poc-margin implements scenario-based portfolio margin in the Derive V3 / CME SPAN tradition, and it is the strongest subsystem in the codebase. The scanning range for each underlying defaults to the perp maintenance ratio — the venue's own definition of the liquidating move — and a grid of spot moves (plus or minus 1.0, 0.5, and 0.25 of that range) is crossed with relative volatility shifts (plus or minus 25%, and zero). Every leg, perps and options together, is repriced per scenario and the portfolio PnL is taken, so a short call hedged by a long perp nets out and only residual risk is capitalized. Maintenance margin adds the short-option minimum charge on net-short option legs — the SPAN answer to the fact that wings move further than any scan range — and initial margin is maintenance times the 1.4 Derive uplift. Underlyings sum additively with no cross-commodity offset, which is the SPAN convention.

The analytics underneath are a dependency-free Black-Scholes-Merton implementation with degenerate-input collapse (tau or sigma at zero price at discounted intrinsic rather than propagating NaN — the convention production risk systems use precisely because scenario grids legitimately probe sigma equals zero), delta and gamma, and an implied-volatility solver used to derive mark IVs from the book. The premium-unpaid accounting convention is implemented end-to-end: options exchange no cash at trade time, entries anchor unrealized PnL, and the single identity equity = cash + sum over positions of (mark - entry) x signed_qty covers deposits, withdrawals, funding, settlement, fees, and liquidation. The audit ran the conservation and funding tests and re-read the scenario scanner line by line; the margin numbers it produces are the ones the liquidation engine acts on, and they are derived honestly.

The margin system's gaps are inherited from the collateral model rather than its mathematics: single-currency cash only (no multi-collateral weights or haircuts — G-17), no interest on collateral, and no per-account Greeks limits (vega and gamma caps that cap concentration before position-lot caps bind — G-41). None of these change what the scanner computes for a portfolio; they change which portfolios are admissible.


### 3.6 Risk Gates and the Liquidation Cascade

Pre-trade risk runs entirely before mutation, and every rejection is a typed variant (twelve in poc_risk::Rejection) that a client layer can translate one-to-one into error codes, mirroring Derive V3's structured rejects. The gates include the price band around the mark, the post-only crossing check, reduce-only caps at current position, position and open-order limits, and the margin gate — which clones the account, applies the full fill at the order's own limit price plus worst-case taker fee, and asks the portfolio engine for the summary. Simulate-then-decide is what guarantees live and replayed decisions cannot diverge, and missing marks fail safe into rejection rather than acceptance.

The liquidation cascade is the most complete in the codebase, and it follows the Deribit pattern end to end. Detection ranks candidates by deficit (maintenance minus equity, counting negative equity fully) with deterministic tie-breaking. The LiquidationPlanner is pure: it ranks underlyings by margin contribution, closes the largest risk contributors first at penalized prices (mark adjusted by the 1.25% penalty), and bisects on the final leg to find the smallest closure that restores the account to maintenance plus a 20% restoration buffer — partial liquidation first, minimizing disruption, exactly Deribit's stated behavior. Execution then runs in phases: phase A crosses the book as an IOC taker with the penalized price as limit, so the account is filled at better prices wherever real liquidity exists; phase B sends the remainder to the insurance fund at the penalized price, the penalty being the fund's compensation for being buyer of last resort; whatever remains below zero is absorbed by the fund exactly (simulated, not projected); and only when the fund is exhausted does ADL force-close the most profitable opposite-side counterparties at the bankruptcy price. The fail-safes are the right ones: no live mark means no liquidation, and halt means no new orders.

Two quality caveats are registered. ADL pricing is single-shot — the bankruptcy price covers the projected deficit in one calculation, where production systems iterate to a fixed point — and the insurance fund is a virtual ledger: it books penalties and absorbed shortfalls but does not carry the closed risk as positions, so there is no fund-level inventory management or rebalancing. These are G-19 and G-23 respectively; both are parity issues, not correctness issues.


### 3.7 Economics: Fees, Funding, Routing, Incentives

The fee module implements the volume-tiered ladder every production CLOB converges on: taker 4.5 down to 0.9 basis points and maker 1.0 down to negative 0.5 (a rebate) by trailing 30-day volume, Hyperliquid and dYdX-shaped, with rebates floored and fees ceiled per the house rounding rules. The revenue router then splits gross fee income 60% house, 30% insurance, 10% buyback, with exact conservation enforced (floors per destination, remainder to the house, sum of allocations equals the amount always — verified by parameter sweep). The insight embedded in the 30% insurance share is worth stating because it is the document's best economic argument: every unit routed to the insurance fund buys deleveraging headroom, and deleveraging headroom is what lets a venue charge lower maintenance margins safely. The insurance allocation is not a cost center; it is the subsidy that pays for capital efficiency.

Perp funding is the BitMEX premium-plus-interest model: the premium index is the clamped difference between mark TWAP and index TWAP, the rate is premium plus a fixed interest component clamped to a cap, positive rates mean longs pay shorts, and the per-lot payment is computed once and applied uniformly to both sides so aggregate funding over a closed position set conserves exactly — asserted by test. The TWAPs are manipulation-resistant by construction: the index TWAP comes from the oracle's ring buffer, and the mark TWAP from BBO-mid samples the engine takes itself.

Liquidity incentives implement the industry's answer to the cold-start problem — rent depth with an explicit budget rather than wait for it — as a transparent score: size times proximity times a two-sided multiplier for quotes inside a band, accumulated per interval, with a fixed pool paid pro-rata and dust carried forward. Paying for quoting rather than fills kills the wash-trading incentive that naive volume-based programs create, and requiring two-sidedness pays for what takers actually consume. The audit's critique of this module is not its structure but its gaming surface, which is thin against a sophisticated market maker who quotes and pulls: it is addressed as G-39 in Part IV with a concrete hardening design.


### 3.8 Engine Integration and Test Quality

The sweep pipeline (poc-engine/src/sweep.rs) is where all time-driven behavior lives, and it is planned like everything else: stop triggers evaluated against marks, GTD expiry, perp funding from TWAPs, option expiry over a 30-minute settlement TWAP followed by delisting, liquidity scoring and reward settlement, and the full liquidation cascade. The engine surfaces read-only views (account, book, market state, statistics) suitable for an API layer, and replay() reconstructs state from a journal. The demo binary runs a scripted session that exercises every subsystem — quoting, taker flow, rewards, oracle quarantine, funding, a liquidation cascade, option expiry — and ends with a full accounting and replay audit.

The test suite is 123 tests across the workspace, and the audit's judgment is that they test the right things: determinism and replay equality, conservation of cash across trades and funding, zero-sum funding over closed position sets, liquidation behavior on a gradual decline (not just a cliff), option ITM settlement, quorum halt and resume, rogue-feed quarantine and reinstatement, STP mode interactions, and parameter sweeps over the revenue router. The gap in testing is not unit coverage but adversarial breadth: no fuzzing of the orderbook (flood-rs ships property tests for its engine), no margin-scenario differential testing against an independent pricer, and no simulation harness for cascade dynamics (what happens when the insurance fund is stressed by correlated multi-account liquidations). These are registered with the roadmap as they become binding constraints.


### 3.9 Assessment: Strengths, Verified Invariants, and Coverage

Table 3 consolidates the audit's assessment of the implemented core against the benchmark set, and Figure 1 places the same judgment on a radar of the ten subsystem dimensions used throughout this report. The visual summary is blunt on purpose: the solvency-critical core (matching, margin, oracle, liquidation) sits at or near the parity band, while everything that makes a venue operable or institutionally reachable — persistence, API, the RFQ liquidity layer, operations — sits near zero. The protocol has built the hard center of the exchange and almost none of the exchange around it.


**Table 3 — Audit assessment of implemented subsystems vs the benchmark set**

| Subsystem | Audit assessment | Nearest competitor analogue |
|---|---|---|
| Determinism / journal | Complete and test-enforced; plan-apply with numbers-in-events; replay bit-exact | dYdX v4, Derive V3 |
| Matching core | Production layout and semantics; missing only institutional order types | flood-rs, Hyperliquid |
| Oracle defense | Four-layer aggregation, sound; no vol oracle, no push model | Chainlink-style layering |
| Portfolio margin | SFPM grid + SOMC + premium-unpaid; strongest subsystem | Derive V3, CME SPAN |
| Liquidation / ADL | Full cascade with partial-first and insurance backstop; ADL single-shot | Deribit, BitMEX |
| Fees / revenue / rewards | Sound ladder, conserved router, quote-scored incentives; options fee cap missing | Hyperliquid, dYdX v4 |
| Perp funding | BitMEX model with clamps and TWAPs; zero-sum by construction | BitMEX, dYdX |
| Everlasting options roll | Formula and tests only — not wired into the engine (F-1) | Paradigm (paper mechanism) |
| Persistence / API / data | Absent — typed seams only | All venues ship these |
| On-chain settlement | Absent | dYdX v3/v4, Hyperliquid |


![Figure 1 — Subsystem coverage, audit assessment (0-10). The parity baseline is the set of capabilities the benchmark venues collectively ship; the score is audit judgment grounded in the source review of Part I.](docs/assets/fig1_radar.png)

*Figure 1 — Subsystem coverage, audit assessment (0-10). The parity baseline is the set of capabilities the benchmark venues collectively ship; the score is audit judgment grounded in the source review of Part I.*

Three invariants the whole architecture rests on were re-verified during the audit and are worth naming because everything in Parts II through V must preserve them: (1) the same command stream always produces the same journal, and the journal always reproduces the state; (2) cash leaves the system only through routed fees, funding is zero-sum over closed position sets, and revenue routing conserves exactly; and (3) no mutation ever precedes its risk check, and no mark nobody can defend ever drives a liquidation. Any implementation of any gap in this report that breaks one of these three has regressed the system, whatever else it improves.


## 4. Part II — Unimplemented Parts of the Designed Protocol

This part treats the distance between the protocol's own specification and its code. The design document lists six known simplifications; the audit confirms all six and adds four more that the design's own claims imply but the code does not deliver — most importantly F-1, which is the difference between the protocol's name and its behavior. Each item below states what exists, why the gap matters, what the benchmark venues do, the recommended design, and an effort class. The register identifiers (G-nn) carry through to the consolidated table in Appendix A and the roadmap in Part V.


### 4.1 F-1 / G-01 — The Everlasting Roll Is Not Wired Into the Engine (Critical)

What exists is a formula with tests: EverlastingRoll::payment_per_lot in poc-economics/src/funding.rs implements the Paradigm roll — the long pays the short the option's mark value each funding interval, making the claim never-expiring and the funding itself the roll — and concentration_days reports the effective maturity the interval implies. What does not exist is any call to it from the engine. The funding sweep in poc-engine/src/sweep.rs, plan_funding, matches every instrument against Instrument::Perp and continues past everything else; the OptionMarket struct carries a mandatory expiry_ts_ms; option lifecycle ends in plan_option_expiry with a 30-minute TWAP settlement and delisting. The engine, as shipped, lists only dated options. A position in the protocol's headline instrument cannot be opened.

Why it matters needs no elaboration beyond one sentence: this is the feature the repository is named for, it is the one mechanism none of the incumbent CLOBs ship natively, and it currently exists as dead code from the engine's point of view. Competitor practice splits into two camps. Paradigm's Everlasting Options paper specifies exactly the roll implemented here, funding per interval at the premium TWAP. Everstrike (the only live perpetual-options venue of note) pairs the roll with daily strike rebasing so contracts never drift deep in-the-money — an addition this protocol should adopt (it is G-03) but which is secondary to the wiring itself.

The recommended design keeps the existing event vocabulary and adds one instrument flavor. Extend the instrument registry with OptionMarket.variant = Everlasting, whose expiry_ts_ms is absent and whose per-interval lifecycle is: (1) mark the option off the volatility surface (G-04 is therefore a hard dependency — an everlasting option without a live mark has no funding basis); (2) accumulate the mark-premium TWAP over the interval in the existing ring-buffer pattern the perp already uses for BBO-mid samples; (3) at the interval boundary, emit a FundingSettled for the option symbol followed by FundingPaid events computed from EverlastingRoll::payment_per_lot, signed longs-pay-shorts, applied uniformly so conservation holds exactly as the perp path does; (4) never expire, never delist. The pre-trade margin path, the liquidation planner, and the account views need no changes — they already treat options through marks and premium-unpaid accounting, which is precisely why the design doc could claim the roll is a Mark-producer swap. Effort: M, dominated by the vol surface dependency. Acceptance is testable and should be: a session holding an everlasting position across three intervals, asserting the long's cumulative funding equals the sum of interval premium TWAPs and that closed-set conservation holds to the unit.


### 4.2 F-2 / G-02 — Option Fees Have No Premium Cap (Critical)

The engine computes trade fees as basis points of notional for every instrument alike (FeeCalculator::taker_fee and maker_fee, driven from the trade's notional_quote_minor). For perps this is correct. For options it is economically broken at the wings: a far out-of-the-money put at 0.1% of notional premium pays 4.5 basis points of notional in taker fees — 4.5 times the entire premium. The trader's expected loss to fees exceeds the instrument's maximum value by an order of magnitude, and rational quoting simply stops at the moneyness where fees cross premium. Every production options venue solves this with a cap. Deribit's schedule is the canonical shape: the fee is the lesser of a rate on underlying notional and a fraction (12.5%) of the option premium; the cap makes cheap wings tradeable while ITM options pay roughly the notional rate.


> **Recommended fee rule for options (drop-in)**
>
> option_fee = min( bps_rate x underlying_notional , cap_pct x premium ). Concretely: taker = min(4.5 bps of notional, 12.5% of premium), maker = min(1.0 bps of notional, 2.5% of premium), with the cap components following the same rounding asymmetry (ceil on fees, floor on rebates). The maker cap matters as much as the taker cap: an uncapped maker fee on a cheap wing is a tax on exactly the two-sided quoting the incentive program pays for.

The implementation is a contained change in the fee plan stage — compute premium_quote_minor per lot from the fill price (already available as ticks x tick_size) and take the min — plus regression tests asserting the cap binds on a wings trade and the notional rate binds ITM. Effort: S. The audit flags it P0 not because it is hard but because it is the difference between an options fee schedule that works and one that silently forbids a third of the surface.


### 4.3 G-04 — Live Volatility Surface (IV Is Configured, Not Surfaced)

Option marks are Black-Scholes at the oracle spot with a per-market IV supplied in EngineConfig.option_ivs — a fixed number per market, set at listing and never derived from anything. The design document honestly labels this a Mark-producer swap, and the audit confirms the seam is typed and clean. The gap is nonetheless P0 because three P0 items depend on it: the everlasting roll needs a premium mark to fund against; scenario margin already consumes an IV field whose credibility currently ends at config time; and any future Greeks exposure (G-28) inherits whatever the mark uses.

Benchmark practice: Deribit marks every option off its own published volatility surface, constructed by fitting the exchange's own traded option prices; Derive V3 runs a surface engine that updates off book quotes with sanity checks; the general pattern is mark IV from the book where the book is liquid, anchored to a model where it is not, and never fully trusting a single venue's prints. The recommended design in three stages: (1) anchor — keep the configured IV as the anchor mark; (2) blend — compute per-market mark IV by inverting the mid or the weighted bid/ask against the existing implied-vol solver whenever book quotes sit inside a sanity band around the anchor, blending with TWAP smoothing so no single print moves the mark; (3) govern — clamp mark IV moves per interval and fall back to the anchor (with the same missing-mark fail-safe the spot path uses) whenever the book is stale, one-sided, or outside the band. This is deliberately the Deribit shape with the manipulation clamps made explicit rather than implicit. Effort: L — it is the largest single P0, and it should be scheduled with F-1 as one workstream.


### 4.4 G-24 — Persistence: Journal to Disk, WAL, Checkpoints

The engine is in-memory: the journal is a Vec<Event> with no writer, there is no restart path, and a process exit is total loss of state. The design's position — the journal is the persistence seam — is correct as an architecture, and the audit found nothing that violates it, which means persistence is additive rather than structural. The production pattern is standard: an append-only write-ahead log of the journal (fsync per command batch or per interval, per the latency budget), periodic checkpointed snapshots of engine state (serialize the BTreeMaps), and on boot, load the latest checkpoint and replay the journal tail. Every deterministic engine ships exactly this shape, from flood-rs to venue cores; the determinism work in 3.1 is precisely what makes it safe. Effort: M. The acceptance test writes itself: kill the process mid-session (the demo session will do), restart from the WAL, and assert state equality against a continuous run.


### 4.5 G-25 — The API Surface (gRPC / REST / WebSocket, Auth, Rate Limits)

There is no network layer at all: commands are function calls and views are return values. The benchmark set is uniform on shape — REST for account and management operations, WebSocket for market data and order events, gRPC or FIX internally for institutions — and uniformly strict on the disciplines: API-key auth with per-key scopes, per-connection order rate limits, and sequence-numbered channels so clients detect gaps and resynchronize rather than trade on stale books. The engine's read-only views (account_view, book_view, market_state, stats) and typed Rejection enum were clearly designed as this layer's substrate; the audit's only design note is that the event journal is the natural feed — a WebSocket that diffs the journal gives every client exactly the stream the engine itself replays. Effort: L, but decomposable: WS market data first (it makes the venue observable), then order entry, then REST/account.


### 4.6 G-30 — On-Chain Settlement (Batched Commitments with Escape Hatch)

The design stops at the typed boundary between the off-chain engine and settlement, which for a protocol in the Paradigm-adjacent, dYdX-lineage tradition is the right scope decision for v1 but not a permanent identity. The two patterns the market has validated are dYdX v3's off-chain matching with StarkEx batched on-chain settlement (now decommissioned, but the pattern is proven), and the two current extremes: dYdX v4's app-chain (the orderbook itself on a validator-run chain) and Hyperliquid's fully on-chain CLOB. For this system, the audit recommends the hybrid: periodic Merkle commitments of account state (equity, positions, margins) to an L1 or L2 contract, with a forced-withdrawal escape hatch that lets any account exit against the last committed root if the operator halts. The commitments give custody-proof properties without the performance sacrifice of on-chain matching; the escape hatch is what converts trust in the operator into verifiable trust. Effort: L, and correctly sequenced after persistence and API — there is nothing to commit until the journal survives restarts.


### 4.7 G-17 — Multi-Collateral Margin (Single Quote-Currency Cash)

Accounts hold quote-minor cash only; there are no collateral tokens, no weights, no haircuts. The design flags it; the audit confirms the account struct would carry the change well (the SFPM scanner consumes a single equity figure, and equity computation is the seam). Competitor practice is settled: Deribit margins in BTC, ETH, and USDC with per-currency stress factors and auto-collateralization; dYdX v4 mints quote-equivalentUSD with a constant multiplier; the general principle is that non-quote collateral enters margin at a haircut reflecting its liquidation risk — which is exactly the scenario-grid machinery already in the codebase, applied one level up. The recommended shape: a collateral registry (asset, weight, haircut), an equity path that values non-quote balances through the same oracle stack, and stress tests that include collateral-asset moves jointly with portfolio moves. Effort: L — it touches deposit, withdrawal, equity, and every view — and it is P1 because it is a capital-efficiency multiplier, not a solvency prerequisite.


### 4.8 G-19 — ADL Iteration Quality

Auto-deleveraging closes the remainder against the most profitable opposite counterparties at the bankruptcy price, in a single pass, with the projected deficit covered in one calculation. Production ADL (BitMEX's original design and its descendants) is an iterative loop: close a tranche, re-mark, re-check the deficit, repeat until the fund's exposure is covered, and rank counterparties by a published priority score so the closed side is predictable. The single-shot version can overshoot (closing more counterparty profit than the deficit requires) or undershoot when the re-marked deficit grows after the first closure. The fix is contained inside the existing planner pattern: loop the plan-execute-mark cycle to a fixed point with a max-iteration bound, and journal each tranche as its own ADL event so replay stays exact. Effort: M.


### 4.9 G-22 — Exact Time-Indexed Fee-Volume Ledger

The 30-day fee volume slides at an approximation — a fixed-rate decay (about 1/90 per funding interval) rather than a recomputed time-indexed ledger, as the design document states. The approximation drifts from the true sliding window in exactly the ways that matter for tier qualification: a burst of volume ages out too slowly for the venue and slightly too fast or slow for the trader depending on interval boundaries. Every competitor with tiers computes the window exactly (dYdX v4 from a time-bucketed ledger; Hyperliquid likewise). The fix is a dated ledger of volume buckets (per-day or per-interval) with the tier resolved from the trailing sum — a contained change behind FeeSchedule with the existing resolve() semantics preserved. Effort: S.


### 4.10 G-23 — Insurance Fund Inventory Accounting

The insurance fund is a scalar balance: it books the liquidation penalty and absorbs simulated deficits, but it does not carry the positions it acquires as buyer of last resort, does not rebalance them back into the market, and publishes no coverage metrics. The design document labels the inventory virtual. Competitor practice is richer because it has to be: Deribit's insurance fund publishes balance and inflow/outflow history; Bybit publishes a daily report; Hyperliquid's HLP backstop is itself a vault with positions and PnL. The protocol's fund needs three things to be operationally real: position inventory for what it absorbs (so the fund's exposure is visible and its liquidation risk is managed), a rebalancing path (dripping absorbed inventory back to the market on a schedule that does not front-run clients), and a published coverage ratio (fund size against aggregate maintenance requirement) that parameterizes both the liquidation penalty and the 30% revenue share — a thin coverage ratio should raise the penalty before it is needed, not after. Effort: M, and it pairs naturally with G-21 (the cascade governor) since both are stress-dynamics work.


## 5. Part III — Competitor-Mandatory Features Missing from the Protocol

Part II measured the protocol against its own specification. This part measures it against the market: capabilities that the benchmark set ships as standard and that this protocol's design never scoped. They are ordered by competitive weight, and the ordering is not subtle — the first three sections (RFQ, auctions, block trades) are collectively the Paradigm playbook, the institutional liquidity layer that the user's directive explicitly asked this system to learn from, and together they are the single largest gap between the protocol and its stated ambition. Each section follows the Part II discipline: what competitors ship, why the protocol needs it, and the recommended design within the existing architecture.


### 5.1 G-11 — RFQ: Multi-Dealer Competitive Quotes (The Paradigm Franchise)

Paradigm's core product is not a venue at all — it is a negotiation network: a taker (or their institutional client) requests a price for a block in a structure the CLOB cannot quote without moving, multiple dealers respond with firm quotes, the taker executes against the best one, and the resulting trade clears and settles on the underlying venue. The economics are the point: RFQ exists because blocks executed against a public book pay the whole book's immediacy premium, while blocks executed against dealers pay only the dealers' competition for the flow. Every options venue of consequence now runs the hybrid — Deribit's BLOCK, Derive's RFQ rails, Aevo's RFQ — because the hybrid serves two different clients: the CLOB serves continuous price discovery, and RFQ serves size.

This protocol has nothing: all liquidity is CLOB liquidity, and the price-band and max-order limits actively push large flow elsewhere. The recommended design maps cleanly onto the existing engine, because an RFQ is administratively just a deferred, private, pre-negotiated trade — and the engine already settles trades event-wise. The lifecycle: (1) RfqCreated — a taker publishes a request (instrument or structure, size, side, quote window); (2) dealer quotes — firm, two-sided or one-sided, with a quote id, a quantity, a price, and a time-to-live; quotes are private to the requesting account; (3) RfqExecuted — the taker accepts one quote (or a partial), which becomes a Trade event between the two accounts at the quoted price, bypassing the book and its bands by construction; (4) the trade flows through the normal fee, margin, and journal path — an RFQ trade that does not consume margin checks is not a feature, it is a hole. Two disciplines from Paradigm's design should be adopted as non-negotiable: quotes are firm while they live (a dealer who pulls quotes after seeing the request pattern gets dropped from the program — G-15 enforces this), and the RFQ tape is broadcast with a delay (the Transparency pattern, section 5.3) so the public market does not front-run private size. Effort: L — it is a new subsystem (request routing, dealer registry, quote matching) plus a thin integration into the existing trade path. It is the centerpiece of the P1 tier.


### 5.2 G-12 — Auctions: The Time-Preference Mechanism

Paradigm's second product is the auction: for structures where even RFQ is inefficient (large one-sided blocks, complexes, listings with no quotes yet), the seller runs a Dutch-style auction where price descends (or rises) over time and any bidder may lift it — converting the taker's urgency into a price concession bidders compete away. Auctions matter to this protocol for a specific structural reason: the cold-start problem of new option listings is exactly the problem auctions solve, and the liquidity incentive program (Part IV) is spending real budget to solve it badly by comparison. The design maps to the engine as another deferred-trade producer: AuctionCreated (parameters, decay curve, minimum acceptable), bids recorded as they arrive, an execution sweep at the deadline (or on first bid clearing the remaining size in a first-price-Dutch hybrid), and AuctionSettled emitting ordinary Trade events for the clears. Effort: M, and it reuses the RFQ plumbing — request routing, private negotiation, trade emission — which is why it should be scheduled after G-11 and share its subsystem.


### 5.3 G-13 — Block Trade Protocol with Delayed Broadcast

Block trades — privately negotiated, venue-cleared trades above a size threshold — are table stakes on every options venue, and the discipline that makes them safe is the broadcast rule: blocks print to the public tape with a delay (Deribit: after a short window), so the public market does not react to information it cannot have, while still recording the print for price discovery and surveillance. Without a block rail, dealers cannot hedge the RFQ flow (G-11), and the venue's tape systematically under-reports where size actually trades. The design: a BlockTrade command carrying both sides' signatures (or, in this engine's trust model, both accounts' consent events), pre-trade margin checks as usual, and a delayed BlockPrinted market-data event. The audit notes that the engine's price-band and band-bypass questions resolve the same way as RFQ: a block is a negotiated trade, not a book crossing, so bands do not apply and the journal records it exactly like any trade. Effort: M.


### 5.4 G-06 through G-10 — The Institutional Order Set

The orderbook supports limit, market (IOC/FOK semantics included), and the two stop variants with post-only and reduce-only flags — the complete retail set. The institutional set is absent, and each absence has a specific client attached. Iceberg orders (display a slice, rest the remainder hidden) are how size is worked on a public book without advertising it — Deribit ships them, and any dealer working the RFQ hedge wants them. Trailing stops (trigger follows the mark by a fixed offset) are the risk-manager's stop — Deribit and dYdX v4 both ship them, and the engine's existing stop-parking machinery (off-book until the mark crosses) extends naturally: the trigger becomes a function of the running extreme, recomputed on each mark update, journaled as its own event so replay stays exact. One-cancels-other pairs and bracket (position plus protective stops as one unit) are the structured-product desk's basic unit. Batch place/cancel and amend-in-place — change price or quantity of a resting order without losing queue position when the amendment is price-widening — are what makes API market making at all practical; Derive's batch endpoints and Paradigm Fleet's programmatic interfaces exist for exactly this client. Effort: each S; amend-in-place is the one with semantic care (queue-priority rules must be explicit: price-improving amendments lose priority, size increases go to the back of the level). Combined: roughly an M-block of well-understood work against a stable orderbook API.


### 5.5 G-28 — Greeks and Position Analytics APIs

The margin engine computes delta and gamma per leg under the hood (the analytics exist in poc-margin::blackscholes), but nothing exposes them: there is no position Greeks view, no account-level aggregate, no per-market IV or greeks snapshot in the market data. Deribit publishes Greeks on every position and every instrument response; for an options venue this is not analytics garnish, it is the interface through which professional traders see their risk at all — a desk that cannot see its vega cannot quote it. The implementation is a read-only projection over data the engine already holds: per-position Greeks at the mark (delta, gamma, vega, theta, with the everlasting variant's theta expressed as the funding roll), account aggregates, and per-market IV marks (which G-04 produces). Effort: S once G-04 lands, and it is the cheapest credibility item in the register.


### 5.6 G-05 — Volatility Index Publication (DVOL-Shaped)

Deribit's DVOL — a 30-day implied volatility index computed from its own option prices — is the reference rate for the entire volatility complex: it is quoted, hedged, and listed as a future. A protocol whose identity is options-native has an obvious strategic interest in owning its own vol number, and the computation is standard (a variance-swap replication weighting of the two nearest expiries around the 30-day tenor). The audit registers this P2 not because it is unimportant but because it requires the live surface (G-04) and liquid book data to be meaningful — publishing an index off an illiquid surface publishes noise. Effort: M, scheduled after the surface. It is also the natural mark basis for a future vol product line, which is where the differentiation strategy in Part V points.


### 5.7 G-27 — Market Data Infrastructure: L2 Feed, Trade Tape, Index Publication

The engine can produce depth() snapshots and BBO views on demand, but there is no feed: no L2 streaming, no trade tape, no index composition publication, no funding-rate history. Market data is half of what a venue sells — the half that market makers price their participation against before they ever place an order. The design follows the API layer (G-25) and the journal-as-feed observation from 4.5: a WebSocket channel that streams diff-by-diff book updates, the trade tape as the journal's Trade events, funding and settlement events as reference streams, and a periodic full-depth snapshot for resynchronization. Sequence numbers on every message so clients detect gaps deterministically are not optional; they are the difference between a feed and a rumor. Effort: M on top of the API layer.


### 5.8 G-21 — Circuit Breakers Beyond the Oracle Halt

The protocol has one circuit breaker — the oracle quorum halt — and it is the right one for its trigger. It has no answer for the other two halt classes every production venue has. Price-move breakers: dYdX v4 halts a market that moves beyond a band against its oracle in a window; the engine already has price bands for order placement, but no trigger that halts sustained dislocation rather than rejecting individual orders. Liquidation-velocity breakers: BitMEX's legacy and every venue's lesson from cascades is that when liquidations per interval cross a threshold, the venue slows the cascade (longer waits between liquidation rounds, reduced position sizes per round) rather than letting the book gap through itself — the protocol's liquidation sweep has no such governor, and the audit's stress-scenario reading of the cascade code says it needs one before the insurance fund's first real workout. Both are parameterized, journaled, and deterministic — they must be, or replay breaks. Effort: M for the pair, and they belong with G-23 as the stress-dynamics workstream.


### 5.9 G-33 / G-34 — Governance, Parameters, and Auto-Listing

Every risk and economic parameter in the system lives in EngineConfig and changes only by redeploying the engine — an operational model that cannot survive contact with production. The benchmark set is uniform: dYdX v4 routes parameters through on-chain governance with timelocks; Hyperliquid ships an admin multisig with public announcements; the pattern that fits this protocol is an operator multisig with a timelock and journaled ParameterChanged events (determinism demands the journal carry parameter history — replay across a parameter change must reproduce the change, not assume it). The second operational gap is auto-listing: Deribit lists new strikes and expiries on a schedule as spot moves, which is why its surface is always quotable; this protocol's instruments are registered by hand at genesis. The everlasting variant (once G-01 lands) needs the rebased-strike ladder on a schedule (G-03's daily rebasing is the same machinery), and the dated complex needs automatic strike listing around the live index. Effort: M combined, and the journaled-parameter work is a determinism-care item, not a UI item.


### 5.10 G-31 / G-32 — Sub-Accounts, Transfers, and the Withdrawal Pipeline

The engine models sub-accounts correctly (margin pooled per sub-account, the Derive pattern) but has no sub-account management: no transfers between own sub-accounts, no API key scoping to a sub-account, and — operationally binding — no withdrawal pipeline. Withdrawals are a synchronous command that checks free equity and debits; there is no queue, no approval workflow, no custody separation, and no rate limiting. Production practice exists because the alternative is how exchanges die: withdrawal requests queue behind policy (manual review above thresholds, velocity limits, address allow-lists), custody lives on the settlement layer (which G-30 introduces), and the engine's job is only to freeze the requested equity against the balance. Transfers between sub-accounts are margin-neutral internally and are what desks need to segregate strategy risk — the event vocabulary needs one Transfer event and the account layer needs the free-equity check that already exists for withdrawals. Effort: S for transfers, M for the withdrawal pipeline once custody exists.


### 5.11 G-37 — Production Observability and the Dead-Man Switch

There are no metrics, no structured operational logs beyond the journal, no health endpoints, and no dead-man switch. The dead-man switch deserves its own sentence because it is the one whose absence bites hardest: every major venue's API offers cancel-on-disconnect — if a market maker's connection drops, their resting orders are pulled so a stale connection cannot leave unmanaged risk resting on the book. The journal already tracks order ownership and the engine can cancel by owner; the design is a session registry mapping connections to sub-accounts and a sweep that executes the cancels on disconnect, journaled exactly like a CancelAll. Metrics (latency percentiles, book depth, margin coverage, insurance coverage) are the operator's dashboard and the incident timeline; they are an afternoon of wiring against the stats struct that already exists. Effort: M for the full observability story, S for the dead-man switch alone.


### 5.12 G-36 — Referral, Affiliate, and API Economics

Every venue in the benchmark set runs a referral program — fee discounts or rebates shared with referrers — because distribution is a feature competitors ship and this protocol does not scope. It is registered here, at P2, with a design note rather than a design: the fee engine's conserved-router pattern extends naturally (a referrer share is another routed destination with a floor and conservation check), and the audit's only recommendation is that it be implemented inside that pattern rather than as a special case, so the conservation invariant that already has a parameter-sweep test extends to cover it. Effort: S. Its priority is deliberately last in this part: it is growth mechanics, and growth mechanics presuppose the venue mechanics that Parts II and the rest of Part III provide.


## 6. Part IV — Economic Incentive Design: Audit and Completion

The user directive that motivated this whole exercise asked for deliberate attention to economic incentive design, so this part audits what the economics layer does, prices its gaps against the competitor set, and specifies the completion: the full fee and rebate architecture, the maker program that Paradigm-style institutional flow requires, the hardening of the maker score against gaming, and the funding policy for the insurance backstop. The theme throughout is one principle the existing code already embodies and the completion must preserve: every economic rule must be exact (conserved rounding, deterministic replay), budgeted (a named source of funds for every payment), and aligned (the behavior rewarded is the behavior takers consume).


### 6.1 What the Economics Layer Already Gets Right

Three design decisions in the current code are genuinely good economics and survive the audit unchanged. First, the rebate ladder is shaped correctly: maker fees descend faster than taker fees and go negative only at the top tier, and the tier-4 rebate (0.5 bps) is smaller than the lowest taker fee it is paired against (0.9 bps), so the house never pays more in rebate on a trade than the same trade's taker fee can fund — the rebate is a redistribution of taker revenue, not a subsidy the venue mints. Second, the 60/30/10 revenue router makes the insurance fund a shareholder in fee income, which is the correct funding model for a backstop: the fund grows with the very activity that stresses it. Third, the liquidity program pays for quoting (two-sided, proximity-weighted) rather than fills, which structurally removes the wash-trade incentive that volume-based programs carry. The completion below builds on these rather than replacing them.


### 6.2 Fee Benchmark Against the Competitor Set

Table 4 positions the protocol's current schedule against the benchmark venues' published structures. The numbers are indicative and structural — the lesson is in the shapes and the rules, not the fourth decimal — and current published schedules should be re-verified before any production decision. Three structural gaps stand out beyond F-2 (the options premium cap): no RFQ or block fee rails exist at all (Paradigm's franchise runs on near-zero maker economics with small capped taker fees); no market-maker program with obligations exists (Deribit's rebate tiers are earned by two-sided quoting, not volume alone); and there is no referral or distribution share.


**Table 4 — Fee and incentive structures, this protocol vs the benchmark set (indicative, structural)**

| Venue | Options taker | Options maker | Perp taker / maker | Program economics |
|---|---|---|---|---|
| This protocol (current) | 4.5 bps of notional (uncapped — F-2) | 1.0 bps to -0.5 bps of notional | 4.5 / 1.0 bps by volume ladder | Quote-scored LP budget; 60/30/10 router |
| Deribit | min(rate on underlying, 12.5% of premium) | min(rate, 2.5%-ish premium cap); rebates at VIP tiers | ~5 / 0-2.5 bps tiered | MM program tiers earned on quoting; insurance from penalties |
| Paradigm (RFQ network) | Small fixed bps on taker side only | Zero for dealers | n/a (network) | Dealer network economics; fee paid on execution, none on quoting |
| Derive V3 | Capped % of premium, tiered | Rebates at tiers | ~2-5 / 0-1 bps tiered | MM program + volume tiers |
| dYdX v4 | n/a (perps) | n/a | ~1-5 bps / rebates at tiers, exact 30d window | Fee-based LP rewards after token-emission era |
| Hyperliquid | n/a (perps) | n/a | ~2-3.5 bps / 0-1 bps, tiered | HLP vault backstop; buyback from fees |

The read of the table is direct. The current ladder is a competent perp schedule wearing an options costume. What it lacks for options is the premium cap (F-2), and what it lacks for institutions is every rail that carries size: RFQ taker-only fees, block fee caps, program tiers earned by obligations, and a distribution share. Those are specified in the next three sections.


### 6.3 The Completed Fee Architecture (Specification)

The completed architecture keeps the existing volume ladder for the CLOB and adds three rails. CLOB options: fee = min(tier bps on notional, 12.5% of premium) for takers and min(tier bps, 2.5% of premium) for makers, per F-2 — this single rule makes the entire options surface tradeable at wings and strikes alike. CLOB perps: unchanged. RFQ and blocks (once G-11 and G-13 land): taker-only 1 bps of notional capped at the same 12.5%-of-premium for option structures, makers (dealers) pay zero — Paradigm's economics, adopted deliberately, because the dealer network is the product RFQ is buying and dealer economics is the admission price. Every rail routes through the existing conserved RevenueRouter, which gains two destinations: the referral share (default zero until G-36 enables it) and the market-maker program pool (6.4). The invariants extend unchanged: fees ceil, rebates floor, allocations conserve exactly, and the parameter sweep test grows to cover the new rails.


### 6.4 The Market-Maker Program: Obligations Earn Rebates

The volume ladder rewards flow after it happens; a market-maker program rewards the standing commitments that make flow possible. Deribit's MM tiers and Aevo's program are the reference shape: quoted obligations (spread, size, uptime across a defined session) earn a rebate tier or direct payment. The protocol should formalize this as a scored program layered on the machinery that already exists — the incentive scorer already measures exactly the right quantities (size within band, proximity, two-sidedness) on a per-interval basis. The program specification: tier obligations are per underlying and per session (for example: two-sided at or inside X bps of mark, Y% of session uptime, minimum resting size); tier rewards are a multiplier on the base rebate plus a priority allocation of the RFQ dealer role (the real prize — RFQ flow routing is the strongest incentive the venue owns, and it costs nothing to grant); and measurement is time-weighted sampling at randomized intervals rather than on-demand snapshots, which is the gaming defense (6.5).


### 6.5 Hardening the Maker Score Against Gaming (G-39, P0)

The current score — size x proximity x two-sided multiplier, accumulated per interval — has three gaming surfaces the audit identified, each with a standard defense. Quote-and-pull: a market maker can observe the scoring instant approaching, quote, and cancel — the defense is scoring at randomized, unannounced sample times (the score is an estimate of a time-integral; randomized sampling makes it an unbiased one) plus a cancel-to-quote ratio penalty inside the band. Self-matched spread capture: a maker quoting both sides to farm the two-sided multiplier while their quotes never trade is charged nothing for it — the defense is weighting the score partly on quoted-size-at-touch that would actually fill (adverse-selection-aware scoring, the dYdX v4 lesson from its liquidity-provider transition). Uptime grinding: quoting one lot on a dead market all interval to farm uptime — the defense is a minimum notional-at-touch floor for the sample to count. None of these change the score's structure; they change when it samples and what a sample requires, which is why this is an M-effort hardening rather than a redesign.


### 6.6 Insurance Funding Policy and the Buyback (G-40)

The insurance fund currently has a seed, a 30% revenue share, and liquidation penalties flowing in — a sound accumulation model with no target, no policy, and no brake. The completion is a policy, not a mechanism: a coverage target defined as a multiple of the trailing worst-week aggregate maintenance requirement (a fund sized to the stress it must absorb, not an arbitrary number); the liquidation penalty (currently fixed 1.25%) becomes a function of coverage — the penalty steps up when coverage thins, which is the pre-stress adjustment 4.10 called for; and above target, the overflow allocation flips from accumulation to the buyback pool (the Hyperliquid-validated pattern: fee income above the safety line returns to token holders, which aligns the venue's growth with the fund's health rather than against it). The buyback pool itself gains an execution policy — accumulate and burn on a schedule, journaled like every other allocation, so the conservation invariant the router enforces extends to the end of the money's journey.


## 7. Part V — Prioritized Roadmap

The roadmap sequences all 41 register items by the priority framework fixed in 2.3: P0 items threaten correctness or block the protocol's claimed identity; P1 items are competitive parity — capabilities every benchmark venue ships; P2 items are differentiation — capabilities that define the protocol's strategic position but presuppose the parity layer. Figure 2 shows the distribution: the P0 tier is small (9 items) but concentrated in exactly the subsystems the radar showed at zero, and the effort is dominated by four L-class builds (the vol surface, the RFQ layer, the API surface, multi-collateral) that between them carry most of the P1 tier's value.


![Figure 2 — The 41-item gap register by area and priority. The register concentrates where the radar is empty: the options-native mechanics carry the most P0 items because F-1 and F-2 sit there; the liquidity and API areas carry the largest totals because the institutional layer does not exist yet.](docs/assets/fig2_pareto.png)

*Figure 2 — The 41-item gap register by area and priority. The register concentrates where the radar is empty: the options-native mechanics carry the most P0 items because F-1 and F-2 sit there; the liquidity and API areas carry the largest totals because the institutional layer does not exist yet.*

The recommended sequencing is six workstreams in dependency order. WS-1 (close the identity): F-2's fee cap, G-04's surface in its three-stage rollout, and G-01's everlasting wiring as one effort — a perpetual option cannot be funded without a mark, and the fee schedule cannot ship without the cap; this workstream alone converts the venue from dated-only to genuinely perpetual. WS-2 (make it run): G-24 persistence and G-25/G-27 API and market data, with the dead-man switch (G-37 partial) riding the session machinery the API introduces. WS-3 (make it safe): G-39 score hardening, G-21 breakers, G-19 iterative ADL, G-23 insurance inventory and policy — the stress-dynamics work, ideally before the first real liquidity arrives. WS-4 (institutions): G-11 RFQ, then G-13 blocks and G-12 auctions sharing its subsystem, then G-06 through G-09's order set and G-28 Greeks. WS-5 (parity completion): multi-collateral, governance, transfers and withdrawal pipeline, auto-listing, exact volume ledger. WS-6 (differentiation): on-chain settlement with escape hatch, DVOL, LP vaults, FIX, proof-of-reserves, referral. Table 5 states the register with priorities, effort classes, and workstream assignments; it is the operational summary of everything Parts II through IV argued.


**Table 5 — Consolidated gap register (41 items)**

| ID | Gap | Benchmark reference | Pri | Eff | WS |
|---|---|---|---|---|---|
| G-01 | Wire the everlasting roll into the engine (F-1) | Paradigm Everlasting Options; Everstrike | P0 | M | 1 |
| G-02 | Option fee premium cap (F-2) | Deribit 12.5%-of-premium cap | P0 | S | 1 |
| G-04 | Live volatility surface, staged anchor-blend-govern | Deribit surface; Derive surface engine | P0 | L | 1 |
| G-39 | Maker-score gaming hardening (randomized sampling, ratios) | dYdX v4 LP lessons; Blur bid pools | P0 | M | 3 |
| G-24 | Journal-to-disk WAL + checkpoints | Every deterministic engine | P0 | M | 2 |
| G-25 | API gateway: WS/REST/gRPC, auth, rate limits | All venues | P0 | L | 2 |
| G-27 | Market data feed: L2, tape, funding history | All venues | P0 | M | 2 |
| G-32 | Withdrawal pipeline with custody and policy | All venues | P0 | M | 2 |
| G-11 | RFQ multi-dealer quote system | Paradigm ONE; Derive RFQ | P0* | L | 4 |
| G-03 | Strike rebasing / constant-maturity ladder for everlasting | Everstrike daily rebase | P1 | M | 4 |
| G-05 | Volatility index publication (DVOL-shaped) | Deribit DVOL | P2 | M | 6 |
| G-06 | Iceberg / hidden orders | Deribit iceberg | P1 | S | 4 |
| G-07 | Trailing stops | Deribit; dYdX v4 | P1 | S | 4 |
| G-08 | OCO / bracket orders | CEX standard | P2 | S | 4 |
| G-09 | Batch place/cancel + amend-in-place | Derive batch; Paradigm Fleet | P1 | S | 4 |
| G-10 | TWAP / execution algorithms | Venue algo suites | P2 | M | 6 |
| G-12 | Auction mechanism (Dutch / time-preference) | Paradigm Auction | P1 | M | 4 |
| G-13 | Block trades with delayed broadcast | Deribit BLOCK; Paradigm Transparency | P1 | M | 4 |
| G-14 | Quote streaming + last-look policy | Paradigm streaming quotes | P1 | S | 4 |
| G-15 | MM tier program with obligations | Deribit / Aevo MM tiers | P1 | M | 4 |
| G-16 | LP vaults / underwriter pools | Lyra v2 wrapped tokens | P2 | L | 6 |
| G-17 | Multi-collateral margin with haircuts | Deribit multi-currency; dYdX v4 | P1 | L | 5 |
| G-18 | Interest on collateral | BitMEX; Deribit | P2 | S | 5 |
| G-19 | Iterative ADL with priority score | BitMEX ADL | P1 | M | 3 |
| G-20 | Spot hedging for short options (partial collateral) | Derive V3 | P2 | M | 6 |
| G-21 | Price-move + liquidation-velocity breakers | dYdX v4 halt bands; BitMEX | P1 | M | 3 |
| G-22 | Exact time-indexed fee-volume ledger | dYdX v4 exact windows | P1 | S | 5 |
| G-23 | Insurance inventory, coverage policy, reporting | Deribit / Bybit fund reporting | P1 | M | 3 |
| G-26 | FIX API for institutions | Deribit FIX; CME | P2 | M | 6 |
| G-28 | Greeks and position analytics APIs | Deribit greeks everywhere | P1 | S | 4 |
| G-29 | Backtest/replay tooling + proof-of-solvency | Hyperliquid PoR culture | P2 | M | 6 |
| G-30 | On-chain settlement: batched commitments + escape hatch | dYdX v3/v4; Hyperliquid | P1 | L | 6 |
| G-31 | Sub-account transfers and key scoping | Deribit / Derive sub-accounts | P1 | S | 5 |
| G-33 | Parameter governance: timelock, multisig, journaled changes | dYdX governance; Aave pattern | P1 | M | 5 |
| G-34 | Auto-listing of strikes / expiries around spot | Deribit auto strike grid | P1 | S | 5 |
| G-35 | Listing / delisting lifecycle policy | All venues | P2 | S | 5 |
| G-36 | Referral / affiliate share inside the router | dYdX / Hyperliquid referrals | P2 | S | 6 |
| G-37 | Observability + dead-man switch (cancel-on-disconnect) | All venues | P1 | M | 2 |
| G-38 | RFQ / block fee rails (taker-only, capped) | Paradigm economics | P1 | S | 4 |
| G-40 | Insurance funding targets + buyback execution policy | Hyperliquid buyback; Deribit fund | P1 | S | 3 |
| G-41 | Portfolio Greeks limits (vega / gamma caps per account) | Deribit PM limits | P1 | M | 5 |

*P0* on G-11 reflects the audit's judgment that the RFQ layer is the largest competitive gap and gates the institutional order set; its dependency on WS-2 (an API to receive requests through) makes it sequenced fourth in practice. Effort: S under one week, M one to four weeks, L over a month, for one experienced Rust engineer on this codebase.*


## 8. Part VI — Risk Register and Final Verdict

The audit closes with the risks that remain after the register is worked — the ones that no single line item retires, because they are properties of the design's ambition rather than its gaps. Table 6 states the top eight with their mitigations; each is live now, not hypothetical, and the mitigations are drawn from how the benchmark venues have handled the same exposures.


**Table 6 — Top risk register**

| Risk | What it threatens | Mitigation path |
|---|---|---|
| Vol-surface manipulation | The everlasting mark and funding basis; scenario margin credibility | Governed surface (G-04 stage 3): move clamps, staleness fallback, deviation quarantine on surface inputs |
| Oracle dependence | Every mark, funding, liquidation, and settlement | Existing four-layer defense plus G-27 index publication and secondary confirmation sources at scale |
| Liquidation cascade dynamics | Insurance fund solvency under correlated stress | G-21 velocity breaker, G-19 iterative ADL, G-23 coverage policy — the WS-3 workstream, before real liquidity |
| Cold-start liquidity failure | Venue viability; the incentive budget spent without depth | G-15 MM program with obligations, RFQ dealer seeding (G-11), auction listings (G-12) — rent depth until the flywheel turns |
| Incentive gaming | Reward budget leakage to quote-and-pull and wash patterns | G-39 randomized sampling, ratio penalties, adverse-selection-aware scoring |
| Regulatory classification | The everlasting instrument itself; RFQ dealer network | Jurisdiction analysis before launch; the instrument is a funding-settled derivative — counsel, not code, retires this |
| Key/operational compromise | Engine and treasury | Multisig governance (G-33), custody separation (G-30/G-32), dead-man switch, journaled parameter history |
| Competitor response | The differentiation window | The window is the everlasting complex itself: incumbents' CLOBs are structurally dated-options; speed through WS-1 and WS-4 |

The final verdict. This codebase is a credible engine core wearing an incomplete venue: its solvency-critical systems — determinism, numerics, portfolio margin, oracle defense, the liquidation cascade — are built to a standard the audit would sign, and its economics layer already encodes the three soundest ideas in the codebase (the rebate ladder funded by taker revenue, the insurance fund as a fee shareholder, the quote-scored liquidity budget). What it is not, yet, is what its name promises: the perpetual-options mechanism is unwired, the fee schedule punishes the wings, and the entire institutional layer — RFQ, auctions, blocks, the order set, the data — does not exist. None of that is a flaw in what was built; it is the honest shape of what was deliberately built first. The register in Table 5 is the bridge from the engine to the venue, and the two findings at its head are small enough to close and large enough that closing them changes what the system is.


## Appendix A — How the Register Was Constructed

The 41 register items were derived from three sources, cross-checked against each other: the design document's own six known simplifications (all confirmed, all placed); the source-level audit of Part I, which surfaced every place the code falls short of its own claims (F-1 and F-2 foremost); and a systematic pass over the benchmark set's shipped feature inventory, which generated the Part III list. Each item carries a benchmark reference (so no gap is asserted without a venue that ships it), a priority per the 2.3 framework, an effort class per 2.4's scale, and a workstream assignment per Part V. The register is intended to be worked as a backlog: every row is small enough to be a specification, and Table 5's ordering is the audit's recommendation for the order in which they should be pulled.


## Appendix B — Feature Matrix vs the Benchmark Set

Table 7 is the competitive snapshot in one view: the protocol against the benchmark venues on the dimensions this report scored. It is read column-wise (where the protocol stands) and row-wise (which venue to copy for a given capability). The pattern the audit has argued throughout is visible here in miniature: full marks on the engine dimensions, near-zero on everything operational, and the one cell no competitor can claim yet — the everlasting roll — still open on this protocol's own board until G-01 closes it.


**Table 7 — Feature matrix: this protocol vs benchmark venues**

| Capability | This protocol | Deribit | Paradigm | Derive V3 | dYdX v4 | Hyperliquid |
|---|---|---|---|---|---|---|
| CLOB matching + determinism | Yes | Yes | n/a | Yes | Yes | Yes |
| Portfolio margin (scenario) | Yes | Yes | n/a | Yes | Partial | No |
| Dated options complex | Yes | Yes | Flow | Yes | No | No |
| Everlasting / perpetual options | Formula only (G-01) | No | Paper | No | No | No |
| Option fee premium cap | No (G-02) | Yes | n/a | Yes | n/a | n/a |
| Vol surface + vol index | Config IV (G-04/G-05) | Yes (DVOL) | Consumer | Yes | No | No |
| RFQ / dealer network | No (G-11) | Partial | Yes | Yes | No | No |
| Auctions | No (G-12) | No | Yes | No | No | No |
| Block trades | No (G-13) | Yes | Yes | Yes | No | Yes |
| Iceberg / trailing / OCO | No (G-06..08) | Yes | n/a | Partial | Partial | Partial |
| Greeks APIs | No (G-28) | Yes | n/a | Yes | n/a | n/a |
| Multi-collateral | No (G-17) | Yes | n/a | Partial | Partial | Yes |
| Insurance fund + ADL | Yes | Yes | n/a | Yes | Yes | HLP |
| Maker rebates | Yes | Yes | Dealer econ | Yes | Yes | Yes |
| API + market data feed | No (G-25/27) | Yes | Yes | Yes | Yes | Yes |
| On-chain settlement / PoR | No (G-30) | No (CEX) | n/a | Partial | Yes | Yes |
| Governance / parameter admin | No (G-33) | Yes (ops) | n/a | Yes | Yes | Yes |
| Auto strike/expiry listing | No (G-34) | Yes | n/a | Partial | n/a | n/a |

*n/a marks capabilities outside a venue's model (Paradigm is a network, not a venue; perp-only venues have no options rows). The matrix is structural, not exhaustive, and reflects the audit-date state of the benchmark set.*

## Appendix C — Closure status (post-audit waves)

The register above is the *audit-time* record. Three implementation
waves have since closed it; this appendix tracks what shipped, so the
table above stays a historical document rather than a stale claim.

| Wave | Closed | Where |
|---|---|---|
| 1 (P0/P1 core) | F-1/G-01 everlasting roll wiring, G-04 governed vol surface, F-2 option fee caps, G-11/13/14/38 RFQ + blocks + streaming + MMP, G-21 breakers, G-22 volume ledger, G-31 transfers, G-28 Greeks view, G-40 coverage policy | engine + volsurface + rfq crates |
| 2 (operations) | G-24 WAL + checkpoints, G-25/27/32 gateway protocol + market data + withdrawal pipeline, G-30 state-diff settlement with merkle proofs and exit queue, G-33 governance, G-19 iterative ADL, G-41 Greeks caps, G-36 referrals | persist + api + settlement + governance crates |
| 3 (parity + differentiation) | G-03 strike-rebase ladder, G-05 DVOL-shaped vol index, G-06 icebergs, G-07 trailing stops, **G-08 OCO brackets**, G-09 batch + amend (with the shared-id/double-fill fix, I-26), **G-10 TWAP parents**, G-12 Dutch auctions, G-16 LP underwriter vaults, G-17 multi-collateral, **G-18 collateral interest**, G-20 spot-hedge-aware margin, G-23 insurance inventory + marking + rebalancing, **G-26 FIX session codec**, G-34 auto-listing, G-39 randomized sampling | engine (algorithms, vaults, sweep) + economics (vaults) + api (fix) |

**Still open, by design:** G-15 (MM tier *program* — the obligations
side; MMP protection itself ships), G-16's revenue-share wiring to the
insurance allocation (vault mechanics ship; the routing weight defaults
to zero), G-26's transport layer (TCP/sequence-reset — the codec and
typed subset ship), G-35 (proof-of-reserves tooling), G-18 interest on
*quote* balances (never charged — only foreign-currency utilization
pays). Each remaining row is a deployment concern riding on shipped
machinery, not a missing subsystem.

The competitive matrix (Table 7) as of wave 3: every row that said
"No (G-xx)" above is now "Yes" in the code, with the everlasting roll —
the cell no incumbent claims — live end to end.
