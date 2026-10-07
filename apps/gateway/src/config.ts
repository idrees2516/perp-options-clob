/**
 * Gateway configuration — every knob is env-driven, validated, frozen.
 *
 * Demo profile (default):  auth off, CORS *, in-memory credentials,
 * admin-equivalent anonymous sessions — exactly the sandbox / local demo.
 *
 * Production profile:      GATEWAY_AUTH_REQUIRED=true, a data dir for
 * persisted keys, a secret pepper, CORS locked to the web origin, admin
 * gated behind GATEWAY_ADMIN_KEY_IDS or signed-by-admin provisioning.
 */

export interface GatewayConfig {
  /** TCP port. Default 3031. */
  port: number;
  /** Bind host. Default 127.0.0.1 (containers override to 0.0.0.0). */
  host: string;
  /** Allowed CORS origins ("*" = any). Default ["*"]. */
  corsOrigins: string[];
  /** Require G-25 auth for sockets and write REST. Default false (demo). */
  authRequired: boolean;
  /** Allow provisioning admin keys without an admin signature. Default false. */
  allowAdminProvision: boolean;
  /** Key ids forced to the admin role (comma list). */
  adminKeyIds: string[];
  /** Deterministic venue seed. Default 42. */
  venueSeed: number;
  /** Venue clock multiplier at boot. Default 600. */
  venueSpeed: number;
  /** Where credentials + pepper persist (null = in-memory only). */
  dataDir: string | null;
  /** Per-key token bucket: capacity. Default 120. */
  rateCapacity: number;
  /** Per-key token bucket: refill per second. Default 60. */
  rateRefillPerSec: number;
  /** Provisioning bucket per IP: capacity. Default 5. */
  provisionCapacity: number;
  /** Provisioning bucket per IP: refill per second. Default 1/7200. */
  provisionRefillPerSec: number;
  /** Max concurrent socket clients. Default 64. */
  maxClients: number;
  /** Signed-request timestamp tolerance (ms). Default 30_000. */
  timestampToleranceMs: number;
  /** Max REST body bytes. Default 262_144. */
  maxBodyBytes: number;
  /** Log level. Default "info". */
  logLevel: "debug" | "info" | "warn" | "error";
}

function num(name: string, fallback: number): number {
  const v = process.env[name];
  if (v === undefined || v === "") return fallback;
  const n = Number(v);
  if (!Number.isFinite(n)) throw new Error(`${name}: not a number (${v})`);
  return n;
}

function bool(name: string, fallback: boolean): boolean {
  const v = process.env[name];
  if (v === undefined || v === "") return fallback;
  return v === "1" || v.toLowerCase() === "true";
}

function list(name: string): string[] {
  const v = process.env[name];
  if (v === undefined || v.trim() === "") return [];
  return v.split(",").map((s) => s.trim()).filter(Boolean);
}

export function loadConfig(): GatewayConfig {
  const level = (process.env.GATEWAY_LOG_LEVEL ?? "info") as GatewayConfig["logLevel"];
  if (!["debug", "info", "warn", "error"].includes(level)) {
    throw new Error(`GATEWAY_LOG_LEVEL: invalid (${level})`);
  }
  const dataDir = process.env.GATEWAY_DATA_DIR?.trim() || null;
  const authRequired = bool("GATEWAY_AUTH_REQUIRED", false);
  const cors = process.env.GATEWAY_CORS_ORIGINS?.trim()
    ? list("GATEWAY_CORS_ORIGINS")
    : ["*"];
  if (authRequired && cors.includes("*")) {
    // Fail loud rather than silently running an open CORS authenticated venue.
    throw new Error("GATEWAY_AUTH_REQUIRED=true requires an explicit GATEWAY_CORS_ORIGINS allowlist");
  }
  return Object.freeze({
    port: num("GATEWAY_PORT", 3031),
    host: process.env.GATEWAY_HOST ?? "127.0.0.1",
    corsOrigins: cors,
    authRequired,
    allowAdminProvision: bool("GATEWAY_ALLOW_ADMIN_PROVISION", false),
    adminKeyIds: list("GATEWAY_ADMIN_KEY_IDS"),
    venueSeed: num("GATEWAY_VENUE_SEED", 42),
    venueSpeed: num("GATEWAY_VENUE_SPEED", 600),
    dataDir,
    rateCapacity: num("GATEWAY_RATE_CAPACITY", 120),
    rateRefillPerSec: num("GATEWAY_RATE_REFILL", 60),
    provisionCapacity: num("GATEWAY_PROVISION_CAPACITY", 5),
    provisionRefillPerSec: num("GATEWAY_PROVISION_REFILL", 1 / 7200),
    maxClients: num("GATEWAY_MAX_CLIENTS", 64),
    timestampToleranceMs: num("GATEWAY_TIMESTAMP_TOLERANCE_MS", 30_000),
    maxBodyBytes: num("GATEWAY_MAX_BODY_BYTES", 262_144),
    logLevel: level,
  });
}
