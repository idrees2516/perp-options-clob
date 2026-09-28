# Operations

How the deterministic core is meant to be deployed, monitored, and
operated. The engine is a library; this document defines the shape of
the host around it — and the operational surfaces the library already
exposes.

---

## 1. Process architecture

```
                    ┌──────────────────────────────────────────┐
                    │              API gateway                 │
                    │  WS/REST (poc-api protocol) · FIX 4.4    │
                    │  (poc-api::fix_session over TcpWire)     │
                    └──────────────────┬───────────────────────┘
                                       │ Command (validated, ordered)
                    ┌──────────────────▼───────────────────────┐
                    │              Sequencer                   │
                    │  Engine::process(cmd) → Vec<Event>      │
                    │  plan (pure) → apply (single mutator)    │
                    └───────┬─────────────────────┬────────────┘
                            │ journal (append)    │ state view
                    ┌───────▼────────┐   ┌────────▼─────────┐
                    │  WAL (G-24)    │   │  Market data     │
                    │  framed, CRC'd │   │  (snapshot+delta,│
                    │  chain-hashed  │   │   G-27)          │
                    └───────┬────────┘   └──────────────────┘
                            │ checkpoints
                    ┌───────▼─────────────────────────────────┐
                    │  Settlement window (G-30)               │
                    │  StateCapture → DiffBuilder → merkleized │
                    │  SettlementBatch → chain + exit queue   │
                    └─────────────────────────────────────────┘
```

**The host adds:** process supervision, TLS termination, key custody,
clock discipline (the engine takes `now_ms` as a command parameter —
the host owns the clock source and must feed it monotonically), and
the chain connection.

**Process count:** the engine is single-threaded by design
(determinism). The gateway fans in; the sequencer is the single
bottleneck by construction — this is the architecture every auditable
venue converges on. Horizontal scaling happens *behind* the sequencer
(read replicas over the journal), never beside it.

---

## 2. Startup and recovery

1. **Load the checkpoint** (atomic, CRC'd — `poc-persist`).
2. **Replay the WAL tail** — torn-tail-safe: a truncated final frame is
   dropped, not fatal. Recovery replays at millions of commands per
   second (measured: 7.45 ms for 50k commands), so checkpoint cadence
   is a storage decision, not a recovery-time decision.
3. **Verify the settlement chain** if a state root is on-chain
   (`validate_chain`: header hashes + `prev_root` linkage).
4. **Publish the market-data snapshot**; clients resync
   (`MarketDataSession::resync` — a gap desyncs, never silently
   continues).
5. **Serve.**

Crash-recovery determinism is stress-tested: 6,004 commands replayed
to a byte-identical state (`crash_recovery_determinism` scenario).

---

## 3. The clock

Every time-sensitive behavior takes an explicit `TimestampMs`: command
wall-clock, oracle timestamps, tick boundaries. The host must feed
time monotonically (the engine clamps but the journal is only as
ordered as the clock). UTC day boundaries drive the interest accruals;
review windows drive the MM tier program; the funding intervals drive
the TWAP settlements. A host with clock skew degrades gracefully (a
late settle settles late, a late review reviews late) because every
boundary is idempotent-once, but the journal's ordering guarantees are
the reason to take clock discipline seriously.

---

## 4. Monitoring surfaces

The library exposes structured views; the host exports them:

| Surface | Source | Alert on |
|---|---|---|
| Insurance coverage ratio | `Engine::insurance_coverage_permille` | < 1.0 (thin), > 2.0 (overflow active) |
| Cascade velocity | `Engine::cascade_closures_in_window(now)` | approaching `breaker.max_closures_per_window` |
| Breaker state | halted map + `price_breaker_until` | any trip (market quality) |
| Oracle health | per-provider feeds, quarantine events | a provider quarantined; quorum at the minimum |
| Vault NAV | `Engine::vaults()` (NAV per share, liabilities) | a draw (NAV drop) — LPs are absorbing |
| Fee routing | `venue_pools()` + revenue router cumulative | ratio drift from 60/30/10 (config change only) |
| MM tier census | `Engine::mm_ledger` + discount map | enrollment collapse (liquidity risk ahead) |
| Vol surface | `Engine::vol_surface` view, staleness sweeps | mark at the anchor (book quotes absent) |
| PoR | publication ledger | attested reserve < committed liabilities |

**The one chart that matters:** insurance coverage ratio through a
liquidation cascade. It is the venue's solvency in one number, and every
economic mechanism in `docs/ECONOMICS.md` exists to keep it above 1.0.

---

## 5. Runbooks

### 5.1 Oracle provider quarantined

*Expected:* providers disagree beyond the deviation bound; the cluster
consensus quarantines the outlier and keeps the mark.
*Do:* nothing — the defense is automatic. Watch that the quarantined
provider recovers or is decommissioned; if quorum drops to the
minimum, halve position limits (`limits` config) until a replacement
feed is live.

### 5.2 Circuit breaker tripped (price dislocation)

*Expected:* sustained BBO-vs-oracle dislocation beyond `dislocation_bps`
for the window; the instrument halts for `cooldown_ms`.
*Do:* nothing — trading resumes automatically. Post-incident, compare
the book's tape against the oracle tape; if the dislocation was
manipulation, the breaker window plus the journaled fills are the
evidence.

### 5.3 Liquidation cascade in progress

*Expected:* the velocity breaker suspends the cascade above
`max_closures_per_window`; the coverage policy boosts penalties.
*Do:* monitor the insurance coverage ratio. If it approaches the thin
threshold, the boosted penalties are already working; if it approaches
zero, iterative ADL is the designed endgame — let it run, then
post-mortem the account population's leverage.

### 5.4 Insurance fund drawn / vault NAV dropped

*Expected:* LPs absorbed what would have been socialized.
*Do:* publish the drawdown (the vault state is public in the PoR
liability set); check the revenue share is routing (fee income should
be rebuilding NAV).

### 5.5 Proof-of-reserves publication

Weekly (or per governance policy): `Engine::por_liabilities(now)` →
`build_report_from_rows(rows, nonce, ts)` → publish
`report_commitment(report)` on-chain + the attestation through the
`ReserveAttestor` implementation. The nonce must strictly increase —
a repeated nonce means a recycled report, and the ledger rejects it.

### 5.6 MM tier review completed

*Expected:* monthly `MmTierAdjusted` events for every enrollment.
*Do:* export the tier census. A mass demotion means depth is about to
thin (makers stopped quoting — find out why before the book does);
mass promotions mean the discount budget is being consumed (check the
ladder's cost against revenue).

---

## 6. Safety parameters (the reference defaults and their meaning)

| Parameter | Default | What it trades |
|---|---|---|
| `dislocation_bps` / window / cooldown | 500 bps / 60 s / 120 s | manipulation sensitivity vs. nuisance halts |
| `max_closures_per_window` | 50 / 60 s | cascade control vs. under-margination festering |
| coverage thin/comfortable/target | 0.5× / 1.5× / 2.0× | penalty revenue vs. liquidator flight |
| max penalty boost | 1.5× | backstop funding speed vs. liquidated-account cost |
| funding cap | per-market | tethering strength vs. payment shock |
| MM discount cap | 50% | depth subsidy vs. fee-base erosion |
| vault epoch | 24 h | LP liquidity vs. NAV staleness |
| PoR cadence | governance | attestation freshness vs. operational cost |
| quote interest | 0 (off) | revenue vs. customer optics — the reason it's a timelock decision |

Every parameter change is a `Command` through the governance path
(G-33: weighted multisig → timelock → grace, guardian veto), and the
journal records the change — parameter history is auditable by
replay.

---

## 7. The safety nets, in firing order

1. **Pre-trade gates** — bands, margin simulation, greeks caps, MMP
   windows (most abuse never becomes an event).
2. **Circuit breakers** — dislocation and velocity halt the venue's
   exposure to a bad market.
3. **Margin** — the scenario grid keeps healthy books healthy through
   the shock.
4. **Liquidation** — partial first, penalized, book-crossing; the
   cascade is deficit-ordered and deterministic.
5. **Insurance fund** — buyer of last resort at the penalized price;
   coverage-scaled penalties fund it.
6. **LP vaults** — the fund's draw absorber, priced by revenue share.
7. **ADL** — iterative, profit-ranked, the designed endgame when
   capital runs out.
8. **Settlement escape hatch** — withdrawal intents past their force
   window are provable neglect; users exit against the committed state.

Each net is tested, each has an invariant, and each is cheaper for the
venue than the one below it — which is the entire engineering logic of
the stack.
