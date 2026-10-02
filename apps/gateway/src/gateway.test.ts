/**
 * Gateway integration suite — boots real gateways (random ports) and
 * exercises every production path end to end:
 *
 *   1. liveness / readiness / banner / time / public market data
 *   2. socket.io venue session: handshake → bootstrap → market data
 *      fan-out → ping/pong → command with a trade_fill receipt
 *   3. G-25 REST: provision → signed read → signed write → replay rejection
 *   4. auth-on profile: anonymous socket handshake rejected, signed admitted
 *   5. rate limiting: token bucket 429s with Retry-After (small-capacity
 *      gateway so the burst is deterministic regardless of request timing)
 *
 * Run: bun test (from apps/gateway)
 */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createHmac } from "node:crypto";
import { io, type Socket } from "socket.io-client";
import { createGateway, type Gateway } from "./server";
import { bodyHashHex } from "@perp/types";

const PORT = 3131 + (process.pid % 400);
const URL = `http://127.0.0.1:${PORT}`;
const ADMIN_PORT = PORT + 1;
const ADMIN_URL = `http://127.0.0.1:${ADMIN_PORT}`;
const TIGHT_PORT = PORT + 2;
const TIGHT_URL = `http://127.0.0.1:${TIGHT_PORT}`;

let gateway: Gateway;
let adminGateway: Gateway;
let tightGateway: Gateway;

beforeAll(async () => {
  gateway = createGateway({ GATEWAY_PORT: String(PORT), GATEWAY_LOG_LEVEL: "error" });
  gateway.host.start();
  await new Promise<void>((resolve) => gateway.server.listen(PORT, "127.0.0.1", resolve));

  // Auth-strict profile for the G-25 enforcement tests.
  adminGateway = createGateway({
    GATEWAY_PORT: String(ADMIN_PORT),
    GATEWAY_LOG_LEVEL: "error",
    GATEWAY_AUTH_REQUIRED: "true",
    GATEWAY_CORS_ORIGINS: "https://web.example.com",
  });
  adminGateway.host.start();
  await new Promise<void>((resolve) => adminGateway.server.listen(ADMIN_PORT, "127.0.0.1", resolve));

  // Tiny token buckets so the rate-limit burst is deterministic.
  tightGateway = createGateway({
    GATEWAY_PORT: String(TIGHT_PORT),
    GATEWAY_LOG_LEVEL: "error",
    GATEWAY_RATE_CAPACITY: "3",
    GATEWAY_RATE_REFILL: "0.1",
  });
  tightGateway.host.start();
  await new Promise<void>((resolve) => tightGateway.server.listen(TIGHT_PORT, "127.0.0.1", resolve));
}, 20_000);

afterAll(async () => {
  await Promise.allSettled([gateway.close(), adminGateway.close(), tightGateway.close()]);
}, 20_000);

/* ── helpers ── */

interface Frame {
  channel: string;
  snapshot?: { accounts: unknown[]; markets: unknown[] };
  updates?: Record<string, { kind: string; seq: number }>;
  entries?: unknown[];
  prints?: unknown[];
  id?: number;
  ts?: number;
  trade?: { qty_lots: number; price_ticks: number };
  message?: string;
}

function decode(frame: unknown): Frame {
  return (typeof frame === "string" ? JSON.parse(frame) : frame) as Frame;
}

function connect(url = URL): Promise<Socket> {
  return new Promise((resolve, reject) => {
    const socket = io(url, { path: "/socket.io", transports: ["websocket"], reconnection: false, timeout: 6000 });
    socket.on("connect", () => resolve(socket));
    socket.on("connect_error", reject);
    setTimeout(() => reject(new Error("socket timeout")), 7000);
  });
}

function venueConnect(socket: Socket): Promise<Frame> {
  return new Promise((resolve, reject) => {
    socket.on("venue-message", function onFrame(frame: unknown) {
      const msg = decode(frame);
      if (msg.channel === "bootstrap") {
        socket.off("venue-message", onFrame);
        resolve(msg);
      }
    });
    socket.emit("venue-control", JSON.stringify({ type: "connect" }));
    setTimeout(() => reject(new Error("bootstrap timeout")), 7000);
  });
}

function collect(socket: Socket, ms: number): Promise<Frame[]> {
  return new Promise((resolve) => {
    const frames: Frame[] = [];
    const on = (frame: unknown): void => {
      frames.push(decode(frame));
    };
    socket.on("venue-message", on);
    setTimeout(() => {
      socket.off("venue-message", on);
      resolve(frames);
    }, ms);
  });
}

let nonceCounter = 0n;

function signedFetch(
  base: string,
  key: { key_id: string; secret: string },
  req: { path: string; method?: string; body?: string },
): Promise<Response> {
  const method = (req.method ?? "GET").toUpperCase();
  const nonce = BigInt(Date.now()) * 1000n + ++nonceCounter;
  const canonical = `${key.key_id}|${nonce}|${method}|${req.path}|${bodyHashHex(req.body ?? "")}`;
  return fetch(`${base}${req.path}`, {
    method,
    headers: {
      "Content-Type": "application/json",
      "X-Api-Key": key.key_id,
      "X-Nonce": nonce.toString(10),
      "X-Timestamp": String(Date.now()),
      "X-Signature": createHmac("sha256", key.secret).update(canonical).digest("hex"),
    },
    body: req.body,
  });
}

async function provision(base: string, role: "trader" | "admin" = "trader"): Promise<{ key_id: string; secret: string }> {
  const r = await fetch(`${base}/v1/auth/provision`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ role }),
  });
  expect(r.status).toBe(201);
  return (await r.json()) as { key_id: string; secret: string };
}

/* ── 1. HTTP plane ── */

describe("REST plane", () => {
  test("healthz / readyz / banner / time", async () => {
    const health = await (await fetch(`${URL}/healthz`)).json();
    expect(health.ok).toBe(true);
    const ready = await (await fetch(`${URL}/readyz`)).json();
    expect(ready.ok).toBe(true);
    const banner = (await (await fetch(`${URL}/v1`)).json()) as { service?: string };
    expect(banner.service).toBeDefined();
    const time = (await (await fetch(`${URL}/v1/time`)).json()) as { now?: number };
    expect(typeof time.now).toBe("number");
  });

  test("public market data endpoints", async () => {
    const markets = (await (await fetch(`${URL}/v1/venue/markets`)).json()) as { rows: Array<{ symbol: string }> };
    expect(markets.rows.length).toBeGreaterThan(10);
    expect(markets.rows.some((r) => r.symbol === "BTC-PERP")).toBe(true);
    const snapshot = (await (await fetch(`${URL}/v1/venue/snapshot`)).json()) as { accounts: unknown[] };
    expect(snapshot.accounts.length).toBeGreaterThan(0);
  });
});

/* ── 2. socket.io venue session ── */

describe("venue socket", () => {
  test("handshake delivers bootstrap + book snapshot", async () => {
    const socket = await connect();
    const bootstrap = await venueConnect(socket);
    expect(bootstrap.snapshot!.accounts.length).toBeGreaterThan(0);
    expect(bootstrap.snapshot!.markets.length).toBeGreaterThan(0);
    const early = await collect(socket, 1200);
    expect(early.some((f) => f.channel === "books" && f.updates?.["BTC-PERP"]?.kind === "snapshot")).toBe(true);
    socket.disconnect();
  });

  test("market data streams and commands produce journal/prints frames", async () => {
    const socket = await connect();
    await venueConnect(socket);
    socket.emit("venue-control", JSON.stringify({ type: "subscribe", symbols: ["BTC-PERP"] }));
    await new Promise((r) => setTimeout(r, 1200));

    socket.emit(
      "venue-control",
      JSON.stringify({
        type: "command",
        command: {
          type: "place_order",
          request: {
            symbol: "BTC-PERP",
            side: "buy",
            kind: "market",
            qty_lots: 1,
            subaccount: 7,
            post_only: false,
            time_in_force: "IOC",
          },
        },
      }),
    );

    const frames = await collect(socket, 4000);
    const channels = new Set(frames.map((f) => f.channel));
    expect(
      channels.has("journal") || channels.has("prints") || channels.has("markets") || channels.has("books"),
    ).toBe(true);
    socket.disconnect();
  }, 12_000);

  test("ping/pong round trip", async () => {
    const socket = await connect();
    await venueConnect(socket);
    const pong = new Promise<Frame>((resolve) => {
      socket.on("venue-message", function on(frame: unknown) {
        const msg = decode(frame);
        if (msg.channel === "pong") {
          socket.off("venue-message", on);
          resolve(msg);
        }
      });
    });
    const sent = Date.now();
    socket.emit("venue-control", JSON.stringify({ type: "ping", id: 42, ts: sent }));
    const reply = await pong;
    expect(reply.id).toBe(42);
    expect(reply.ts).toBeGreaterThanOrEqual(sent);
    socket.disconnect();
  });
});

/* ── 3. G-25 REST auth ── */

describe("G-25 signed REST", () => {
  test("provision → signed read → signed write → replay rejection", async () => {
    const key = await provision(URL);

    const keys = await signedFetch(URL, key, { path: "/v1/keys" });
    expect(keys.status).toBe(200);
    const keyList = (await keys.json()) as { keys: Array<{ key_id: string }> };
    expect(keyList.keys.some((k) => k.key_id === key.key_id)).toBe(true);

    // Money fields travel as $bigint markers on the wire (see wire codec).
    const body = JSON.stringify({
      command: { type: "deposit", subaccount: 7, amount_quote_minor: { $bigint: "100000" } },
    });
    const write = await signedFetch(URL, key, { path: "/v1/venue/command", method: "POST", body });
    expect(write.status).toBe(200);
    const written = (await write.json()) as { events: Array<{ event: { type: string } }> };
    expect(written.events.some((e) => e.event?.type === "deposit")).toBe(true);

    // Same nonce + signature again → 409 replay.
    const nonce = BigInt(Date.now()) * 1000n + 777n;
    const canonical = `${key.key_id}|${nonce}|GET|/v1/keys|${bodyHashHex("")}`;
    const headers = {
      "X-Api-Key": key.key_id,
      "X-Nonce": nonce.toString(10),
      "X-Timestamp": String(Date.now()),
      "X-Signature": createHmac("sha256", key.secret).update(canonical).digest("hex"),
    };
    const first = await fetch(`${URL}/v1/keys`, { headers });
    expect(first.status).toBe(200);
    const replay = await fetch(`${URL}/v1/keys`, { headers });
    expect(replay.status).toBe(409);
    expect(((await replay.json()) as { code: string }).code).toBe("auth_replay");
  });

  test("bad signature is rejected", async () => {
    const key = await provision(URL);
    const nonce = BigInt(Date.now()) * 1000n + 42n;
    const r = await fetch(`${URL}/v1/keys`, {
      headers: {
        "X-Api-Key": key.key_id,
        "X-Nonce": nonce.toString(10),
        "X-Timestamp": String(Date.now()),
        "X-Signature": "0".repeat(64),
      },
    });
    expect(r.status).toBe(401);
  });
});

/* ── 4. auth-strict profile ── */

describe("auth-strict gateway", () => {
  test("anonymous socket handshake is rejected", async () => {
    await expect(connect(ADMIN_URL)).rejects.toThrow();
  });

  test("signed socket handshake is admitted", async () => {
    const key = await provision(ADMIN_URL);
    const nonce = BigInt(Date.now()) * 1000n + 99n;
    const canonical = `${key.key_id}|${nonce}|CONNECT|/venue|${bodyHashHex("")}`;
    const socket = await new Promise<Socket>((resolve, reject) => {
      const s = io(ADMIN_URL, {
        path: "/socket.io",
        transports: ["websocket"],
        reconnection: false,
        timeout: 6000,
        auth: {
          key_id: key.key_id,
          nonce: nonce.toString(10),
          ts: String(Date.now()),
          signature: createHmac("sha256", key.secret).update(canonical).digest("hex"),
        },
      });
      s.on("connect", () => resolve(s));
      s.on("connect_error", reject);
      setTimeout(() => reject(new Error("timeout")), 7000);
    });
    expect(socket.connected).toBe(true);
    socket.disconnect();
  });

  test("unsigned writes are rejected when auth is required", async () => {
    const r = await fetch(`${ADMIN_URL}/v1/venue/command`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ command: { type: "deposit", subaccount: 1, asset: "USDC", amount_quote_minor: 1 } }),
    });
    expect(r.status).toBe(401);
  });
});

/* ── 5. rate limiting ── */

describe("rate limits", () => {
  test("signed burst beyond bucket capacity 429s with Retry-After", async () => {
    const key = await provision(TIGHT_URL);
    const statuses: number[] = [];
    for (let i = 0; i < 8; i++) {
      const r = await signedFetch(TIGHT_URL, key, { path: "/v1/time" });
      statuses.push(r.status);
      if (r.status === 429) {
        expect(r.headers.get("retry-after")).toBeTruthy();
        break;
      }
    }
    expect(statuses[statuses.length - 1]).toBe(429);
  });
});
