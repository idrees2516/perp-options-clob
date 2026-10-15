/**
 * Session persistence — sessionStorage ONLY (never localStorage), per the
 * security requirements: the API secret must never outlive the tab and must
 * never be logged or rendered in cleartext.
 */
import type { Scope } from './types';

export interface SessionRecord {
  keyId: string;
  secretHex: string;
  baseUrl: string;
  account: number;
  tier: string;
  scope: Scope;
}

const SESSION_KEY = 'nsvenue.session';
const NONCE_KEY_PREFIX = 'nsvenue.nonce.';

function ss(): Storage | null {
  try {
    if (typeof window === 'undefined') return null;
    return window.sessionStorage;
  } catch {
    return null;
  }
}

export function loadSession(): SessionRecord | null {
  const store = ss();
  if (!store) return null;
  try {
    const raw = store.getItem(SESSION_KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as Partial<SessionRecord>;
    if (
      typeof v.keyId === 'string' &&
      typeof v.secretHex === 'string' &&
      typeof v.baseUrl === 'string' &&
      typeof v.account === 'number' &&
      typeof v.tier === 'string' &&
      (v.scope === 'read' || v.scope === 'trade' || v.scope === 'admin')
    ) {
      return { keyId: v.keyId, secretHex: v.secretHex, baseUrl: v.baseUrl, account: v.account, tier: v.tier, scope: v.scope };
    }
    return null;
  } catch {
    return null;
  }
}

export function saveSession(rec: SessionRecord): void {
  const store = ss();
  if (!store) return;
  store.setItem(SESSION_KEY, JSON.stringify(rec));
}

export function clearSession(): void {
  const store = ss();
  if (!store) return;
  // Also drop the per-key nonce so a re-login re-seeds fresh.
  try {
    const raw = store.getItem(SESSION_KEY);
    if (raw) {
      const v = JSON.parse(raw) as { keyId?: string };
      if (typeof v.keyId === 'string') store.removeItem(NONCE_KEY_PREFIX + v.keyId);
    }
  } catch {
    /* ignore */
  }
  store.removeItem(SESSION_KEY);
}

export function updateSessionScope(scope: Scope): void {
  const cur = loadSession();
  if (!cur || cur.scope === scope) return;
  saveSession({ ...cur, scope });
}

// --- nonce storage (shared by REST client and WS manager, per key) ---------

export function loadNonce(keyId: string): string | null {
  return ss()?.getItem(NONCE_KEY_PREFIX + keyId) ?? null;
}

export function saveNonce(keyId: string, decimalValue: string): void {
  ss()?.setItem(NONCE_KEY_PREFIX + keyId, decimalValue);
}

// --- masking ---------------------------------------------------------------

/** Masked display form of a secret — never render the raw secret. */
export function maskSecret(secretHex: string): string {
  return '•'.repeat(Math.min(12, secretHex.length)) + ` (${secretHex.length} hex)`;
}

export function isHex(s: string, len: number): boolean {
  return new RegExp(`^[0-9a-f]{${len}}$`).test(s);
}

/** Normalize a user-entered base URL; null when unusable. */
export function normalizeBaseUrl(input: string): string | null {
  let s = input.trim();
  // Empty base URL ⇒ same-origin gateway mode: requests carry an
  // XTransformPort routing parameter and are proxied to the venue by the
  // reverse proxy that serves this terminal (see lib/api.ts).
  if (!s) return '';
  if (!/^https?:\/\//i.test(s)) s = `http://${s}`;
  try {
    const u = new URL(s);
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
    return u.origin + u.pathname.replace(/\/+$/, '');
  } catch {
    return null;
  }
}
