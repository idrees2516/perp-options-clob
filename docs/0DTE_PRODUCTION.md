# ns-venue — Production Readiness & Threat Model

This document inventories what the venue defends against, how, and where
the residual boundaries are. Companion documents: the API contract
([`docs/0DTE_API.md`](0DTE_API.md)) and the README
([`rust/README.md`](../rust/README.md)).

Every defense below is **enforced by code and verified by a test** —
`rust/tests/gateway_e2e.rs` drives each scenario over real TCP against a
booted venue.

## 1. Identity & authentication

| Threat | Defense | Test |
|---|---|---|
| Forged requests | Every `/v1` + `/admin` request is HMAC-SHA256 signed over `key_id\|ts\|nonce\|METHOD\|path\|body_hash`; constant-time MAC comparison | `auth_failures_are_specific` |
| Key brute-force / enumeration | 8-hex key ids, 256-bit secrets minted from `/dev/urandom`, failures counted per key, no oracle distinguishing unknown key from bad signature beyond the documented reason codes | same |
| Replay | `X-Nonce` anti-replay: 4096-slot sliding window per key (absorbs legitimate out-of-order delivery from concurrent connections; rejects duplicates and stale nonces) + ±5 s timestamp window | `rejects_replay_and_stale`, `nonce_window_semantics` |
| Stolen signature reuse on another endpoint | The MAC covers METHOD + full request target + body hash — a signed order cannot be replayed as a cancel or against another path | tampered-body case in `auth_failures_are_specific` |
| Privilege escalation | Scopes (`read` < `trade` < `admin`) enforced per route class; **the account id is never accepted from the client** — it is bound to the key at minting and forced onto every order | `account_cannot_be_spoofed_via_orders`, scope cases |
| Cancel/replace of another user's order | Engine-level ownership check (`Reject::NotOwner`) in addition to the edge pre-check — defense in depth | `order_lifecycle…` (B cancelling A's order → 403) |
| WS clients that cannot set headers (browsers) | Query-string auth on the upgrade with the same canonical string over the bare path | `websocket_session_full_flow` |
| Admin takeover | Funding, halts, session listing, key minting, metrics, shutdown are admin-scope only | `admin_operations_and_key_minting`, `admin_shutdown_endpoint_drains` |

Secrets live in process memory only: never journaled, never logged, never
returned after minting. Key custody (HSM, rotation) is fronting
infrastructure.

## 2. Protocol & parser abuse

| Threat | Defense | Test |
|---|---|---|
| HTTP request smuggling | `Transfer-Encoding` rejected outright; `Content-Length` required for bodies; duplicate/conflicting CL rejected; pipelining refused | `http_protocol_abuse` |
| Slowloris | Header-read deadline (10 s default) closes half-open requests | slowloris case in `http_protocol_abuse` |
| Oversized payloads | Request line 512 B, header block 8 KiB, 64 headers, body 16 KiB → 400/413 | oversized-body case |
| Malformed JSON | Strict integer-only codec: floats/exponents rejected (trading is integers), depth ≤ 16, duplicate keys rejected, control chars and invalid UTF-8 rejected, surrogate pairs validated; 40 000 fuzz iterations assert no panics | `batch_orders_and_validation`, `json.rs` fuzz tests |
| WebSocket protocol attacks | RFC 6455 codec: client frames must be masked, RSV bits zero, control frames ≤ 125 B and unfragmented, frames ≤ 16 KiB, fragment bombs capped at message size; violations close 1002/1009 | `ws_protocol_abuse` |
| Subscription floods | ≤ 64 subs per connection, unknown symbol/underlying rejected, idempotent re-subscribe | sub-flood case |
| Message floods | ≤ 100 WS messages per 10 s per connection (violation closes 1008); per-key REST token bucket (burst 200, 100/s) → 429 + `Retry-After` | `rest_rate_limit_engages` |

## 3. Market abuse & resource exhaustion

| Threat | Defense | Test |
|---|---|---|
| Book flooding / memory exhaustion | Per-account resting-order cap (`max_open_orders`, default 256) → `TooManyOrders`; per-book slab caps | `hardening_caps_and_ownership` (engine), book-flood case |
| Fat-finger orders | Per-order qty cap (default 10 000) → `QtyTooLarge`; per-option price bands around the last mark (±20 %, ±5 % fast-market) | engine test + `batch_orders_and_validation` |
| Manipulated prices | Price bands, fast-market regime (±3 % trigger with hysteresis), circuit breaker halt → reopening auction | `admin_operations…` halt/resume; e2e day tests |
| Wash trades / self-matches | STP DecrementAndCancel on same-account crossings | e2e day tests |
| Cash manipulation via admin API | Deposits/withdrawals: positive integer only, known accounts only, engine re-validates (checked arithmetic — overflow can never break conservation) | `admin_operations_and_key_minting` |
| Over-committing buying power | Admission-time commitments for the FULL order quantity, released exactly per filled/cancelled qty | engine tests, randomized invariants |
| House account drain | Exact-integer double-entry: `Σ cash + house ≡ Σ deposits` after **every** command; `/admin/audit` + `/metrics` expose conservation state continuously | every test asserts conservation |
| Slow consumers blocking the venue | Bounded WS outbox (1024 frames): overflow disconnects the slow client, never blocks the fan-out | `slow_dropped` counter (metrics) |
| Connection floods | Global connection cap (default 512) + per-IP cap (default 16) at the acceptor | acceptor logic; configurable |

## 4. Integrity, persistence & recovery

* **WAL**: every command (orders, cancels, funding, sessions, oracle marks,
  clock advances) is journaled before execution into CRC32-framed batches,
  flushed per batch, fsynced every 64 flushes.
* **Crash recovery**: on boot the venue replays the WAL, tolerates a torn
  tail frame (stops there), restores accounts/positions/resting orders/
  session listings bit-exactly, verifies conservation, then re-opens for
  trading. Verified end-to-end: `wal_recovery_restores_state` (places →
  trades → resting orders across a restart, cash and positions identical).
* **Determinism**: replaying the journal reproduces `state_hash`
  bit-for-bit (e2e day tests).
* **Graceful drain**: `/admin/shutdown` (admin scope) stops accepting,
  answers 503 to stragglers, drains queues, flushes journals, joins
  workers, and exits non-zero if conservation is broken.
* Boot seeding is idempotent after replay (no double deposits on restart).

## 5. Operational posture

* `/health` (no auth) for load balancers; `/v1/time` for clock sync;
  `/metrics` (admin) with per-shard order/trade/TPS-EMA/queue-depth/journal
  bytes/state-hash and gateway counters (auth failures, rate limits, slow
  drops, malformed).
* Structured reject codes on every refusal — clients can distinguish
  `RateLimit` from `PriceBand` from `NotOwner`.
* All limits are boot-time **validated** (nonsense configurations abort
  before binding a port).
* `--loadtest` exists for benchmarking and is documented as never-for-
  production.

## 6. Residual boundaries (by design)

* TLS termination, DDoS scrubbing, IP reputation: fronting proxy tier.
* Key custody/rotation (HSM), KYC/account lifecycle: operational policy.
* Kernel-bypass network ingress for the 20M+ class: gateway tier beyond
  `std::net` (the shard queues are the integration point).
* WAL segmentation/compaction: currently append-only per shard with
  replay-from-genesis; snapshot compaction is future work.
* The delta for delta-notional limits is a documented heuristic, not a
  pricing-model greek.

## 7. Frontend hardening summary

The terminal (Next.js 16.3.4, separate repo `ns-venue-terminal`) mirrors the backend rules:
HMAC signing in the browser with a strictly-increasing nonce; secret in
`sessionStorage` only (cleared per session, never logged); every input
validated client-side against the same schema the server enforces; WS
reconnect with exponential backoff and resubscription; a visible offline
state that **disables order entry** rather than queueing guesses; integer
arithmetic end-to-end (no floats touch money).
