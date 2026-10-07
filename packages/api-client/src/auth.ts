/**
 * HMAC-SHA256 request signing — the G-25 gateway auth contract.
 *
 * Canonical string: `key_id|nonce|method|path|body_hash` signed with the
 * secret. Nonces are strictly increasing per key; replays are rejected.
 * Rate limiting is a token bucket — clients back off on 429-equivalents.
 */

/** FNV-1a based body hash — canonical G-25 form, shared via @perp/types. */
export { bodyHashHex } from "@perp/types";
import { bodyHashHex } from "@perp/types";

/** Web Crypto HMAC-SHA256, hex output. SSR-safe (lazy import). */
export async function hmacSha256Hex(secret: string, message: string): Promise<string> {
  const crypto = globalThis.crypto;
  if (!crypto?.subtle) throw new Error("WebCrypto unavailable in this environment");
  const enc = new TextEncoder();
  const key = await crypto.subtle.importKey(
    "raw",
    enc.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign("HMAC", key, enc.encode(message));
  return Array.from(new Uint8Array(sig))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

export interface ApiCredentials {
  key_id: string;
  secret: string;
}

/** Signed request headers for the venue gateway. */
export interface SignedRequest {
  "X-Api-Key": string;
  "X-Nonce": string;
  "X-Signature": string;
  "X-Timestamp": string;
}

/** Nonce manager: strictly increasing, resilient to clock weirdness. */
export class NonceManager {
  private last: bigint;

  constructor(start?: bigint) {
    this.last = start ?? BigInt(Date.now()) * 1000n;
  }

  next(): bigint {
    const candidate = BigInt(Date.now()) * 1000n;
    this.last = candidate > this.last ? candidate : this.last + 1n;
    return this.last;
  }

  current(): bigint {
    return this.last;
  }
}

/** Auth session: signs requests, tracks nonce, handles rate-limit backoff. */
export class AuthSession {
  private nonces = new NonceManager();
  private backoffUntil = 0;

  constructor(private creds: ApiCredentials) {}

  get keyId(): string {
    return this.creds.key_id;
  }

  /** Sign a request. Throws if currently backing off from a rate limit. */
  async sign(method: string, path: string, body: string): Promise<SignedRequest> {
    if (Date.now() < this.backoffUntil) {
      throw new Error(`rate-limit backoff until ${new Date(this.backoffUntil).toISOString()}`);
    }
    const nonce = this.nonces.next();
    const canonical = `${this.creds.key_id}|${nonce}|${method}|${path}|${bodyHashHex(body)}`;
    const signature = await hmacSha256Hex(this.creds.secret, canonical);
    return {
      "X-Api-Key": this.creds.key_id,
      "X-Nonce": nonce.toString(),
      "X-Signature": signature,
      "X-Timestamp": Date.now().toString(),
    };
  }

  /** Call this when the venue signals rate limiting (token bucket drained). */
  rateLimited(retryAfterMs = 1000): void {
    this.backoffUntil = Math.max(this.backoffUntil, Date.now() + retryAfterMs);
  }
}

/** Credential storage: WebCrypto AES-GCM at rest (production path). */
export async function encryptSecret(secret: string, passphrase: string): Promise<string> {
  const crypto = globalThis.crypto;
  const enc = new TextEncoder();
  const keyMaterial = await crypto.subtle.importKey("raw", enc.encode(passphrase), "PBKDF2", false, ["deriveKey"]);
  const salt = crypto.getRandomValues(new Uint8Array(16));
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const key = await crypto.subtle.deriveKey(
    { name: "PBKDF2", salt, iterations: 100_000, hash: "SHA-256" },
    keyMaterial,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt"],
  );
  const cipher = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, enc.encode(secret));
  const bytes = new Uint8Array(16 + 12 + cipher.byteLength);
  bytes.set(salt, 0);
  bytes.set(iv, 16);
  bytes.set(new Uint8Array(cipher), 28);
  return Array.from(bytes)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

export async function decryptSecret(encrypted: string, passphrase: string): Promise<string> {
  const crypto = globalThis.crypto;
  const enc = new TextEncoder();
  const bytes = new Uint8Array(encrypted.length / 2);
  for (let i = 0; i < bytes.length; i++) bytes[i] = parseInt(encrypted.slice(i * 2, i * 2 + 2), 16);
  const salt = bytes.slice(0, 16);
  const iv = bytes.slice(16, 28);
  const data = bytes.slice(28);
  const keyMaterial = await crypto.subtle.importKey("raw", enc.encode(passphrase), "PBKDF2", false, ["deriveKey"]);
  const key = await crypto.subtle.deriveKey(
    { name: "PBKDF2", salt, iterations: 100_000, hash: "SHA-256" },
    keyMaterial,
    { name: "AES-GCM", length: 256 },
    false,
    ["decrypt"],
  );
  const plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv }, key, data);
  return new TextDecoder().decode(plain);
}
