# ns-venue 0DTE Platform — HTTP + WebSocket API Contract (v1)

This document is the **normative interface** between the Rust matching engine
(`rust/`, binary `ns-venue`) and the Next.js terminal (`frontend/`).
Both sides implement exactly this contract.

- Transport: HTTP/1.1 + WebSocket (RFC 6455), default `http://localhost:8080`
  (configurable `--port`). TLS is terminated at a reverse proxy in production.
- All money/price values are **integer ticks** (1 tick = 0.01). Quantities are
  integer contracts. **No floats anywhere** — clients format for display only.
- Timestamps: integer milliseconds since Unix epoch (`ms`), or engine nanos
  where stated.
- CORS: server answers preflight `OPTIONS` with
  `Access-Control-Allow-Origin: *`, `Allow-Methods: GET,POST,DELETE,OPTIONS`,
  `Allow-Headers: Content-Type,X-Api-Key,X-Timestamp,X-Nonce,X-Signature`.
  All responses carry `Access-Control-Allow-Origin: *`.

## 1. Authentication (HMAC request signing)

Every `/v1/*` and `/admin/*` request (except the no-auth endpoints below) must
carry these headers:

| Header         | Value                                            |
|----------------|--------------------------------------------------|
| `X-Api-Key`    | key id, 8 lowercase hex chars (e.g. `a1b2c3d4`)  |
| `X-Timestamp`  | request time, Unix **milliseconds**, decimal    |
| `X-Nonce`      | decimal `u64`, **strictly increasing per key**   |
| `X-Signature`  | lowercase hex HMAC-SHA256                        |

**Canonical signing string** (fields joined with `|`, no spaces):

```
key_id + "|" + timestamp + "|" + nonce + "|" + METHOD + "|" + path_with_query + "|" + sha256_hex(body_bytes)
```

- `METHOD` — uppercase request method (`GET`, `POST`, `DELETE`).
- `path_with_query` — the URL path **including** query string, exactly as sent
  (e.g. `/v1/book?symbol=42&depth=10`), no host, no scheme.
- `sha256_hex(body_bytes)` — lowercase hex SHA-256 of the raw request body;
  for empty body, SHA-256 of the empty string
  (`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`).
- `signature = hex(HMAC-SHA256(secret_bytes, canonical))` where `secret_bytes`
  is the 32-byte key secret, hex-decoded from the 64-char hex secret string.

Server-side checks (in order): key exists → `|now − timestamp| ≤ 5000 ms` →
nonce anti-replay window (see below) → constant-time HMAC compare → scope
check. Failures: `401 {"error":"BadSignature"|"StaleTimestamp"|"BadNonce"|"UnknownKey"}`
or `403 {"error":"ScopeDenied"}`.

**Nonce anti-replay window**: nonces must increase over time, but
concurrent connections under one key legitimately deliver them out of
order. The server keeps a 64-slot sliding window above the highest nonce
seen: any unused nonce inside the window is accepted; duplicates and
nonces older than the window are rejected (`BadNonce`). Clients should
still use a monotonic counter per key — the window only absorbs delivery
reordering, it does not license stale nonces.

**Scopes**: `read` (GET data), `trade` (implies read, order actions),
`admin` (implies all: accounts, funding, sessions, halts, metrics, key minting).

No-auth endpoints: `GET /health`, `GET /v1/time`.

WebSocket (`GET /v1/ws`) authenticates with the same headers on the upgrade
request. Failed auth ⇒ HTTP 401 before upgrade.

## 2. REST endpoints

Success responses are `200` with JSON. Errors:
`400 Malformed`, `401/403` auth, `404 NotFound`, `409 UnknownOrder`,
`413 TooLarge`, `429 RateLimited` (+ `Retry-After` seconds), `503` engine
timeout/shutdown. Error body: `{"error":"<Code>","msg":"<human text>"}`.
Business (engine) rejects of well-formed orders return `200` with
`"ok": false` — see §2.3.

### 2.1 Public / market data (`read`)

`GET /v1/time` → `{"ms":1730000000000,"nanos":1730000000000000000}` (no auth)

`GET /health` → `{"ok":true,"uptime_ms":123456,"shards":2,"draining":false}` (no auth)

`GET /v1/symbols` →
```json
{
  "underlyings": [
    {"underlying":0,"mark":500000,"ref":500000,"fast":false,"halted":false}
  ],
  "symbols": [
    {"symbol":1,"underlying":0,"kind":"call","strike":500000,
     "expiry_min":29700000,"expiry_ms":1782000000000,
     "phase":"continuous","mark":250,"last":250,"vol":120,
     "bid":240,"bid_qty":10,"ask":260,"ask_qty":8,
     "open":0,"high":0,"low":0,"n_trades":12}
  ]
}
```
- `kind`: `"call"` | `"put"`. `strike`, prices: ticks.
- `expiry_min`: minutes since Unix epoch; `expiry_ms = expiry_min * 60000`.
- `phase`: `"continuous"` | `"auction"` | `"expired"`.

`GET /v1/book?symbol=<u32>&depth=<1..25, default 10>` →
```json
{"symbol":1,"bids":[[250,10],[240,8]],"asks":[[260,8],[270,5]]}
```
Bids descending, asks ascending. `[price_ticks, qty]`.

`GET /v1/tape?symbol=<u32>&n=<1..200, default 50>` →
```json
{"symbol":1,"trades":[{"price":250,"qty":5,"aggressor":"buy","ms":1730000000123}]}
```
Most recent first. `aggressor`: `"buy"` | `"sell"` | `"auction"`.

### 2.2 Account (`read`)

`GET /v1/account` →
```json
{
  "account":7,"tier":"base",
  "balances":[
    {"underlying":0,"cash":100000,"hold":0,"margin_required":0,
     "margin_reserved":0,"available":100000,"fees_paid":2,"rebates_recv":0}
  ],
  "positions":[{"symbol":1,"qty":5,"margin":0,"delta_notional":0}],
  "orders":[
    {"order":123,"symbol":1,"side":"buy","price":250,"qty":10,"leaves":10,
     "tif":"gtc","kind":"limit","post_only":false,"ms":1730000000123}
  ]
}
```
- `qty`: signed (`+` long / `−` short). `positions` only non-flat.

### 2.3 Order entry (`trade`)

`POST /v1/orders` body:
```json
{"symbol":1,"side":"buy","qty":10,"price":250,
 "tif":"gtc","kind":"limit","post_only":false,"reduce_only":false}
```
- `symbol`: u32 global symbol id. `side`: `"buy"`|`"sell"`. `qty`: u32, 1..=10000.
- `kind`: `"limit"`|`"market"`. For market, `price` = worst-acceptable
  protection price (mandatory), `tif` must be `"ioc"`.
- `tif`: `"gtc"`|`"ioc"`|`"fok"`.
- Unknown fields are rejected (`400 Malformed`). All numbers must be JSON
  integers (no exponents, no floats).

Response (engine ack):
```json
{"ok":true,"order_id":123,"leaves":5,
 "fills":[{"price":250,"qty":5,"fee":2,"rebate":0}]}
```
or `{"ok":false,"code":7,"reason":"RateLimit"}`.
`reason` strings: `UnknownSymbol, UnknownAccount, PriceBand, PostOnlyCross,
ReduceOnlyWouldIncrease, InsufficientFunds, PositionLimit, RateLimit, Malformed,
NotContinuous, FokDepth, UnknownOrder, Halted, DeltaLimit, TooManyOrders,
QtyTooLarge, NotOwner`.

`DELETE /v1/orders/<order_id>?symbol=<u32>` →
`{"ok":true,"order_id":123,"leaves":4}` or `409 {"error":"UnknownOrder"}`.

`POST /v1/orders/replace` body `{"order":123,"symbol":1,"price":240,"qty":12}`
→ atomic cancel-replace; response like `POST /v1/orders` (new order id).

`POST /v1/orders/batch` body `{"orders":[ ...up to 64 order objects... ]}` →
`{"results":[ ...same shape as POST /v1/orders, in order... ]}`.

### 2.4 Admin (`admin`)

`POST /admin/keys` `{"account":7,"scope":"trade","tier":"base"}` →
`{"key_id":"e41a2f7c","secret":"<64 hex chars — shown once>","account":7,"scope":"trade"}`

`POST /admin/accounts` `{"account":7,"tier":"base|silver|gold|prime"}` → `{"ok":true}`

`POST /admin/deposit` / `POST /admin/withdraw`
`{"account":7,"underlying":0,"amount":1000000}` (ticks, u64) → `{"ok":true}`

`POST /admin/oracle` `{"underlying":0,"price":500000}` → `{"ok":true}` (mark feed)

`POST /admin/sessions`
`{"underlying":0,"expiry_ms":1782000000000,"strikes":[490000,500000,510000],"kinds":["call","put"]}` → `{"ok":true,"symbols":[11,12,13,14,15,16]}` (global ids)

`POST /admin/halt` `{"underlying":0}` / `POST /admin/resume` `{"underlying":0}` → `{"ok":true}`

`GET /admin/audit` →
```json
{"conservation":true,"shards":[
  {"shard":0,"underlying":0,"live_accounts":3,"open_positions":2,"symbols":48,
   "book_orders":12,"house":0,"deposits":3000000,"sum_cash":3000000,
   "state_hash":"1f2e3d4c5b6a"}],
 "gateway":{"conns":4,"ws":2,"rx":12034,"tx":40211,"auth_fails":2,"rate_limited":1}}
```

`GET /metrics` (admin) → same shape as `/admin/audit` plus per-shard counters:
`orders, rejects, trades, volume, premium, fees, cancels, cmds, expiries,
auctions, queue_depth, journal_bytes, tps_ema` and gateway `tps` (rest+ws).

## 3. WebSocket stream

`GET /v1/ws` (authenticated upgrade). Server → client, one JSON object per
text frame, always with `"t"` type tag:

```json
{"t":"hello","account":7,"scope":"trade","ms":1730000000000}
{"t":"bbo","symbol":1,"bid":250,"bid_qty":10,"ask":260,"ask_qty":8}
{"t":"trade","symbol":1,"price":250,"qty":5,"aggressor":"buy","ms":...}
{"t":"depth","symbol":1,"bids":[[250,10]],"asks":[[260,8]]}
{"t":"phase","symbol":1,"phase":"auction"}
{"t":"mark","symbol":1,"price":255}
{"t":"underlying","underlying":0,"price":500000,"ref":500000,"fast":false,"halted":false}
{"t":"halt","underlying":0,"level":7} / {"t":"resumed","underlying":0}
{"t":"ack","order":123,"symbol":1,"leaves":5,"ms":...}
{"t":"reject","order":123,"symbol":1,"code":7,"reason":"RateLimit"}
{"t":"canceled","order":123,"symbol":1,"leaves":4,"reason":"user","ms":...}
{"t":"fill","symbol":1,"price":250,"qty":5,"order":123,"role":"taker",
 "fee":2,"rebate":0,"ms":...}
{"t":"exercised","symbol":1,"qty":5,"intrinsic":100,"cash_flow":498,"fee":2,"ms":...}
{"t":"balance","underlying":0,"cash":99998,"hold":0,"margin_required":0,
 "margin_reserved":0,"available":99998}
{"t":"pong"}
```
- `bbo`, `trade`, `depth`, `phase`, `mark`, `underlying`, `halt`, `resumed` are
  **public channels** (subscribe by symbol / underlying).
- `ack`, `reject`, `canceled`, `fill`, `exercised`, `balance` are **private** —
  always delivered to the authenticated account's connections, no subscription
  needed.
- `depth` refresh: at most one frame per symbol per 100 ms (top 10 each side).
- `reason` in `canceled`: `"user"|"stp"|"expiry"|"auction"|"oco"|"replace"`.
- `role` in `fill`: `"taker"`|`"maker"`.

Client → server (text frames, JSON):
```json
{"op":"sub","channel":"bbo|trade|depth|phase|mark","symbol":1}
{"op":"unsub","channel":"...","symbol":1}
{"op":"sub","channel":"underlying","underlying":0}
{"op":"unsub","channel":"underlying","underlying":0}
{"op":"ping"}
```
On subscribe, an immediate snapshot is sent (current bbo/depth/tape tail/phase).
Limits: ≤ 64 subscriptions per connection; ≤ 100 client messages / 10 s;
frame size ≤ 16 KiB. Violations close the connection (code 1008).

Server pings every 15 s; a client that fails to pong within 10 s is dropped.
A slow consumer whose outbound queue overflows (1024 frames) is dropped.
Reconnect with exponential backoff (client responsibility).

## 4. Rate limits (per API key)

- REST: 200 burst, sustained 100 req/s (token bucket).
- WS client messages: 100 per 10 s.
- New orders additionally pass the engine's per-account order-rate token
  bucket (engine reject code 7 `RateLimit`).
- `429` responses include `Retry-After: 1`.

## 5. Number precision rules

- Prices/strikes/cash: unsigned integers (ticks). Cash may be negative (JSON
  integer, i64 range in practice).
- Quantities: unsigned integers (contracts) in requests; signed in positions.
- Clients MUST parse integers exactly (no `parseFloat`). u64-safe parsing
  required (`BigInt` where necessary; all real values fit i53 comfortably).
