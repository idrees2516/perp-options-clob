# Improvement Register — the six-axis assessment

This document answers the architectural review that mapped proposed
fixes to the code along four benefit axes (efficiency, security,
scalability, simplicity). Each suggestion is assessed against the
codebase as it stands, with one of three verdicts: **DONE** (implemented
in this drop, with the seam it changed), **PARTLY DONE** (the safe
core landed; the rest is scoped below), or **ROADMAP** (deliberately
not done now, with the design and the reason).

---

## 1. Efficiency — the clone-everything planning tax

### 1a. Pre-trade margin gate: clone per order — ROADMAP (design fixed)

The `poc_risk::check_order` clone is the price of the plan/apply
discipline: a pure planner answers "would this breach margin?" by
simulating on a clone. The suggested fix (cache the account's margin
summary keyed on content) landed for the *liquidation scan* (see 2a);
the per-order gate keeps its clone because the hypothetical there is
order-specific (the order's own limit price enters the simulation),
so a content cache does not apply. What WOULD apply: an incremental
scenario scan that reprices only the legs the order touches. That is
a margin-engine refactor (incremental SPAM/SPAN), not a cache —
scoped, not started.

### 1b. `commit` clones every event into the journal — DONE

`commit` clones the planned batch so it can both journal and return
it. Added **`Engine::process_quiet`**: the hot sequencer/WAL feed path
that journals by *moving* the batch (zero clones, roughly halving
event allocations per command). `process` keeps its contract for
callers that need the events back; the demo and tests use it, the
feed path uses `process_quiet`. Verified state-identical by
`process_quiet_matches_process_state`.

### 1c/1d. Vault epochs and MM review run their simulations twice — ROADMAP

`plan_vault_epochs`/`plan_mm_review` clone, simulate, journal the
outcome, then apply re-runs the identical computation. The
suggested fix (journal the computed numbers and apply them
directly, trusting the planner) trades simulation cost for journal
size and moves correctness weight onto the plan/apply agreement.
Not taken now: the double-run is the *proof* that apply matches
plan. If profiling ever shows it hot, the fix is mechanical.

---

## 2. Efficiency/Scalability — the O(accounts) margin scan per tick

### 2a/2b/2c/2d. The 18-scenario grid for every account, every tick — DONE

`plan_liquidations` ran the full SFPM scenario grid for **every**
account on **every** tick. Added the **content-addressed margin
cache** (`Engine::effective_margin_summary_cached`):

* key = (cash, order margin, positions (symbol, lots, entry), a
  FNV-1a digest of the whole mark set, the governance config
  version);
* a hit returns *exactly* what a recompute would — stale entries
  simply never hit, so there is no invalidation logic and no replay
  hazard (the cache is planning-only `RefCell` state, never
  journaled; replay rebuilds nothing because replay never plans);
* accounts holding non-quote collateral bypass the cache (collateral
  valuation is time-windowed and would need a time key) — the
  no-collateral accounts are the dominant population;
* candidacy cost drops from O(all accounts × positions × 18) to
  O(at-risk accounts + key hashing).

The per-candidate `sim` clone (2d) is untouched: it is the rare path
it was designed for. `margin_cache_matches_recompute` pins the
hit-equals-recompute property.

---

## 3. Security — authenticating commands and normalizing venue money

### 3a/3b. Unauthenticated commands — ROADMAP (design fixed)

The engine is a deterministic library; auth belongs to the gateway
(`poc-api` keys/HMAC/nonces/rate limits, `poc-fix` sessions). The
review's fix — commands become self-authenticating envelopes
(`{actor, sig, payload}`) — is right for the sequencer boundary and
reuses `poc-api`'s `Signer`; what it needs beyond that is an
engine-side **actor key registry** with journaled
`ActorKeyInstalled` events (so replay verifies rather than trusts)
and a nonce ladder for replay protection. Scoped, not started: it
changes the command surface every caller shares.

### 3c. The insurance fund's magic subaccount — PARTLY DONE (documented + funnel)

`INSURANCE_SUBACCOUNT = u64::MAX - 1` remains the wire identity (a
typed enum on every fill would churn the persist codec's tags), but
every exemption now funnels through the single `fee_exempt` check in
the fee helper, and the reserved-id convention is documented here as
the venue-account contract: **u64::MAX-1 is reserved for the
insurance fund; ids ≥ u64::MAX-1 never belong to users.** The
settlement diff already carries the fund as a first-class pool.

### 3d. House revenue off-ledger — DONE (already first-class; now documented)

The review's claim was half-stale: the house share is tracked by the
`RevenueRouter`'s cumulative allocation and exposed through
`venue_pools()`; `poc-settlement`'s diff includes it as
`HOUSE_SUBACCOUNT` so conservation audits account for it. What was
missing is the *documentation of the contract*, now stated in
`docs/OPERATIONS.md` terms: money lives in exactly five places —
customer accounts, the insurance fund, LP vault collateral, the
buyback pool, and the house revenue counter — plus the deliberately
synthetic insurance inventory; every new money path must declare
itself to `venue_pools()` or the settlement diff fails.

---

## 4. Security — hardening the f64 risk boundary

### 4a–4d. Marks, margin grids, greeks caps, collateral legs — ROADMAP (design fixed, deliberately)

The boundary itself is correct and Deribit/Derive-shaped: f64 for
risk figures, one rounding helper back to integer money. The
suggested hardening — ceil the requirement, epsilon-band the caps,
integer-scale the collateral exposures — converts cross-platform
libm disagreement (last-ulp differences in `exp`/`erf`) into bounded
conservatism. It is **deliberately not flipped on yet** because it
moves every liquidation trigger and order-gate boundary at once;
existing journals replay fine (planning-only change) but every
risk-figure test expectation shifts. The staged plan: a
`RequirementRounding { HalfUp, Ceil }` config knob (governed, like
the other parameters), Ceil validated against the full suite, then
flipped by governance vote. The epsilon band for greeks caps is the
same one-line direction change with the same blast radius.

---

## 5. Scalability — journal, snapshots, staged ticks

### 5a. Unbounded in-RAM journal — PARTLY DONE

`process_quiet` removes the per-event *allocation* clone, but the
journal `Vec` still grows with command count. The complete fix —
checkpoint-trimmable journal bound to the WAL's disk segments — is a
`poc-persist` integration: the engine needs a
`truncate_journal_to(watermark)` that the host calls after a
checkpoint manifest commits, plus a `state_fingerprint()` continuity
check. Scoped; the WAL side (framed, CRC'd, chain-hashed segments +
atomic manifests that prune) already exists.

### 5b. Replay is full-history — ROADMAP

`Engine::replay(config, journal)` rebuilds by applying the whole
journal; checkpoints bound it operationally (measured ~7.7 M
commands/s). Engine-side snapshot/restore needs full `serde` on the
state graph or an event-derived snapshot format; that is a format
decision (snapshot versioning belongs in the settlement batch
format), not a quick win.

### 5c/5d. The stale-snapshot tick and staged plan-apply — ROADMAP

The tick plans ~15 stages against one pre-tick snapshot; ordering is
the only coordination, and the same single-pass model produced the
I-26 batch bug that forced sequential commit for batches/OCO/TWAP.
The suggested generalization — staged plan-apply for the tick (plan
stage 1, apply, plan stage 2 against post-stage-1 state, ...) — is
the right shape and reuses the existing sequential-commit machinery,
but it changes tick semantics subtly (stages would see earlier
stages' effects), so it needs its own invariant pass. Scoped, not
started.

---

## 6. Simplicity — the god-object and the governance disconnect

### 6a/6b. The ~40-field Engine and the 800-line matches — ROADMAP (map exists)

The module layout already draws the boundaries (`sweep`, `exercise`,
`vaults`, `mm`, `collateral`, `auction`, `amend`, `combo`, `router`
own their planners). The decomposition — subsystem state structs +
per-family apply modules behind a thin `apply_event` dispatcher — is
mechanical and preserves the single-mutator invariant (I-3), but it
touches every arm of the apply match at once. It should be its own
PR series (one subsystem per PR, replay-fingerprinted), not a
drive-by.

### 6c/6d. The governance disconnect — DONE

`poc-governance` could run a full proposal lifecycle but nothing
consumed its output: `EngineConfig` was set at construction and never
mutated, so replaying across a mid-history parameter change would
silently diverge — while `docs/OPERATIONS.md` claimed parameter
history was auditable by replay. Now wired end to end:

* `Command::GovernanceApply { key, value }` — submitted by the host
  after `Governor::execute` succeeds;
* `Event::ParameterChanged` — journaled, applied through the single
  config-mutation path (`apply_governed_parameter`, a whitelist of
  tunable risk/economic keys: combo config, dealer enforcement,
  quote interest, ADL rounds, vol-index band, insurance rebalancing);
* `config_version` — bumped on every application, part of the
  margin-cache key, so governed risk changes invalidate cached
  summaries;
* replay applies the identical mutation, so **parameter history is
  now actually auditable by replay**
  (`replay_carries_parameter_history`).

Codec tag 38 carries the command through the WAL.

---

## Summary table

| # | Suggestion | Verdict | Seam |
|---|------------|---------|------|
| 1a | pre-trade clone | ROADMAP (incremental margin engine) | `poc_risk::check_order` |
| 1b | journal clone | **DONE** — `process_quiet` | `engine.rs` commit path |
| 1c/1d | double simulations | ROADMAP (journal computed outcomes) | `vaults.rs`, `mm.rs` |
| 2a–2d | O(accounts) margin scan | **DONE** — content-addressed cache | `sweep.rs` candidacy |
| 3a/3b | command auth | ROADMAP (actor-key registry + envelopes) | command surface |
| 3c | magic insurance id | PARTLY — single funnel + reserved-id contract | fee helpers |
| 3d | house off-ledger | **DONE** — already first-class; documented | `venue_pools`, settlement diff |
| 4a–4d | f64 hardening | ROADMAP (governed `Ceil` knob, staged rollout) | margin grid, greeks caps |
| 5a | unbounded journal | PARTLY — clone gone; trimming scoped | journal ↔ WAL watermark |
| 5b | full-history replay | ROADMAP (snapshot format decision) | `Engine::replay` |
| 5c/5d | staged ticks | ROADMAP (own invariant pass) | `sweep.rs::plan_tick` |
| 6a/6b | god-object split | ROADMAP (one subsystem per PR) | module boundaries exist |
| 6c/6d | governance wiring | **DONE** — `GovernanceApply` → `ParameterChanged` | config mutation path |
