# @perp/web — the perp-options-clob trading terminal

A production-grade, single-route trading terminal for the
[perp-options-clob](../../README.md) protocol — perpetuals, European &
American options, everlasting rolls, portfolio margin, RFQ, vaults,
governance, and proof-of-reserves — every subsystem of the engine surfaced
through one deterministic, event-sourced interface.

```bash
bun install
bun run dev        # → http://localhost:3000
```

Also from the repo root: `bun run dev:web` (turbo), `bun run build`,
`bun run typecheck`, `bun run lint`.

## Architecture

```
┌────────────────────────────────────────────────────────────────┐
│ apps/web — Next.js 16 (App Router, single route, dark terminal) │
│   views/terminal  book ladder · canvas chart · tape · ticket    │
│   views/markets   instruments + options chain matrix             │
│   views/…         portfolio · funding · risk · incentives ·     │
│                   vaults · rfq · governance · reserves ·         │
│                   journal · console                              │
├────────────────────────────────────────────────────────────────┤
│ @perp/api-client — VenueClient: market-data sessions            │
│   (snapshot→delta, seq gap detection, auto-resync),             │
│   HMAC-SHA256 request signing (key|nonce|method|path|body_hash) │
├────────────────────────────────────────────────────────────────┤
│ @perp/sim-engine — the deterministic venue simulator:          │
│   channel orderbook (price-time FIFO, STP, icebergs),           │
│   SFPM portfolio margin, fee ladder + 60/30/10 revenue router, │
│   funding (premium + interest), everlasting rolls, American     │
│   exercise with pro-rata assignment, liquidation cascade        │
│   (book → insurance → ADL), circuit breakers, LP vaults,        │
│   MM tier program, PoR merkle trees, governance, withdrawals    │
├────────────────────────────────────────────────────────────────┤
│ @perp/types — the domain, 1:1 with the Rust engine: all 28      │
│   Commands, all 57 Events, bigint money (u128 minor units)      │
└────────────────────────────────────────────────────────────────┘
```

### Demo mode, production path

The Rust engine is a library — a gateway host embeds it. The terminal
therefore ships two transports behind one interface:

- **Sim worker** (default): the deterministic simulator runs in a Web
  Worker; the UI consumes real snapshot/delta frames with sequence
  numbers and gap detection — the exact session contract the gateway
  speaks. No backend needed; the app deploys anywhere as a static site.
- **Remote gateway** (production): point `VenueClient` at a socket.io
  gateway speaking the same `VenueControl` / `VenueMessage` frames. The
  market-data session, the command surface, and every view are
  transport-agnostic by construction.

### The scripted session

The simulator reproduces the protocol's demo narrative:

1. **Genesis** — 7 subaccounts funded, the chain listed (3 tenors × 9
   strikes × C/P + everlasting strikes + American 30D), oracle quorum
   of 3 providers, LP vault opened at 40% of the insurance share.
2. **Market making** — MM-1/MM-2 quote the perp and the near-ATM
   chain; both enroll in the tier program.
3. **Taker flow** — seeded takers cross the book; funding accrues its
   TWAPs; hourly rewards score two-sided quoting.
4. **Oracle defense** (+2.5 min) — a compromised feed prints $160k;
   deviation quarantine trips; the median mark never moves.
5. **Liquidation shock** (+6 min) — Degen-6 bids 8,000 lots at ~20×
   leverage, then price walks down in ≤3% steps: partial liquidation
   into book liquidity, the insurance fund as buyer of last resort at
   the penalized price, ADL if the fund exhausts.
6. **Recovery → long run** — funding boundaries, vault epochs, MM
   reviews, expiries and everlasting rolls roll on by sim clock.

Speed presets (1× / 60× / 600× / 3600×) scale the economic clock; the
tape stays wall-clock paced.

## The command surface

Every control in the UI maps to an engine `Command` (see
`packages/types/src/command.ts`): limit/market/stop/trailing orders
with GTC/IOC/FOK/GTD, post-only, reduce-only, icebergs, OCO brackets,
TWAP parents, batch place/cancel/amend, exercise (American tenders),
multi-collateral deposits/conversions, RFQ packages, block trades, MMP
and cancel-on-disconnect configuration, vault subscriptions, MM tier
enrollment — plus the gateway-level surfaces: withdrawals
(time-locked pipeline), governance (weighted multisig → timelock →
grace), and proof-of-reserves publication.

The **Console** view exposes the raw composer; the **Journal** view is
the event stream itself — the append-only log whose replay
reconstructs state bit-for-bit.

## Engineering notes

- **Money is exact.** bigint quote-minor units end-to-end (worker →
  store → render); `f64` touches only analytics (IV, greeks), exactly
  as in the Rust engine.
- **Market data integrity.** Deltas apply by `seq + 1`; gaps mark the
  session desynced and demand a fresh snapshot — enforced in
  `MarketDataSession`, identically to `poc-api`.
- **Performance.** The venue runs off the main thread; the book ladder
  and chart are memoized per level / canvas-rendered; store
  subscriptions are per-selector; views are lazy-loaded.
- **Security.** Strict CSP (tightened in production), no server
  surface (static export capable), request signing via WebCrypto
  HMAC-SHA256 with strictly-increasing nonces, secrets encrypted at
  rest (AES-GCM) for the remote path.
- **Determinism.** Same seed → same journal. The sim's property tests
  assert conservation (cash + uPnL + routed fees + insurance +
  inventory = seed capital) and SHA-256/merkle correctness.
