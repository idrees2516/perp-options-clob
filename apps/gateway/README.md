# @perp/gateway — the production venue gateway

The authoritative backend for the perp-options-clob terminal: one process, two
planes over the same wire codec (bigint-safe JSON):

- **socket.io venue stream** — `venue-control` in, `venue-message` out, the
  exact `VenueControl` / `VenueMessage` contract the embedded simulator speaks
  (bootstrap, book snapshot/delta with G-27 sequencing, prints, journal,
  account deltas, market rows, trade_fill receipts, ping/pong).
- **signed REST (G-25)** — provisioning, market data, stats, and command
  submission with HMAC-SHA256 request signing.

```
┌──────────┐  socket.io  venue-control/venue-message   ┌─────────────┐
│  web UI  │ ───────────────────────────────────────▶ │  gateway     │
│ (Next.js) │ ◀─────────────────────────────────────── │  :3031       │
└──────────┘  REST  X-Api-Key/X-Nonce/X-Signature      └──────┬──────┘
                                                          VenueHost
                                                       (SimEngine core)
```

## Quick start

```bash
bun install
bun run dev          # hot reload on http://127.0.0.1:3031 (demo profile)
```

Then in the terminal UI: open the connection settings (status bar → `SIM`,
or the cable icon in the top bar) → **Live gateway** → *Connect*.

Default demo profile: auth off, CORS `*`, anonymous sessions get
admin-equivalent rights — the sandbox/local profile.

## Production profile

```bash
GATEWAY_AUTH_REQUIRED=true \
GATEWAY_CORS_ORIGINS=https://terminal.example.com \
GATEWAY_DATA_DIR=/var/lib/gateway \
GATEWAY_VENUE_SEED=42 \
GATEWAY_VENUE_SPEED=1 \
bun apps/gateway/src/index.ts
```

With auth on:

- anonymous sockets are rejected at the handshake;
- every write (command / governance / withdrawal / PoR) needs a signed request;
- trader keys can trade but not run venue-wide controls (reset, speed, oracle
  injection, governance, operator withdrawals, PoR builds);
- `/v1/keys` and admin provisioning require an admin signature (or
  `GATEWAY_ADMIN_KEY_IDS` / `GATEWAY_ALLOW_ADMIN_PROVISION`).

## G-25 request signing

Canonical string (identical logic in `@perp/api-client` and here — single
source for the body hash lives in `@perp/types`):

```
key_id | nonce | METHOD | /path (no query) | fnv(body)
```

Headers: `X-Api-Key`, `X-Nonce` (strictly increasing per key — replays
rejected with 409), `X-Signature` (hex HMAC-SHA256), `X-Timestamp` (±30s
default tolerance). Secrets are stored AES-256-GCM encrypted under a server
pepper (`GATEWAY_SECRET_PEPPER` or `data/pepper.key`); nonces are a per-key
watermark; comparisons are constant-time.

## REST surface

| Method | Path | Auth | Purpose |
| --- | --- | --- | --- |
| GET | `/healthz` | — | liveness |
| GET | `/readyz` | — | readiness (clients, keys, phase) |
| GET | `/v1/time` | — | venue sim clock |
| POST | `/v1/auth/provision` | optional admin sig | issue a key (role trader/admin) |
| GET | `/v1/venue/snapshot` | — | full `VenueSnapshot` |
| GET | `/v1/venue/markets` | — | market rows |
| GET | `/v1/venue/book/:symbol?depth=` | — | L2 snapshot |
| GET | `/v1/venue/prints?since=&limit=` | — | print ring (last 500) |
| GET | `/v1/venue/account/:id` | signed* | account view |
| POST | `/v1/venue/command` | signed* | any of the 28 engine Commands |
| POST | `/v1/venue/governance` | admin* | governance actions |
| POST | `/v1/venue/withdrawal` | signed*/admin* | withdrawal pipeline |
| POST | `/v1/venue/por_build` | admin* | proof-of-reserves build |
| GET | `/v1/stats` | signed* | gateway + venue counters |
| GET | `/v1/keys` | admin* | key directory (redacted) |

`*` = signed request required when `GATEWAY_AUTH_REQUIRED=true`.

Responses are **wire-encoded** — bigint money arrives as `{"$bigint":"…"}`
markers; `@perp/api-client` parses them transparently.

## Market-data discipline (G-27)

Each connection owns a per-symbol frame sequencer: every book snapshot/delta
consumes exactly one seq, and the client applies the `seq === last + 1` rule —
a visible gap is a real transport drop and triggers a snapshot resync. The
engine batches mutations into net deltas; the sequencer stamps frames, so
multi-client fan-out never fabricates gaps.

## Limits

Token buckets: per-key (default 120 cap / 60 per sec), per-IP (6× key headroom),
provisioning per IP (default 5 per ~2h). 429s carry `Retry-After`; the client's
`AuthSession` enters backoff automatically.

## Configuration

See `.env.example` — every knob (ports, CORS, auth, seeds, data dir, buckets,
client cap, body cap, log level) is env-driven. `GATEWAY_AUTH_REQUIRED=true`
**requires** an explicit `GATEWAY_CORS_ORIGINS` allowlist — the gateway
refuses to boot otherwise.

## Tests

```bash
cd apps/gateway && bun test
```

10 integration tests pin the contract: signed REST + bigint wire, order
placement receipts, malformed-command handling (422, never 500), 401 bad
signatures, 409 nonce replays, 429 rate limiting, the socket venue stream
(bootstrap → G-27 seq discipline → trade_fill → pong), and authenticated-mode
gating (anonymous rejected, trader limited, admin-gated provisioning).
