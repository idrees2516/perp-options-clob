/**
 * The socket.io venue — one namespace, two events:
 *   client → server: "venue-control"  (wire-encoded VenueControl)
 *   server → client: "venue-message"  (wire-encoded VenueMessage)
 *
 * Auth (G-25): the handshake carries { key_id, nonce, ts, signature } over
 * the canonical CONNECT form. Demo deployments (auth off) admit anonymous
 * sessions with demo (admin-equivalent) rights — the sandbox profile.
 *
 * Every connection gets its own G-27 book stream sequencer; commands are
 * rate-limited per connection; venue-wide controls are role-gated.
 */

import type { Server as HttpServer } from "node:http";
import { Server, type Socket } from "socket.io";
import { BookFrameSequencer } from "@perp/sim-engine";
import { stringifyWire, tryParseWire } from "@perp/types";
import type { Command, JournalEntry, Trade, VenueControl, VenueMessage } from "@perp/types";
import type { GatewayConfig } from "./config";
import type { CredentialStore } from "./credentials";
import { log } from "./log";
import { BucketRegistry, type TokenBucket } from "./ratelimit";
import { isAdminLike, type ClientSession, type VenueHost } from "./venue";

export interface SocketDeps {
  config: GatewayConfig;
  host: VenueHost;
  creds: CredentialStore;
}

interface HandshakeAuth {
  key_id?: unknown;
  nonce?: unknown;
  ts?: unknown;
  signature?: unknown;
}

export function createVenueSocket(server: HttpServer, deps: SocketDeps): Server {
  const { config, host, creds } = deps;
  const buckets = new BucketRegistry(Math.max(30, Math.floor(config.rateCapacity / 2)), config.rateRefillPerSec);

  const io = new Server(server, {
    cors: { origin: config.corsOrigins, methods: ["GET", "POST"] },
    pingInterval: 25_000,
    pingTimeout: 60_000,
    maxHttpBufferSize: config.maxBodyBytes,
  });

  io.use((socket, next) => {
    if (host.clientCount >= config.maxClients) {
      next(new Error("venue at capacity — try again later"));
      return;
    }
    if (!config.authRequired) {
      socket.data.role = "demo";
      next();
      return;
    }
    const auth = (socket.handshake.auth ?? {}) as HandshakeAuth;
    const result = creds.verify(auth, "CONNECT", "/venue", "");
    if (!result.ok) {
      if (result.code === "auth_replay") host.stats.rejectedReplays++;
      log.warn("socket.auth_rejected", {
        key_id: typeof auth.key_id === "string" ? auth.key_id : null,
        code: result.code,
      });
      next(new Error(`auth: ${result.message}`));
      return;
    }
    socket.data.role = result.record.role;
    socket.data.keyId = result.record.key_id;
    next();
  });

  io.on("connection", (socket: Socket) => {
    const role = (socket.data.role as ClientSession["role"]) ?? "demo";
    const bucket = buckets.of(socket.id);

    const session: ClientSession = {
      id: socket.id,
      role,
      symbols: new Set<string>(),
      sequencer: new BookFrameSequencer(),
      send: (msg: VenueMessage): void => {
        socket.emit("venue-message", stringifyWire(msg));
      },
    };
    host.attach(session);
    log.info("socket.connected", { id: socket.id, role, clients: host.clientCount });

    socket.on("venue-control", (frame: unknown) => {
      const msg = tryParseWire<VenueControl | null>(frame, null);
      if (!msg || typeof msg.type !== "string") return;
      handleControl(session, msg, bucket);
    });

    socket.on("disconnect", (reason) => {
      buckets.delete(socket.id);
      host.detach(session.id);
      log.info("socket.disconnected", { id: socket.id, reason });
    });

    socket.on("error", (err) => {
      log.warn("socket.error", { id: socket.id, error: String(err) });
    });

    // NOTE: no data is pushed until the client sends its opening "connect"
    // control — listeners on the other side must be attached first (the
    // VenueClient contract mirrors the sim worker).
  });

  function handleControl(session: ClientSession, msg: VenueControl, bucket: TokenBucket): void {
    const admin = isAdminLike(session.role);
    const deny = (what: string): void => {
      session.send({ channel: "error", message: `${what} requires an admin key` });
    };

    switch (msg.type) {
      case "connect": {
        // The venue is authoritative — seed is ignored; speed via set_speed.
        host.handshake(session);
        break;
      }
      case "ping": {
        session.send({ channel: "pong", id: msg.id, ts: Date.now() });
        break;
      }
      case "subscribe": {
        host.subscribe(session, msg.symbols);
        break;
      }
      case "request_snapshot": {
        host.requestSnapshot(session);
        break;
      }
      case "command": {
        if (!bucket.tryRemove(1)) {
          host.stats.rateLimited++;
          session.send({ channel: "error", message: "rate limited — slow down (token bucket)" });
          return;
        }
        const accounts = commandAccounts(msg.command);
        let outcome: { events: JournalEntry[]; trades: Trade[] };
        try {
          outcome = host.command(msg.command);
        } catch (err) {
          log.warn("command.invalid", { id: session.id, error: String(err) });
          session.send({ channel: "error", message: `invalid command: ${String(err)}` });
          return;
        }
        for (const t of outcome.trades) {
          const taker = accounts.has(t.taker_subaccount);
          const maker = accounts.has(t.maker_subaccount);
          if (taker || maker) {
            session.send({ channel: "trade_fill", trade: t, taker, maker });
          }
        }
        break;
      }
      case "governance": {
        if (!admin) {
          deny("governance");
          return;
        }
        const err = host.governance(msg.action);
        if (err) session.send({ channel: "error", message: `governance: ${err}` });
        break;
      }
      case "withdrawal": {
        if (msg.op !== "request" && !admin) {
          deny(`withdrawal ${msg.op}`);
          return;
        }
        host.withdrawalOp(msg);
        break;
      }
      case "por_build": {
        if (!admin) {
          deny("proof-of-reserves builds");
          return;
        }
        host.porBuild();
        break;
      }
      case "oracle_inject": {
        if (!admin) {
          deny("oracle injection");
          return;
        }
        host.oracleInject(msg.provider, msg.price_quote_minor);
        break;
      }
      case "oracle_toggle_quarantine": {
        if (!admin) {
          deny("oracle quarantine");
          return;
        }
        host.oracleToggleQuarantine(msg.provider);
        break;
      }
      case "reset": {
        if (!admin) {
          deny("venue resets");
          return;
        }
        host.reset(msg.seed ?? Date.now() % 1_000_000);
        break;
      }
      case "set_speed": {
        if (!admin) {
          deny("venue clock controls");
          return;
        }
        host.setSpeed(msg.speed);
        break;
      }
      case "pause": {
        if (!admin) {
          deny("venue clock controls");
          return;
        }
        host.pause();
        break;
      }
      case "resume": {
        if (!admin) {
          deny("venue clock controls");
          return;
        }
        host.resume();
        break;
      }
      case "step": {
        if (!admin) {
          deny("venue clock controls");
          return;
        }
        host.step(msg.ms ?? 250);
        break;
      }
    }
  }

  return io;
}

/** Accounts a command touches — used to route trade_fill receipts. */
function commandAccounts(cmd: Command): Set<number> {
  const out = new Set<number>();
  const c = cmd as unknown as Record<string, unknown>;
  const add = (v: unknown): void => {
    if (typeof v === "number") out.add(v);
  };
  // Direct fields (deposit/withdraw/cancel/transfer/exercise/…).
  add(c.subaccount);
  add(c.from);
  add(c.taker);
  // Order-carrying commands nest the owner inside the request.
  const req = c.request as Record<string, unknown> | undefined;
  if (req) add(req.subaccount);
  for (const key of ["first", "second"] as const) {
    const r = c[key] as Record<string, unknown> | undefined;
    if (r) add(r.subaccount);
  }
  const reqs = c.requests as Array<Record<string, unknown>> | undefined;
  if (Array.isArray(reqs)) for (const r of reqs) add(r?.subaccount);
  return out;
}
