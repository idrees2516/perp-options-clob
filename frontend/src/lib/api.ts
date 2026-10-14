/**
 * lib/api.ts — the single module owning:
 *   1. HMAC-SHA-256 request signing (Web Crypto `crypto.subtle`)
 *   2. The per-key strictly-increasing u64 nonce counter
 *   3. `VenueClient` — signed REST fetch wrapper with error mapping and
 *      single retry on nonce races / timestamp drift
 *   4. `WsManager` — the market-data WebSocket with query-auth workaround,
 *      exponential backoff reconnect, resubscription and heartbeat
 *
 * Everything follows docs/0DTE_API.md (v1) EXACTLY:
 *   canonical = key_id|timestamp|nonce|METHOD|path_with_query|sha256_hex(body)
 *   headers:  X-Api-Key / X-Timestamp (ms) / X-Nonce (u64) / X-Signature (hex)
 * All money values remain integer ticks end-to-end; formatting is display-only.
 *
 * SECURITY: the secret lives in sessionStorage, is passed to Web Crypto, and
 * is NEVER logged, never persisted elsewhere, never rendered in clear text.
 */
import {
  asWsServerMsg,
  type AccountResp,
  type AdminOk,
  type AuditResp,
  type BatchResp,
  type BookResp,
  type CancelResp,
  type FundingRequest,
  type HealthResp,
  type KeyMintRequest,
  type KeyMintResp,
  type MetricsResp,
  type OracleRequest,
  type OrderRequest,
  type PlaceResult,
  type ReplaceRequest,
  type Scope,
  type SessionRequest,
  type SessionResp,
  type SymbolsResp,
  type TapeResp,
  type TimeResp,
  type WsChannel,
  type WsClientMsg,
  type WsServerMsg,
} from './types';
import { loadNonce, saveNonce } from './session';

export const DEFAULT_BASE_URL = process.env.NEXT_PUBLIC_VENUE_URL || 'http://localhost:8080';

/** SHA-256 of the empty string — for body-less requests (contract §1). */
const EMPTY_BODY_SHA256 = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855';

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/** HTTP-level failure: body {"error":"<Code>","msg":"<human text>"}. */
export class ApiError extends Error {
  name = 'ApiError';
  constructor(
    readonly status: number,
    readonly code: string,
    readonly msg: string,
    readonly retryAfterSec?: number,
  ) {
    super(`${code}: ${msg}`);
  }
}

/** fetch() itself failed — engine unreachable / network / CORS. */
export class NetworkError extends Error {
  name = 'NetworkError';
  constructor(msg = 'engine unreachable') {
    super(msg);
  }
}

/** Client-side environment problem (e.g. no Web Crypto). */
export class ApiClientError extends Error {
  name = 'ApiClientError';
}

// ---------------------------------------------------------------------------
// Web Crypto helpers
// ---------------------------------------------------------------------------

const textEncoder = new TextEncoder();

function subtleCrypto(): SubtleCrypto {
  const c = globalThis.crypto;
  if (!c || !c.subtle) {
    throw new ApiClientError(
      'Web Crypto (crypto.subtle) unavailable — open the terminal over HTTPS or from localhost',
    );
  }
  return c.subtle;
}

function toHex(buf: ArrayBuffer): string {
  const view = new Uint8Array(buf);
  let s = '';
  for (let i = 0; i < view.length; i++) s += view[i]!.toString(16).padStart(2, '0');
  return s;
}

function hexDecode(hex: string): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/** Lowercase hex SHA-256 of the UTF-8 encoding of `data`. */
export async function sha256Hex(data: string): Promise<string> {
  const digest = await subtleCrypto().digest('SHA-256', textEncoder.encode(data));
  return toHex(digest);
}

/** hex(HMAC-SHA256(secret_bytes, message)) — secret is the 64-hex string. */
export async function hmacSha256Hex(secretHex: string, message: string): Promise<string> {
  if (!/^[0-9a-f]{64}$/.test(secretHex)) {
    throw new ApiClientError('API secret must be exactly 64 lowercase hex characters');
  }
  const subtle = subtleCrypto();
  const key = await subtle.importKey(
    'raw',
    hexDecode(secretHex),
    { name: 'HMAC', hash: 'SHA-256' },
    false,
    ['sign'],
  );
  const sig = await subtle.sign('HMAC', key, textEncoder.encode(message));
  return toHex(sig);
}

// ---------------------------------------------------------------------------
// Nonce counter — strictly increasing u64 per key, persisted in sessionStorage
// with a random 48-bit seed. Shared by REST (headers) and WS (query auth):
// the server enforces nonce > last_nonce(key) across ALL authenticated
// requests, so every draw comes through this same store.
// ---------------------------------------------------------------------------

function randomU48(): bigint {
  const b = new Uint8Array(6);
  globalThis.crypto.getRandomValues(b);
  let v = 0n;
  for (let i = 0; i < b.length; i++) v = (v << 8n) | BigInt(b[i]!);
  return v;
}

export class NonceCounter {
  constructor(private readonly keyId: string) {}

  private read(): bigint {
    const stored = loadNonce(this.keyId);
    if (stored !== null && /^\d+$/.test(stored)) {
      try {
        return BigInt(stored);
      } catch {
        /* fall through to reseed */
      }
    }
    const seed = randomU48();
    saveNonce(this.keyId, seed.toString());
    return seed;
  }

  /** Next strictly-increasing value, persisted synchronously before use. */
  next(): string {
    const v = this.read() + 1n;
    saveNonce(this.keyId, v.toString());
    return v.toString();
  }

  /**
   * Recovery after a BadNonce race (e.g. a duplicated browser tab sharing the
   * same sessionStorage nonce store): jump the counter forward by a random
   * amount so the retried request is safely above the server's last nonce.
   */
  bumpForRace(): string {
    const v = this.read() + 4096n + BigInt(Math.floor(Math.random() * 1_048_576));
    saveNonce(this.keyId, v.toString());
    return v.toString();
  }
}

// ---------------------------------------------------------------------------
// Credentials + telemetry
// ---------------------------------------------------------------------------

export interface Credentials {
  keyId: string;
  secretHex: string;
  baseUrl: string;
}

export interface RequestTelemetry {
  onResult?: (r: { ok: boolean; network: boolean; status?: number; path: string }) => void;
}

// ---------------------------------------------------------------------------
// VenueClient — signed REST wrapper
// ---------------------------------------------------------------------------

export class VenueClient {
  private readonly nonce: NonceCounter;
  private clockOffsetMs = 0;

  constructor(
    readonly creds: Credentials,
    private readonly telemetry?: RequestTelemetry,
  ) {
    this.nonce = new NonceCounter(creds.keyId);
  }

  /** Best estimate of engine wall-clock (kept within the 5s auth window). */
  venueNow(): number {
    return Date.now() + this.clockOffsetMs;
  }

  get clockOffset(): number {
    return this.clockOffsetMs;
  }

  /** Re-sync signing clock from the no-auth time endpoint. */
  async syncTime(): Promise<number> {
    const t = await VenueClient.getTime(this.creds.baseUrl);
    this.clockOffsetMs = t.ms - Date.now();
    return this.clockOffsetMs;
  }

  // -- no-auth endpoints ----------------------------------------------------

  static async getTime(baseUrl: string): Promise<TimeResp> {
    return plainFetch<TimeResp>(baseUrl, '/v1/time');
  }

  static async getHealth(baseUrl: string): Promise<HealthResp> {
    return plainFetch<HealthResp>(baseUrl, '/health');
  }

  // -- public / account (read) ----------------------------------------------

  getAccount(): Promise<AccountResp> {
    return this.request<AccountResp>('GET', '/v1/account');
  }

  getSymbols(): Promise<SymbolsResp> {
    return this.request<SymbolsResp>('GET', '/v1/symbols');
  }

  getBook(symbol: number, depth = 10): Promise<BookResp> {
    return this.request<BookResp>('GET', `/v1/book?symbol=${symbol}&depth=${depth}`);
  }

  getTape(symbol: number, n = 50): Promise<TapeResp> {
    return this.request<TapeResp>('GET', `/v1/tape?symbol=${symbol}&n=${n}`);
  }

  // -- order entry (trade) ----------------------------------------------------

  placeOrder(order: OrderRequest): Promise<PlaceResult> {
    return this.request<PlaceResult>('POST', '/v1/orders', order);
  }

  replaceOrder(req: ReplaceRequest): Promise<PlaceResult> {
    return this.request<PlaceResult>('POST', '/v1/orders/replace', req);
  }

  batchOrders(orders: OrderRequest[]): Promise<BatchResp> {
    return this.request<BatchResp>('POST', '/v1/orders/batch', { orders });
  }

  cancelOrder(orderId: number, symbol: number): Promise<CancelResp> {
    return this.request<CancelResp>('DELETE', `/v1/orders/${orderId}?symbol=${symbol}`);
  }

  // -- admin ------------------------------------------------------------------

  mintKey(req: KeyMintRequest): Promise<KeyMintResp> {
    return this.request<KeyMintResp>('POST', '/admin/keys', req);
  }

  createAccount(account: number, tier: string): Promise<AdminOk> {
    return this.request<AdminOk>('POST', '/admin/accounts', { account, tier });
  }

  deposit(req: FundingRequest): Promise<AdminOk> {
    return this.request<AdminOk>('POST', '/admin/deposit', req);
  }

  withdraw(req: FundingRequest): Promise<AdminOk> {
    return this.request<AdminOk>('POST', '/admin/withdraw', req);
  }

  setOracleMark(req: OracleRequest): Promise<AdminOk> {
    return this.request<AdminOk>('POST', '/admin/oracle', req);
  }

  createSession(req: SessionRequest): Promise<SessionResp> {
    return this.request<SessionResp>('POST', '/admin/sessions', req);
  }

  haltUnderlying(underlying: number): Promise<AdminOk> {
    return this.request<AdminOk>('POST', '/admin/halt', { underlying });
  }

  resumeUnderlying(underlying: number): Promise<AdminOk> {
    return this.request<AdminOk>('POST', '/admin/resume', { underlying });
  }

  getAudit(): Promise<AuditResp> {
    return this.request<AuditResp>('GET', '/admin/audit');
  }

  getMetrics(): Promise<MetricsResp> {
    return this.request<MetricsResp>('GET', '/metrics');
  }

  // -- core signed request ----------------------------------------------------

  private async request<T>(
    method: 'GET' | 'POST' | 'DELETE',
    pathWithQuery: string,
    body?: unknown,
  ): Promise<T> {
    let nonceRetryAvailable = true;
    let clockRetryAvailable = true;
    for (;;) {
      const ts = this.venueNow().toString();
      const nonce = this.nonce.next();
      const bodyStr = body === undefined ? '' : JSON.stringify(body);
      const bodyHash = bodyStr === '' ? EMPTY_BODY_SHA256 : await sha256Hex(bodyStr);
      // Canonical string, contract §1, joined with '|' and no spaces:
      //   key_id|timestamp|nonce|METHOD|path_with_query|sha256_hex(body_bytes)
      const canonical = [
        this.creds.keyId,
        ts,
        nonce,
        method,
        pathWithQuery,
        bodyHash,
      ].join('|');
      const signature = await hmacSha256Hex(this.creds.secretHex, canonical);

      const headers: Record<string, string> = {
        'X-Api-Key': this.creds.keyId,
        'X-Timestamp': ts,
        'X-Nonce': nonce,
        'X-Signature': signature,
      };
      if (bodyStr !== '') headers['Content-Type'] = 'application/json';

      let res: Response;
      try {
        res = await fetch(this.creds.baseUrl + pathWithQuery, {
          method,
          headers,
          body: bodyStr === '' ? undefined : bodyStr,
          cache: 'no-store',
        });
      } catch {
        this.telemetry?.onResult?.({ ok: false, network: true, path: pathWithQuery });
        throw new NetworkError(`Engine unreachable at ${this.creds.baseUrl}`);
      }

      if (res.status === 200) {
        this.telemetry?.onResult?.({ ok: true, network: false, status: 200, path: pathWithQuery });
        return (await res.json()) as T;
      }

      // Error body: {"error":"<Code>","msg":"<human text>"} (contract §2)
      let code = `Http${res.status}`;
      let msg = res.statusText || code;
      try {
        const j = (await res.json()) as { error?: unknown; msg?: unknown };
        if (typeof j.error === 'string') {
          code = j.error;
          msg = typeof j.msg === 'string' ? j.msg : code;
        }
      } catch {
        /* non-JSON error body */
      }
      this.telemetry?.onResult?.({ ok: false, network: false, status: res.status, path: pathWithQuery });

      if (code === 'BadNonce' && nonceRetryAvailable) {
        // nonce race (duplicated tab / shared counter): jump forward, re-sign once
        this.nonce.bumpForRace();
        nonceRetryAvailable = false;
        continue;
      }
      if (code === 'StaleTimestamp' && clockRetryAvailable) {
        // local clock drifted past the 5s window since the last sync
        clockRetryAvailable = false;
        try {
          await this.syncTime();
        } catch {
          /* keep the previous offset for the retry */
        }
        continue;
      }
      const retryAfterSec =
        res.status === 429 ? Number(res.headers.get('Retry-After')) || undefined : undefined;
      throw new ApiError(res.status, code, msg, retryAfterSec);
    }
  }
}

async function plainFetch<T>(baseUrl: string, path: string): Promise<T> {
  let res: Response;
  try {
    res = await fetch(baseUrl + path, { cache: 'no-store' });
  } catch {
    throw new NetworkError(`Engine unreachable at ${baseUrl}`);
  }
  if (!res.ok) {
    let code = `Http${res.status}`;
    let msg = res.statusText || code;
    try {
      const j = (await res.json()) as { error?: unknown; msg?: unknown };
      if (typeof j.error === 'string') {
        code = j.error;
        msg = typeof j.msg === 'string' ? j.msg : code;
      }
    } catch {
      /* ignore */
    }
    throw new ApiError(res.status, code, msg);
  }
  return (await res.json()) as T;
}

// ---------------------------------------------------------------------------
// WsManager — market data / private events stream
// ---------------------------------------------------------------------------

export type WsStatus = 'idle' | 'connecting' | 'live' | 'reconnecting' | 'closed';

export interface WsHandlers {
  onStatus: (s: WsStatus) => void;
  onMessage: (m: WsServerMsg) => void;
  onError?: (e: { stage: 'parse' | 'handler'; detail: string }) => void;
}

/**
 * WEB SOCKET AUTH — WORKAROUND (agreed with the backend team):
 *
 * Browsers cannot set custom headers on a WebSocket upgrade, so the contract's
 * header-based HMAC auth (§1) is impossible for `new WebSocket()`. For the WS
 * upgrade ONLY, the backend accepts the same credentials in the query string:
 *
 *     ws(s)://<host>/v1/ws?key=<key_id>&ts=<ms>&nonce=<u64>&sig=<hex>
 *
 * where `sig` = HMAC-SHA256 over the canonical string
 *
 *     key_id|ts|nonce|GET|/v1/ws|<sha256_hex_of_empty_body>
 *
 * i.e. the BARE path `/v1/ws` (no query — the query itself carries the
 * credentials). `ts` is engine-adjusted milliseconds and `nonce` draws from
 * the same per-key strictly-increasing u64 counter as REST requests, because
 * the server tracks one monotonic nonce per key across all auth attempts.
 * REST endpoints continue to use the standard X-* headers exclusively.
 */
export class WsManager {
  private ws: WebSocket | null = null;
  private status: WsStatus = 'idle';
  private attempts = 0;
  private everOpened = false;
  private closedByUser = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private hbTimer: ReturnType<typeof setInterval> | null = null;
  private lastMsgAt = 0;
  private readonly nonce: NonceCounter;
  private readonly subs = new Map<WsChannel, Set<number>>();
  private outbox: string[] = [];
  private flushTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(
    private readonly creds: Credentials,
    private readonly handlers: WsHandlers,
    private readonly now: () => number = () => Date.now(),
  ) {
    this.nonce = new NonceCounter(creds.keyId);
  }

  // -- lifecycle --------------------------------------------------------------

  connect(): void {
    if (this.closedByUser || this.ws) return;
    this.openSocket();
  }

  private openSocket(): void {
    this.setStatus(this.attempts === 0 && !this.everOpened ? 'connecting' : 'reconnecting');
    void this.buildAuthedUrl()
      .then((url) => {
        if (this.closedByUser) return;
        const ws = new WebSocket(url);
        this.ws = ws;
        ws.onopen = () => {
          if (this.ws !== ws || this.closedByUser) return;
          this.attempts = 0;
          this.everOpened = true;
          this.lastMsgAt = Date.now();
          this.replaySubscriptions();
          this.startHeartbeat();
          this.setStatus('live');
        };
        ws.onmessage = (ev: MessageEvent) => {
          if (this.ws !== ws) return;
          this.lastMsgAt = Date.now();
          let parsed: unknown;
          try {
            parsed = JSON.parse(typeof ev.data === 'string' ? ev.data : '');
          } catch {
            this.handlers.onError?.({ stage: 'parse', detail: 'non-JSON frame' });
            return;
          }
          const msg = asWsServerMsg(parsed);
          if (msg === null) {
            this.handlers.onError?.({ stage: 'parse', detail: 'unknown frame tag' });
            return;
          }
          try {
            this.handlers.onMessage(msg);
          } catch {
            // a single bad frame must never kill the stream or the app
            this.handlers.onError?.({ stage: 'handler', detail: `t=${msg.t}` });
          }
        };
        ws.onclose = () => {
          if (this.ws !== ws) return;
          this.ws = null;
          this.stopHeartbeat();
          this.outbox.length = 0;
          if (this.flushTimer !== null) {
            clearTimeout(this.flushTimer);
            this.flushTimer = null;
          }
          if (this.closedByUser) {
            this.setStatus('closed');
          } else {
            this.scheduleReconnect();
          }
        };
        ws.onerror = () => {
          /* onclose follows; nothing to do here */
        };
      })
      .catch(() => {
        // signing/Web Crypto failure — treat like a failed connect and retry
        if (this.closedByUser) return;
        this.ws = null;
        this.scheduleReconnect();
      });
  }

  private scheduleReconnect(): void {
    this.attempts += 1;
    // exponential backoff 1s -> 30s cap, ±25% jitter
    const base = Math.min(30_000, 1_000 * 2 ** Math.min(this.attempts - 1, 5));
    const delay = Math.round(base * (0.75 + Math.random() * 0.5));
    this.setStatus('reconnecting');
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (!this.closedByUser) this.openSocket();
    }, delay);
  }

  close(): void {
    this.closedByUser = true;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.stopHeartbeat();
    if (this.flushTimer !== null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    this.outbox.length = 0;
    const ws = this.ws;
    this.ws = null;
    if (ws) {
      try {
        ws.close(1000, 'logout');
      } catch {
        /* already closing */
      }
    }
    this.setStatus('closed');
  }

  // -- subscriptions ------------------------------------------------------------

  subscribe(channel: WsChannel, id: number): void {
    let set = this.subs.get(channel);
    if (!set) {
      set = new Set();
      this.subs.set(channel, set);
    }
    if (set.has(id)) return;
    set.add(id);
    this.sendSub('sub', channel, id);
  }

  unsubscribe(channel: WsChannel, id: number): void {
    const set = this.subs.get(channel);
    if (!set || !set.delete(id)) return;
    this.sendSub('unsub', channel, id);
  }

  subCount(): number {
    let n = 0;
    for (const set of this.subs.values()) n += set.size;
    return n;
  }

  /** Current subscription set (for diffing after UI changes). */
  subsSnapshot(): ReadonlyMap<WsChannel, ReadonlySet<number>> {
    return this.subs;
  }

  private sendSub(op: 'sub' | 'unsub', channel: WsChannel, id: number): void {
    // Not open: the sub set above is already updated; replaySubscriptions()
    // will emit exactly the current set once the socket opens.
    if (!this.isOpen()) return;
    const msg: WsClientMsg =
      channel === 'underlying'
        ? { op, channel, underlying: id }
        : { op, channel, symbol: id };
    this.enqueue(JSON.stringify(msg));
  }

  private replaySubscriptions(): void {
    for (const [channel, ids] of this.subs) {
      for (const id of ids) {
        const msg: WsClientMsg =
          channel === 'underlying'
            ? { op: 'sub', channel, underlying: id }
            : { op: 'sub', channel, symbol: id };
        this.enqueue(JSON.stringify(msg));
      }
    }
  }

  // -- outbound pacing (contract: <= 100 client messages / 10 s) ----------------

  private enqueue(frame: string): void {
    this.outbox.push(frame);
    if (this.flushTimer === null) this.flushTimer = setTimeout(() => this.flush(), 0);
  }

  private flush(): void {
    this.flushTimer = null;
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      this.outbox.length = 0;
      return;
    }
    let sent = 0;
    while (this.outbox.length > 0 && sent < 25) {
      const frame = this.outbox.shift()!;
      try {
        ws.send(frame);
      } catch {
        /* frame lost — snapshot refetch covers it */
      }
      sent += 1;
    }
    if (this.outbox.length > 0) {
      this.flushTimer = setTimeout(() => this.flush(), 150);
    }
  }

  // -- heartbeat / watchdog ------------------------------------------------------

  /** Server pings every 15s and expects a pong within 10s; we also send the
   *  app-level {"op":"ping"} (answered by {"t":"pong"}) and treat >35s of
   *  total silence as a dead connection. */
  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.hbTimer = setInterval(() => {
      const ws = this.ws;
      if (!ws || ws.readyState !== WebSocket.OPEN) return;
      this.enqueue(JSON.stringify({ op: 'ping' } satisfies WsClientMsg));
      if (Date.now() - this.lastMsgAt > 35_000) {
        try {
          ws.close(4000, 'stream stalled');
        } catch {
          /* onclose will still fire */
        }
      }
    }, 10_000);
  }

  private stopHeartbeat(): void {
    if (this.hbTimer !== null) {
      clearInterval(this.hbTimer);
      this.hbTimer = null;
    }
  }

  // -- internals -------------------------------------------------------------------

  private isOpen(): boolean {
    return this.ws !== null && this.ws.readyState === WebSocket.OPEN;
  }

  private async buildAuthedUrl(): Promise<string> {
    const base = new URL(this.creds.baseUrl);
    base.protocol = base.protocol === 'https:' ? 'wss:' : 'ws:';
    const ts = this.now().toString();
    const nonce = this.nonce.next();
    const canonical = [this.creds.keyId, ts, nonce, 'GET', '/v1/ws', EMPTY_BODY_SHA256].join('|');
    const sig = await hmacSha256Hex(this.creds.secretHex, canonical);
    return `${base.origin}/v1/ws?key=${this.creds.keyId}&ts=${ts}&nonce=${nonce}&sig=${sig}`;
  }

  private setStatus(s: WsStatus): void {
    if (this.status === s) return;
    this.status = s;
    this.handlers.onStatus(s);
  }
}
