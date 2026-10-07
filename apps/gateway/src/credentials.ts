/**
 * Credential store — the server half of G-25.
 *
 * key_id + secret pairs are provisioned once; the secret is stored
 * AES-256-GCM encrypted under a server-side pepper (env-provided or
 * persisted in the data dir; ephemeral in-memory for demo deployments).
 * Verification recomputes the HMAC over the canonical request string and
 * compares in constant time. Nonces are a strictly-increasing per-key
 * watermark — replays are rejected and counted.
 *
 * Canonical string: `key_id|nonce|method|path|body_hash`
 *  - REST: method = HTTP verb, path = pathname WITHOUT query string,
 *    body_hash = FNV body hash of the exact raw request body.
 *  - Socket handshake: method = "CONNECT", path = "/venue", empty body.
 */

import { createCipheriv, createDecipheriv, createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { bodyHashHex } from "@perp/types";
import type { GatewayConfig } from "./config";
import { log } from "./log";

export type KeyRole = "trader" | "admin";

export interface ApiKeyRecord {
  key_id: string;
  /** base64(iv|tag|ciphertext) — the secret under the pepper. */
  enc: string;
  role: KeyRole;
  created_at: number;
  last_used: number | null;
  /** Strictly increasing watermark (string decimal of a bigint). */
  last_nonce: string;
  disabled: boolean;
}

export type VerifyResult =
  | { ok: true; record: ApiKeyRecord }
  | { ok: false; status: number; code: string; message: string };

export interface ProvisionedKey {
  key_id: string;
  secret: string;
  role: KeyRole;
}

export class CredentialStore {
  private keys = new Map<string, ApiKeyRecord>();
  private pepper: Buffer;
  private readonly file: string | null;
  private readonly tolerancems: number;
  replayCount = 0;

  constructor(private config: GatewayConfig) {
    this.tolerancems = config.timestampToleranceMs;
    this.file = config.dataDir ? join(config.dataDir, "credentials.json") : null;
    this.pepper = this.loadPepper();
    this.loadPersisted();
  }

  /* ── provisioning ── */

  provision(role: KeyRole): ProvisionedKey {
    const keyId = `poc-${randomBytes(6).toString("hex")}`;
    const secret = randomBytes(32).toString("base64url");
    const record: ApiKeyRecord = {
      key_id: keyId,
      enc: this.encrypt(secret),
      role,
      created_at: Date.now(),
      last_used: null,
      last_nonce: "0",
      disabled: false,
    };
    if (configAdminOverride(this.config, keyId) === "admin") record.role = "admin";
    this.keys.set(keyId, record);
    this.persist();
    log.info("key.provisioned", { key_id: keyId, role: record.role });
    return { key_id: keyId, secret, role: record.role };
  }

  get size(): number {
    return this.keys.size;
  }

  list(): Array<Pick<ApiKeyRecord, "key_id" | "role" | "created_at" | "last_used" | "disabled">> {
    return [...this.keys.values()].map((k) => ({
      key_id: k.key_id,
      role: k.role,
      created_at: k.created_at,
      last_used: k.last_used,
      disabled: k.disabled,
    }));
  }

  /* ── verification ── */

  /**
   * Verify a signed request (REST headers or socket handshake auth payload).
   * `path` must be the bare pathname (no query); `rawBody` the exact bytes.
   */
  verify(
    auth: { key_id?: unknown; nonce?: unknown; signature?: unknown; ts?: unknown },
    method: string,
    path: string,
    rawBody: string,
  ): VerifyResult {
    const keyId = typeof auth.key_id === "string" ? auth.key_id : "";
    const nonceStr = typeof auth.nonce === "string" ? auth.nonce : "";
    const signature = typeof auth.signature === "string" ? auth.signature : "";
    const tsStr = typeof auth.ts === "string" ? auth.ts : "";

    if (!keyId || !nonceStr || !signature || !tsStr) {
      return { ok: false, status: 401, code: "auth_missing", message: "missing auth fields" };
    }
    const record = this.keys.get(keyId);
    if (!record) return { ok: false, status: 401, code: "auth_unknown_key", message: "unknown key" };
    if (record.disabled) return { ok: false, status: 403, code: "auth_disabled", message: "key disabled" };

    const ts = Number(tsStr);
    if (!Number.isFinite(ts) || Math.abs(Date.now() - ts) > this.tolerancems) {
      return { ok: false, status: 401, code: "auth_stale", message: "timestamp outside tolerance" };
    }

    let nonce: bigint;
    try {
      nonce = BigInt(nonceStr);
    } catch {
      return { ok: false, status: 401, code: "auth_bad_nonce", message: "nonce not a decimal integer" };
    }
    if (nonce <= BigInt(record.last_nonce)) {
      this.replayCount++;
      return { ok: false, status: 409, code: "auth_replay", message: "nonce not increasing — replay rejected" };
    }

    const canonical = `${keyId}|${nonceStr}|${method}|${path}|${bodyHashHex(rawBody)}`;
    const expected = createHmac("sha256", this.decrypt(record.enc)).update(canonical).digest("hex");
    if (!timingSafeEqualHex(expected, signature)) {
      return { ok: false, status: 401, code: "auth_bad_signature", message: "signature mismatch" };
    }

    record.last_nonce = nonceStr;
    record.last_used = Date.now();
    record.role = configAdminOverride(this.config, keyId) ?? record.role;
    return { ok: true, record };
  }

  /* ── secret at rest ── */

  private encrypt(plaintext: string): string {
    const iv = randomBytes(12);
    const cipher = createCipheriv("aes-256-gcm", this.pepper, iv);
    const enc = Buffer.concat([cipher.update(plaintext, "utf8"), cipher.final()]);
    return Buffer.concat([iv, cipher.getAuthTag(), enc]).toString("base64");
  }

  private decrypt(payload: string): string {
    const buf = Buffer.from(payload, "base64");
    const iv = buf.subarray(0, 12);
    const tag = buf.subarray(12, 28);
    const data = buf.subarray(28);
    const decipher = createDecipheriv("aes-256-gcm", this.pepper, iv);
    decipher.setAuthTag(tag);
    return Buffer.concat([decipher.update(data), decipher.final()]).toString("utf8");
  }

  private loadPepper(): Buffer {
    const env = process.env.GATEWAY_SECRET_PEPPER;
    if (env) {
      const buf = Buffer.from(env, "base64");
      if (buf.length !== 32) throw new Error("GATEWAY_SECRET_PEPPER: expected 32 bytes (base64)");
      return buf;
    }
    if (this.config.dataDir) {
      const file = join(this.config.dataDir, "pepper.key");
      if (existsSync(file)) {
        const buf = Buffer.from(readFileSync(file, "utf8").trim(), "base64");
        if (buf.length === 32) return buf;
      }
      mkdirSync(this.config.dataDir, { recursive: true });
      const buf = randomBytes(32);
      writeFileSync(file, buf.toString("base64"), { mode: 0o600 });
      return buf;
    }
    // Ephemeral demo pepper — credentials vanish on restart (by design).
    log.warn("credentials.ephemeral", {
      detail: "no GATEWAY_SECRET_PEPPER / GATEWAY_DATA_DIR — provisioned keys do not survive restarts",
    });
    return randomBytes(32);
  }

  private loadPersisted(): void {
    if (!this.file || !existsSync(this.file)) return;
    try {
      const parsed = JSON.parse(readFileSync(this.file, "utf8")) as { keys?: ApiKeyRecord[] };
      for (const k of parsed.keys ?? []) {
        if (k?.key_id && k.enc) this.keys.set(k.key_id, k);
      }
      log.info("credentials.loaded", { count: this.keys.size });
    } catch (err) {
      log.error("credentials.load_failed", { error: String(err) });
    }
  }

  private persist(): void {
    if (!this.file) return;
    try {
      const dir = this.config.dataDir!;
      mkdirSync(dir, { recursive: true });
      const tmp = join(dir, `.credentials.${process.pid}.tmp`);
      writeFileSync(tmp, JSON.stringify({ keys: [...this.keys.values()] }, null, 2), { mode: 0o600 });
      renameSync(tmp, this.file);
    } catch (err) {
      log.error("credentials.persist_failed", { error: String(err) });
    }
  }
}

/** Env-forced admin role for specific key ids. */
function configAdminOverride(config: GatewayConfig, keyId: string): KeyRole | null {
  return config.adminKeyIds.includes(keyId) ? "admin" : null;
}

function timingSafeEqualHex(a: string, b: string): boolean {
  const ab = Buffer.from(a, "utf8");
  const bb = Buffer.from(b, "utf8");
  if (ab.length !== bb.length) return false;
  return timingSafeEqual(ab, bb);
}
