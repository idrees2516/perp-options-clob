/**
 * The signed REST surface of the venue gateway (G-25).
 *
 * Reads are public market data; writes require an HMAC-signed request when
 * GATEWAY_AUTH_REQUIRED=true (demo mode admits anonymous writes). Every
 * response is wire-encoded (bigint money survives JSON). Rate limits are
 * token buckets per key / per IP; 429s carry Retry-After.
 *
 * Signing (see @perp/api-client AuthSession): headers
 *   X-Api-Key / X-Nonce / X-Signature / X-Timestamp
 * over the canonical `key_id|nonce|METHOD|/path|body_hash` where the path
 * EXCLUDES the query string and body_hash is the FNV hash of the raw body.
 */

import type { IncomingMessage, ServerResponse } from "node:http";
import { stringifyWire, tryParseWire } from "@perp/types";
import type { Command, GovernanceAction } from "@perp/types";
import type { GatewayConfig } from "./config";
import type { CredentialStore } from "./credentials";
import { log } from "./log";
import { BucketRegistry } from "./ratelimit";
import { isAdminLike, type VenueHost } from "./venue";

export interface HttpDeps {
  config: GatewayConfig;
  host: VenueHost;
  creds: CredentialStore;
}

type Role = "demo" | "trader" | "admin";

interface Ctx {
  method: string;
  pathname: string;
  query: URLSearchParams;
  rawBody: string;
  role: Role;
  keyId: string | null;
  authFailure: { status: number; code: string; message: string } | null;
}

export function createRestRouter(deps: HttpDeps): (req: IncomingMessage, res: ServerResponse) => void {
  const { config, host, creds } = deps;
  const keyBuckets = new BucketRegistry(config.rateCapacity, config.rateRefillPerSec);
  // IP bucket has generous headroom: it exists for anonymous abuse, not for
  // legitimate bursty clients sharing a NAT/proxy egress.
  const ipBuckets = new BucketRegistry(config.rateCapacity * 6, config.rateRefillPerSec * 3);
  const provisionBuckets = new BucketRegistry(
    config.provisionCapacity,
    Math.max(config.provisionRefillPerSec, 1 / 86_400),
  );

  return async (req, res) => {
    const started = Date.now();
    try {
      const url = new URL(req.url ?? "/", "http://internal");
      const ctx = await readContext(req, url);

      setHeaders(res, config);
      if (corsPreflight(req, res)) return;

      if (ctx.authFailure) {
        if (ctx.authFailure.code === "auth_replay") host.stats.rejectedReplays++;
        return sendJson(res, ctx.authFailure.status, { error: ctx.authFailure.message, code: ctx.authFailure.code });
      }

      // Every signed request consumes key-bucket tokens — the bucket is the
      // per-key budget for the whole API surface, not just writes.
      if (ctx.keyId && !keyBuckets.of(ctx.keyId).tryRemove(1)) {
        host.stats.rateLimited++;
        const after = Math.ceil(keyBuckets.of(ctx.keyId).retryAfterMs(1) / 1000);
        return sendJson(res, 429, { error: "rate limit", code: "rate_limited" }, { "Retry-After": String(after) });
      }

      if (!ipBuckets.of(ipOf(req)).tryRemove(1)) {
        host.stats.rateLimited++;
        return sendJson(res, 429, { error: "ip rate limit", code: "rate_limited" }, { "Retry-After": "1" });
      }

      await route(ctx, req, res, { keyBuckets, provisionBuckets });
      log.debug("http.request", {
        method: ctx.method,
        path: ctx.pathname,
        status: res.statusCode,
        ms: Date.now() - started,
        key: ctx.keyId,
      });
    } catch (err) {
      log.error("http.error", { error: String(err), url: req.url });
      if (!res.headersSent) sendJson(res, 500, { error: "internal error", code: "internal" });
      else res.end();
    }
  };

  async function route(
    ctx: Ctx,
    req: IncomingMessage,
    res: ServerResponse,
    limits: { keyBuckets: BucketRegistry; provisionBuckets: BucketRegistry },
  ): Promise<void> {
    const { config: cfg, host, creds: store } = deps;
    const admin = isAdminLike(ctx.role);
    const signed = ctx.keyId !== null;

    /* ── liveness / readiness / banner ── */
    if (ctx.method === "GET" && ctx.pathname === "/healthz") {
      return sendJson(res, 200, { ok: true, uptimeMs: Date.now() - host.stats.startedAt });
    }
    if (ctx.method === "GET" && ctx.pathname === "/readyz") {
      return sendJson(res, 200, { ok: true, clients: host.clientCount, keys: store.size, phase: host.venuePhase() });
    }
    if (ctx.method === "GET" && (ctx.pathname === "/" || ctx.pathname === "/v1")) {
      return sendJson(res, 200, {
        service: "perp-options-clob venue gateway",
        version: 1,
        authRequired: cfg.authRequired,
        transport: 'socket.io — emit "venue-control" (wire string), listen "venue-message"',
      });
    }

    /* ── time ── */
    if (ctx.method === "GET" && ctx.pathname === "/v1/time") {
      return sendJson(res, 200, { now: host.venue.engine.now });
    }

    /* ── provisioning ── */
    if (ctx.method === "POST" && ctx.pathname === "/v1/auth/provision") {
      const ip = ipOf(req);
      if (!limits.provisionBuckets.of(ip).tryRemove(1)) {
        host.stats.rateLimited++;
        const after = Math.ceil(limits.provisionBuckets.of(ip).retryAfterMs(1) / 1000);
        return sendJson(res, 429, { error: "provisioning rate limit", code: "rate_limited" }, { "Retry-After": String(after) });
      }
      const body = tryParseWire<{ role?: string }>(ctx.rawBody || "{}", {});
      const wantsAdmin = body?.role === "admin";
      if (wantsAdmin && cfg.authRequired && !cfg.allowAdminProvision && ctx.role !== "admin") {
        return sendJson(res, 403, {
          error: "admin provisioning requires an admin signature or GATEWAY_ALLOW_ADMIN_PROVISION",
          code: "admin_provision_denied",
        });
      }
      return sendJson(res, 201, store.provision(wantsAdmin ? "admin" : "trader"));
    }

    /* ── public market data ── */
    if (ctx.method === "GET" && ctx.pathname === "/v1/venue/snapshot") {
      return sendJson(res, 200, host.snapshot());
    }
    if (ctx.method === "GET" && ctx.pathname === "/v1/venue/markets") {
      return sendJson(res, 200, { rows: host.marketRows() });
    }
    if (ctx.method === "GET" && ctx.pathname.startsWith("/v1/venue/book/")) {
      const symbol = decodeURIComponent(ctx.pathname.slice("/v1/venue/book/".length));
      const depth = clampInt(ctx.query.get("depth"), 1, 100, 20);
      const update = host.bookSnapshot(symbol, depth);
      if (!update) return sendJson(res, 404, { error: `unknown symbol ${symbol}`, code: "unknown_symbol" });
      return sendJson(res, 200, { symbol, update });
    }
    if (ctx.method === "GET" && ctx.pathname === "/v1/venue/prints") {
      const since = clampInt(ctx.query.get("since"), 0, Number.MAX_SAFE_INTEGER, 0);
      const limit = clampInt(ctx.query.get("limit"), 1, 500, 200);
      return sendJson(res, 200, { prints: host.recentPrints(since, limit) });
    }

    /* ── authed surfaces (signed request required in prod mode) ── */
    const authedPaths = [
      "/v1/venue/account/",
      "/v1/venue/command",
      "/v1/venue/governance",
      "/v1/venue/withdrawal",
      "/v1/venue/por_build",
      "/v1/stats",
      "/v1/keys",
    ];
    if (cfg.authRequired && !signed && authedPaths.some((p) => ctx.pathname.startsWith(p))) {
      return sendJson(res, 401, { error: "signed request required", code: "auth_required" });
    }

    if (ctx.method === "GET" && ctx.pathname.startsWith("/v1/venue/account/")) {
      const id = Number(ctx.pathname.slice("/v1/venue/account/".length));
      const view = Number.isInteger(id) ? host.accountView(id) : null;
      if (!view) return sendJson(res, 404, { error: `unknown account ${id}`, code: "unknown_account" });
      return sendJson(res, 200, { view });
    }

    if (ctx.method === "GET" && ctx.pathname === "/v1/stats") {
      const e = host.venue.engine;
      return sendJson(res, 200, {
        uptimeMs: Date.now() - host.stats.startedAt,
        connections: host.clientCount,
        keys: store.size,
        commandsProcessed: host.stats.commandsProcessed,
        messagesOut: host.stats.messagesOut,
        rateLimited: host.stats.rateLimited,
        rejectedReplays: store.replayCount,
        seqDrops: 0,
        venue: {
          events: e.stats.events,
          trades: e.stats.trades,
          ticks: e.ticks,
          now: e.now,
          phase: host.venuePhase(),
        },
      });
    }

    if (ctx.method === "GET" && ctx.pathname === "/v1/keys") {
      if (cfg.authRequired && ctx.role !== "admin") {
        return sendJson(res, 403, { error: "admin key required", code: "forbidden" });
      }
      return sendJson(res, 200, { keys: store.list() });
    }

    /* ── writes ── */
    if (ctx.method === "POST" && ctx.pathname === "/v1/venue/command") {
      const body = tryParseWire<{ command?: Command }>(ctx.rawBody || "{}", {});
      if (!body?.command || typeof body.command !== "object") {
        return sendJson(res, 400, { error: "body must be { command }", code: "bad_request" });
      }
      try {
        const outcome = host.command(body.command);
        return sendJson(res, 200, { ok: true, events: outcome.events, trades: outcome.trades });
      } catch (err) {
        log.warn("command.invalid", { key: ctx.keyId, error: String(err) });
        return sendJson(res, 422, { error: `invalid command: ${String(err)}`, code: "invalid_command" });
      }
    }

    if (ctx.method === "POST" && ctx.pathname === "/v1/venue/governance") {
      if (cfg.authRequired && !admin) {
        return sendJson(res, 403, { error: "admin key required", code: "forbidden" });
      }
      const body = tryParseWire<{ action?: GovernanceAction }>(ctx.rawBody || "{}", {});
      if (!body?.action) return sendJson(res, 400, { error: "body must be { action }", code: "bad_request" });
      const err = host.governance(body.action);
      return sendJson(res, err ? 422 : 200, err ? { ok: false, error: err } : { ok: true, error: null });
    }

    if (ctx.method === "POST" && ctx.pathname === "/v1/venue/withdrawal") {
      const body = tryParseWire<{
        op?: VenueControlOp;
        id?: number;
        subaccount?: number;
        amount_quote_minor?: bigint;
        destination?: string;
      }>(ctx.rawBody || "{}", {});
      if (!body?.op) return sendJson(res, 400, { error: "body must include op", code: "bad_request" });
      if (body.op !== "request" && cfg.authRequired && !admin) {
        return sendJson(res, 403, { error: "admin key required", code: "forbidden" });
      }
      host.withdrawalOp({
        op: body.op,
        id: body.id,
        subaccount: body.subaccount,
        amount_quote_minor: body.amount_quote_minor,
        destination: body.destination,
      });
      return sendJson(res, 200, { ok: true, events: [], trades: [] });
    }

    if (ctx.method === "POST" && ctx.pathname === "/v1/venue/por_build") {
      if (cfg.authRequired && !admin) {
        return sendJson(res, 403, { error: "admin key required", code: "forbidden" });
      }
      host.porBuild();
      return sendJson(res, 200, { ok: true });
    }

    return sendJson(res, 404, { error: "no such route", code: "not_found" });
  }

  async function readContext(req: IncomingMessage, url: URL): Promise<Ctx> {
    const rawBody = await readBody(req, config.maxBodyBytes);
    const auth = {
      key_id: header(req, "x-api-key"),
      nonce: header(req, "x-nonce"),
      signature: header(req, "x-signature"),
      ts: header(req, "x-timestamp"),
    };
    let role: Role = "demo";
    let keyId: string | null = null;
    let authFailure: Ctx["authFailure"] = null;
    if (auth.key_id && auth.nonce && auth.signature && auth.ts) {
      const result = creds.verify(auth, (req.method ?? "GET").toUpperCase(), url.pathname, rawBody);
      if (result.ok) {
        role = result.record.role;
        keyId = result.record.key_id;
      } else {
        authFailure = { status: result.status, code: result.code, message: result.message };
      }
    }
    return { method: (req.method ?? "GET").toUpperCase(), pathname: url.pathname, query: url.searchParams, rawBody, role, keyId, authFailure };
  }
}

type VenueControlOp = "request" | "approve" | "cancel" | "settle_due" | "force_queue";

/* ── helpers ── */

function header(req: IncomingMessage, name: string): string | undefined {
  const v = req.headers[name];
  return Array.isArray(v) ? v[0] : v;
}

function readBody(req: IncomingMessage, cap: number): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    req.on("data", (chunk: Buffer) => {
      size += chunk.length;
      if (size > cap) {
        reject(new Error("body too large"));
        req.destroy();
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    req.on("error", reject);
  });
}

function sendJson(res: ServerResponse, status: number, payload: unknown, extraHeaders?: Record<string, string>): void {
  const body = stringifyWire(payload);
  res.writeHead(status, {
    "Content-Type": "application/json",
    "Content-Length": Buffer.byteLength(body),
    ...extraHeaders,
  });
  res.end(body);
}

function setHeaders(res: ServerResponse, config: GatewayConfig): void {
  res.setHeader("X-Content-Type-Options", "nosniff");
  res.setHeader("Cache-Control", "no-store");
  res.setHeader("Referrer-Policy", "no-referrer");
  res.setHeader("Access-Control-Allow-Origin", config.corsOrigins.includes("*") ? "*" : config.corsOrigins.join(", "));
  res.setHeader("Access-Control-Allow-Methods", "GET, POST, OPTIONS");
  res.setHeader("Access-Control-Allow-Headers", "Content-Type, X-Api-Key, X-Nonce, X-Signature, X-Timestamp");
}

function corsPreflight(req: IncomingMessage, res: ServerResponse): boolean {
  if (req.method !== "OPTIONS") return false;
  res.writeHead(204);
  res.end();
  return true;
}

function ipOf(req: IncomingMessage): string {
  const fwd = req.headers["x-forwarded-for"];
  if (typeof fwd === "string" && fwd.length > 0) return fwd.split(",")[0]!.trim();
  return req.socket.remoteAddress ?? "unknown";
}

function clampInt(raw: string | null, min: number, max: number, fallback: number | undefined): number {
  if (raw === null || raw === "") return fallback ?? min;
  const n = Number(raw);
  if (!Number.isFinite(n)) return fallback ?? min;
  return Math.min(max, Math.max(min, Math.trunc(n)));
}
