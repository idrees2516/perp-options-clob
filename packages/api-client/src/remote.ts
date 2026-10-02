/**
 * RemoteConnection — the production transport: socket.io to the venue gateway.
 *
 * Owns the full connection lifecycle so the rest of the app never touches a
 * socket:
 *  - G-25 HMAC handshake (key_id + nonce + timestamp + signature) when the
 *    gateway requires authentication; anonymous when it doesn't
 *  - wire codec on every frame (bigint money survives the JSON boundary)
 *  - heartbeat ping/pong with RTT latency + zombie-connection watchdog
 *  - socket.io reconnection with queue-while-disconnected semantics; the
 *    `connect` control is replayed on every reconnect so the gateway re-sends
 *    the bootstrap, then `onResubscribe` lets the client re-arm subscriptions
 *  - two endpoint shapes:
 *      • direct origin   — "https://venue.example.com" (production deployment)
 *      • same-origin     — behind the edge proxy (Caddy): "/" + XTransformPort
 */

import { io, type Socket } from "socket.io-client";
import type { VenueControl, VenueMessage } from "@perp/types";
import { stringifyWire, tryParseWire } from "@perp/types";
import { bodyHashHex, type ApiCredentials } from "./auth";

export type RemoteStatus = "connecting" | "live" | "reconnecting" | "error" | "closed";

/** Where the gateway lives. `origin` empty ⇒ same-origin reverse-proxy mode. */
export interface RemoteEndpoint {
  /** Direct origin, e.g. "https://venue.example.com" — may carry a path prefix ("https://host/gateway"). */
  origin?: string;
  /** Same-origin gateway port (XTransformPort proxy). Default 3031. */
  port?: number;
}

export interface RemoteConnectionOptions {
  endpoint?: RemoteEndpoint;
  /** When set, the socket handshake is HMAC-signed (G-25). */
  credentials?: ApiCredentials | null;
  onMessage: (msg: VenueMessage) => void;
  onError?: (message: string) => void;
  onStatus?: (status: RemoteStatus, detail?: string) => void;
  onLatency?: (rttMs: number) => void;
  /** Fires after each reconnect — re-arm subscriptions. */
  onResubscribe?: () => void;
  /** Heartbeat cadence. Default 10s. */
  heartbeatMs?: number;
}

const DEFAULT_PORT = 3031;
const HEARTBEAT_MS = 10_000;
const MISSED_PONG_LIMIT = 3;

/** Split an origin that may carry a mount path ("https://h/gateway") */
function splitOrigin(origin: string): { base: string; path: string } {
  try {
    const u = new URL(origin);
    const path = u.pathname.replace(/\/+$/, "");
    return { base: `${u.protocol}//${u.host}`, path };
  } catch {
    return { base: origin, path: "" };
  }
}

/** Socket.io URL + engine path for an endpoint. */
export function socketTarget(endpoint: RemoteEndpoint = {}): { url: string; path: string } {
  if (endpoint.origin) {
    // Relative mount path ("/gateway") — same origin, engine path carries
    // the prefix. This is the reverse-proxy topology (docker-compose edge).
    if (endpoint.origin.startsWith("/")) {
      const mount = endpoint.origin.replace(/\/+$/, "");
      return { url: "/", path: `${mount}/socket.io` };
    }
    const { base, path } = splitOrigin(endpoint.origin);
    return { url: base, path: path ? `${path}/socket.io` : "/socket.io" };
  }
  // Same-origin edge proxy — routed by the XTransformPort query param.
  return { url: `/?XTransformPort=${endpoint.port ?? DEFAULT_PORT}`, path: "/socket.io" };
}

/** Build the socket.io URL for an endpoint (relative ⇒ proxy mode). */
export function endpointUrl(endpoint: RemoteEndpoint = {}): string {
  return socketTarget(endpoint).url;
}

/** Build a REST URL for a gateway path. */
export function endpointRestUrl(path: string, endpoint: RemoteEndpoint = {}): string {
  const p = path.startsWith("/") ? path : `/${path}`;
  if (endpoint.origin) {
    if (endpoint.origin.startsWith("/")) {
      const mount = endpoint.origin.replace(/\/+$/, "");
      return `${mount}${p}`;
    }
    const { base, path: mount } = splitOrigin(endpoint.origin);
    return `${base}${mount}${p}`;
  }
  return `${p}${p.includes("?") ? "&" : "?"}XTransformPort=${endpoint.port ?? DEFAULT_PORT}`;
}

/** Sign the socket handshake: canonical G-25 form with CONNECT method. */
export async function signHandshake(
  creds: ApiCredentials,
  nonce: bigint,
): Promise<{ key_id: string; nonce: string; ts: string; signature: string }> {
  const { hmacSha256Hex } = await import("./auth");
  const canonical = `${creds.key_id}|${nonce}|CONNECT|/venue|${bodyHashHex("")}`;
  const signature = await hmacSha256Hex(creds.secret, canonical);
  return {
    key_id: creds.key_id,
    nonce: nonce.toString(10),
    ts: Date.now().toString(10),
    signature,
  };
}

export class RemoteConnection {
  readonly kind = "remote" as const;
  private socket: Socket;
  private queue: VenueControl[] = [];
  private heartbeat: ReturnType<typeof setInterval> | null = null;
  private pingId = 0;
  private pendingPings = new Map<number, number>();
  private missedPongs = 0;
  private everConnected = false;
  private terminated = false;
  private readonly opts: Required<Pick<RemoteConnectionOptions, "heartbeatMs">> &
    RemoteConnectionOptions;

  private constructor(opts: RemoteConnectionOptions, auth: Record<string, unknown> | undefined) {
    this.opts = { heartbeatMs: opts.heartbeatMs ?? HEARTBEAT_MS, ...opts };
    const target = socketTarget(opts.endpoint);
    this.socket = io(target.url, {
      path: target.path,
      transports: ["websocket", "polling"],
      reconnection: true,
      reconnectionDelay: 800,
      reconnectionDelayMax: 15_000,
      reconnectionAttempts: Infinity,
      timeout: 15_000,
      auth,
    });
    this.wire();
  }

  /** Connect (signs the handshake first when credentials are present). */
  static async create(opts: RemoteConnectionOptions): Promise<RemoteConnection> {
    let auth: Record<string, unknown> | undefined;
    if (opts.credentials) {
      auth = await signHandshake(opts.credentials, BigInt(Date.now()) * 1000n + BigInt(Math.floor(Math.random() * 1000)));
    }
    return new RemoteConnection(opts, auth);
  }

  private wire(): void {
    this.socket.on("connect", () => {
      this.missedPongs = 0;
      this.pendingPings.clear();
      this.opts.onStatus?.(this.everConnected ? "reconnecting" : "live");
      if (this.everConnected) {
        // Reconnect: replay the connect control — the gateway answers with a
        // fresh bootstrap — then let the client re-arm its subscriptions.
        this.rawSend({ type: "connect" });
        this.opts.onResubscribe?.();
        this.opts.onStatus?.("live");
      } else {
        this.everConnected = true;
        this.opts.onStatus?.("live");
      }
      this.flush();
      this.startHeartbeat();
    });

    this.socket.on("disconnect", (reason) => {
      this.stopHeartbeat();
      if (this.terminated) {
        this.opts.onStatus?.("closed");
      } else {
        this.opts.onStatus?.("reconnecting", reason);
      }
    });

    this.socket.on("connect_error", (err) => {
      this.stopHeartbeat();
      this.opts.onStatus?.("error", err.message);
      this.opts.onError?.(`gateway: ${err.message}`);
      // Auth failures never recover on retry — stop hammering the gateway.
      const msg = String(err.message ?? "");
      if (msg.includes("auth") || msg.includes("Unauthorized") || msg.includes("signature")) {
        this.socket.io.opts.reconnectionAttempts = 1;
      }
    });

    this.socket.on("venue-message", (frame: unknown) => {
      const msg = tryParseWire<VenueMessage | null>(frame, null);
      if (!msg) return;
      if (msg.channel === "pong") {
        const sent = this.pendingPings.get(msg.id);
        if (sent !== undefined) {
          this.pendingPings.delete(msg.id);
          this.missedPongs = 0;
          this.opts.onLatency?.(Math.max(0, Date.now() - sent));
        }
        return;
      }
      this.opts.onMessage(msg);
    });
  }

  send(msg: VenueControl): void {
    if (this.terminated) return;
    if (this.socket.connected) this.rawSend(msg);
    else this.queue.push(msg);
  }

  private rawSend(msg: VenueControl): void {
    this.socket.emit("venue-control", stringifyWire(msg));
  }

  private flush(): void {
    const q = this.queue;
    this.queue = [];
    for (const msg of q) this.rawSend(msg);
  }

  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.heartbeat = setInterval(() => {
      if (!this.socket.connected) return;
      const id = ++this.pingId;
      this.pendingPings.set(id, Date.now());
      if (this.pendingPings.size > 32) {
        // Drain stale entries (keep the newest 16).
        const keep = [...this.pendingPings.keys()].slice(-16);
        this.pendingPings = new Map(keep.map((k) => [k, this.pendingPings.get(k)!]));
      }
      this.rawSend({ type: "ping", id, ts: Date.now() });
      this.missedPongs++;
      if (this.missedPongs >= MISSED_PONG_LIMIT) {
        // Zombie connection: force a fresh one.
        this.missedPongs = 0;
        this.socket.disconnect();
      }
    }, this.opts.heartbeatMs);
  }

  private stopHeartbeat(): void {
    if (this.heartbeat) clearInterval(this.heartbeat);
    this.heartbeat = null;
  }

  terminate(): void {
    this.terminated = true;
    this.stopHeartbeat();
    this.socket.removeAllListeners();
    this.socket.disconnect();
    this.opts.onStatus?.("closed");
  }
}
