# Invariants

The engine's contract with itself. Every invariant has an id, a precise
statement, an enforcement point, and the test that would catch a
violation. Fuzzing (`docs/FUZZING.md`) re-checks the dynamic ones under
random input; the static ones are enforced by types and constructors.

## Ledger & settlement

### I-1 Money is integer-exact
All quote amounts are `i128` minor units; every division goes through
`poc_core::mul_div` with an explicit `Rounding`.
*Enforcement:* `poc-core::num`; no `/` operator on money paths.
*Tests:* `poc-core::num` rounding tables; conservation tests below.

### I-2 No silent overflow
All money arithmetic is `checked_*`/`saturating_*`; overflow clamps
toward the venue (never mints).
*Enforcement:* clippy `unwrap_used`/`expect_used` banned in lib code.
*Tests:* economics overflow tests (fees, funding, revenue).

### I-3 Replay determinism (bit-exact)
Two engines fed the same command sequence (or journal) hold identical
state. This is the property that makes off-chain matching with on-chain
settlement auditable.
*Enforcement:* single mutator `Engine::apply_event`; plan/apply
discipline; `ClockAdvanced` journaling.
*Tests:* `determinism_and_replay`, `new_features_replay_bit_for_bit`,
`properties_hold_across_random_sequences` (12 random sequences ×
140 commands), stress `crash-recovery determinism`.

### I-4 Settlement conservation (venue-wide)
`Σ user cash + venue pools` moves **only** by declared custodial flow
(deposits − withdrawals ± reward emissions). Fees route to pools;
funding, trades, liquidations, and ADL are zero-sum inside the tracked
universe.
*Enforcement:* `poc-settlement` validator
(`validate_batch` → `UnexplainedFlow`); `venue_pools()` exposes every
sink (insurance, rewards, house, buyback).
*Tests:* `trades_fee_and_fund_flows_conserve`,
`unexplained_flow_is_rejected`, `full_pipeline_batches_chain_and_validate`.
**Found & fixed by this invariant:** the routed buyback fee share was
debited but never credited to any pool (a genuine conservation leak,
fixed in all three fee-routing sites).

### I-5 Entry-anchored equity conservation
Under the premium-unpaid, entry-anchored realization model the conserved
quantity is `Σ cash − Σ(signed_lots × entry_per_lot) + pools` — exact up
to per-lot rounding dust (integer VWAP entry averages and the single
per-lot price conversion truncate; empirically < 5 minor per traded lot,
bounded in tests at 10×lots + 16).
*Tests:* `properties_hold_across_random_sequences`.
*Follow-up:* per-lot cost-basis tracking would make it exact.

### I-6 State diffs cannot lie
A settlement batch's mutation list applied to the pre-state must
reproduce the committed root, and batch headers hash-chain
(`prev_root` linkage). No silent forks.
*Enforcement:* `poc-settlement::validate_chain` / `validate_batch`.
*Tests:* `full_pipeline_batches_chain_and_validate` (including a
forged-header rejection).

## Matching

### I-7 No crossed book at rest
Best bid < best ask whenever both exist; orders only rest on the
correct side of their own price.
*Tests:* property suite after every random command; spam stress.

### I-8 Price-time priority, no partial-self-matching
Fills respect the maker's limit; takers never pay worse than their
limit. STP policies (cancel-newest/oldest/both, decrement) resolve
same-subaccount crossings before fills.
*Tests:* orderbook unit tests, engine matching tests.

### I-9 Fees are capped by option premium
Option fees = `min(bps × notional, cap × premium)`; the taker cap is
12.5% of premium, the maker cap 2.5% (Deribit/Derive rule F-2).
*Tests:* economics fee-cap tests, engine F-2 integration test.

## Risk

### I-10 Pre-trade margin gate
An order that would leave the hypothetical post-fill account below
initial margin (worst-case fill at its own limit + worst-case fee) is
rejected before any state change.
*Tests:* pretrade suite (bands, post-only, reduce-only, position limits,
hypothetical-fill gate).

### I-11 Liquidations are deficit-ordered and legal
Only equity-below-maintenance accounts enter the cascade; partial
closures precede full; the insurance fund absorbs only real deficits;
ADL closes only the opposite side of the bankrupt position, in capped
rounds.
*Enforcement:* sweep candidates + planner; counterparty sign filter.
*Tests:* liquidation suite, `bankrupt_account_liquidates_collateral_then_adls_in_rounds`
(**found & fixed by this test**: the ADL counterparty filter was
inverted, and the round-based iteration was one-shot).

### I-12 Greeks caps (G-41)
Portfolio vega/gamma limits reject option orders that would breach the
configured caps pre-trade.
*Tests:* `vega_cap_rejects_option_order`, greeks-limit unit tests.

## Oracles

### I-13 Quorum or nothing
The mark exists only with `min_providers` fresh providers in one
coherence cluster; otherwise the underlying halts (fail-safe).
*Tests:* oracle unit tests, `oracle_genuine_jumps_always_accepted`
property (8 seeds × 60 coordinated jumps).

### I-14 A rogue feed cannot drag the mark; a genuine jump must not be suppressed
Cluster consensus: the authoritative cluster is (1) the single coherent
cluster, else (2) the quorate cluster nearest the last mark, else (3)
the largest quorate cluster. A lone rogue print is soft-quarantined;
coordinated genuine jumps are accepted.
**Found & fixed by this invariant:** the old deviation check anchored
to the stale last mark and quarantined every honest provider on a
genuine +15% market move (a liveness bug).
*Tests:* `genuine_jump_accepted_when_all_providers_move`,
`split_cluster_keeps_continuity_with_quorate_old_level`,
`two_provider_disagreement_loses_quorum_fail_safe`,
`rogue_provider_is_quarantined` (engine).

## Persistence

### I-15 Torn tails recover; mid-log corruption aborts
A crash mid-write loses at most the final frame (prefix replay,
reported); corruption in the middle of a segment refuses recovery
rather than silently skipping.
*Tests:* `torn_tail_is_dropped_cleanly` (truncation at **every** byte
offset), `mid_wal_bitrot_is_refused`.

### I-16 The WAL chain cannot be spliced
Frames carry an FNV chain hash over `(prev, payload)` + CRC-32;
reordering, splicing, or truncating history breaks resumption.
*Tests:* `resume_continues_chain_across_reopen`, WAL unit tests.

## Market data & API

### I-17 Snapshot-delta continuity
A session applies deltas only in strict sequence; a gap desyncs the
book (never silently continues) and requires an explicit resync.
*Tests:* session suite (gap detection, dropped-delta accounting).

### I-18 Nonces are monotonic per API key
Replayed or out-of-order authenticated requests are rejected.
*Tests:* auth suite (replay, disabled keys, bad signatures, rate limits).

## Governance

### I-19 Timelock cannot be skipped
Parameter changes execute only after the timelock elapses, inside the
grace window, with threshold-weighted approvals; the guardian can veto
anything instantly; executed proposals are immutable.
*Tests:* governance lifecycle suite.

## Withdrawals

### I-20 Withdrawals are never instant
Every withdrawal waits out the settlement delay; large ones require
manual approval; pending ones are cancellable (refunding).
*Tests:* withdrawal pipeline suite.

## Execution algorithms (second closure wave)

### I-21 An OCO group has at most one live member
From the moment a bracket is linked (`OcoLinked`), the first member to
reach *any* terminal state — filled completely, triggered, canceled,
expired, or IOC remainder — cancels the other with the dedicated
`OcoSibling` reason. The cascade is planned after every command batch as
a pure function over (pre-apply state, planned events), so replay
reproduces it exactly, and the group is released from the registry when
either member closes.
*Tests:* `oco_take_profit_fill_cancels_stop_loss`,
`oco_stop_trigger_cancels_take_profit`,
`second_wave_commands_hold_invariants` (fuzzed over random OCO mixes —
asserts at most one member open per group after every command).

### I-22 A TWAP parent never overslices
The slicer's integer split guarantees `Σ slice_lots == total_lots`
exactly (the first `total % slices` children carry one extra lot), each
tick emits at most one child per parent, and the parent closes only
after the last slice. Children are full order requests journaled inside
`TwapSliced`, so replay never re-derives the arithmetic.
*Tests:* `twap_slices_complete_and_sum_to_total`,
`twap_cancel_stops_slicing`, and the fuzzed slice-bound check in
`second_wave_commands_hold_invariants`.

### I-23 Vault epoch flows conserve exactly
At every epoch boundary, the per-subscriber signed flows inside
`VaultEpochSettled` sum to `redeemed − subscribed` exactly; subscription
shares are floored (dust never mints), redemptions are floored against
NAV (the vault never overpays), and cash moves only at boundaries —
never mid-epoch.
*Tests:* `vault_epoch_subscribes_redeems_and_pays`, vault unit suite,
fuzzed `second_wave_commands_hold_invariants` (flow-sum check per epoch
plus the global conserved-quantity identity extended with vault
collateral and insurance inventory).

## Backstop & indexing

### I-24 Insurance inventory is marked, and unwinds only into real liquidity
Every position the fund absorbs as buyer of last resort is booked into
`insurance_inventory` at the penalized execution price, marked to the
current marks every sweep (PnL landing in the fund balance), and dripped
back into the book only when a resting bid pays the configured edge over
the carrying mark — crossing the book as a synthetic IOC taker whose
fills are ordinary journaled trades (makers settle; nothing prints
against phantom liquidity). The fund pays no taker fee on its own
unwinds: routing a fee nobody paid into the pools would mint quote.
*Tests:* `insurance_books_marks_and_rebalances_inventory`
(booking, marking, and the drip), liquidation suite.

### I-25 The volatility index is bounded by its inputs
The published index (G-05) is an integer-arithmetic
moneyness-weighted variance of the governed mark IVs inside the
configured band — it can never exceed the max squared input IV and is
computed without floating point in the publication path (integer
`isqrt`), so it is byte-identical on every platform.
*Tests:* `vol_index_publishes_near_the_mark_ivs` (band assertions),
engine-level DVOL sanity in the demo run.

## Batch atomicity (G-09 hardening)

### I-26 A batch is atomic on validity, sequential in effect
Every member of a `PlaceBatch`/`PlaceOco` is validated against the
pre-command snapshot with a cumulative margin simulation; one failure
rejects the whole command. After the gate passes, members commit
sequentially: order ids advance and later members see earlier fills —
no shared-id collisions, no phantom double-fills of the same maker. (The
original single-pass planner violated both; the fuzz suite now pins
distinct ids and maker-size-limited fills.)
*Tests:* `batch_siblings_get_distinct_ids_and_see_each_other`,
`batch_places_and_amend_reduces_in_place`, fuzzed batches in
`second_wave_commands_hold_invariants`.

## Fourth closure wave (G-15/16/18/26/35)

### I-27 An MM tier discount exists only for an enrolled maker, and only as earned
The discount map is written exclusively by journaled `MmTierAdjusted`
events; a discount requires prior enrollment, is re-derived at every
review from the drained measurement window (never carried by trust),
never exceeds the program's configured cap (50%), and never flips a
fee into a rebate. Demotion on silence is automatic — there is no
grandfathering path in the state machine.
*Tests:* `mm_tier_discount_requires_earned_obligations` (award, fee
discrimination, demotion, replay), `mm_tiers` unit suite (ladder
validity, evaluation, bounded discount math), fuzzed tier legality in
`third_wave_commands_hold_invariants` (enrollment ⊆ discount, cap).

### I-28 The vault revenue split never exceeds the insurance allocation
Every routed fee's insurance share splits through the LP vaults first:
each vault takes `floor(allocation × revenue_share_bps / 10_000)`,
granted in ascending vault-id order and capped by the remaining
allocation, so `Σ vault credits + fund credit == insurance share`
exactly — under any configuration of shares, including multiple vaults
each claiming 100%.
*Tests:* `vault_revenue_share_routes_and_conserves` (exact split +
global conservation), `vault_share_never_exceeds_the_allocation`
(over-subscribed shares), fuzzed routing inside
`third_wave_commands_hold_invariants`.

### I-29 A FIX session delivers each application message exactly once, or explains the gap
Inbound sequence numbers must arrive exactly at the expected value;
ahead-of-sequence messages trigger a ResendRequest and buffer until the
gap fills; PossDup-tagged replays below the sequence are dropped
silently; untagged regressions are answered with a GapFill resync. On
resend, application messages replay with their original sequence and
`43=Y`, while administrative messages are covered by GapFill — the
recovered stream contains state changes, never heartbeats.
*Tests:* the `fix_session` suite (gap + buffered delivery, possdup
drop, stale reset, admin-run gapfill on resend, sequence-reset jump,
TCP loopback lifecycle).

### I-30 Every provable liability is committed, and only to its owner
The proof-of-reserves tree binds (subaccount, nonce, cash, collateral
valuations, vault claims) into one leaf per account, sorted by
subaccount, under a domain-separated hash; the total committed equals
the sum of the leaves; any balance change (or a nonce change alone)
changes the root; and a redemption can only burn shares its redeemer
holds — the claim ledger is exactly the liability set.
*Tests:* `por_proves_every_liability_from_engine_state` (proofs,
tamper resistance, totals, monotonic publication), `por` unit suite
(duplicates, nonce binding, ledger monotonicity), `redemption_requires_owned_shares`.

### I-31 Quote interest charges utilization, never balances
The quote-currency charge at a UTC day boundary is
`ceil(min(positive cash, maintenance requirement) × bps)`: idle cash
pays nothing, negative balances pay nothing, the charge can never
exceed the cash available, and exactly one accrual happens per day per
engine. The charge routes through the same revenue router as every
other fee income — no parallel money path exists.
*Tests:* `quote_interest_charges_only_utilized_cash` (idle exemption,
single charge per day, conservation through routing, replay),
`quote_interest_ceils_against_the_holder` unit test.
