/**
 * Gateway configuration + credential storage (browser).
 *
 * Model: connection prefs (mode, endpoint) persist in localStorage; the API
 * secret lives ONLY in sessionStorage — it dies with the tab, never written
 * to disk. A user who wants persistence re-authenticates per session
 * (paste-once or re-provision), which is the right trade-off for a trading
 * terminal. SSR-safe: every accessor bails on non-browser environments.
 */

import type { ApiCredentials } from "./auth";
import type { RemoteEndpoint } from "./remote";

export type TransportMode = "sim" | "remote";

export interface GatewayConfig {
  mode: TransportMode;
  endpoint: RemoteEndpoint;
  credentials: ApiCredentials | null;
}

const LS_MODE = "perp.gateway.mode";
const LS_ORIGIN = "perp.gateway.origin";
const LS_PORT = "perp.gateway.port";
const SS_CREDS = "perp.gateway.creds";

function safeLocal(): Storage | null {
  try {
    return typeof window !== "undefined" ? window.localStorage : null;
  } catch {
    return null; // privacy mode / disabled storage
  }
}

function safeSession(): Storage | null {
  try {
    return typeof window !== "undefined" ? window.sessionStorage : null;
  } catch {
    return null;
  }
}

export function loadGatewayConfig(defaultPort = 3031): GatewayConfig {
  const ls = safeLocal();
  const ss = safeSession();
  const envOrigin = process.env.NEXT_PUBLIC_VENUE_GATEWAY_URL?.trim() || "";
  const envPort = Number(process.env.NEXT_PUBLIC_VENUE_GATEWAY_PORT);
  const mode = (ls?.getItem(LS_MODE) as TransportMode | null) ?? (envOrigin ? "remote" : "sim");
  const origin = ls?.getItem(LS_ORIGIN) ?? envOrigin;
  const port = Number(ls?.getItem(LS_PORT) ?? (Number.isFinite(envPort) ? envPort : defaultPort));
  let credentials: ApiCredentials | null = null;
  const raw = ss?.getItem(SS_CREDS);
  if (raw) {
    try {
      const parsed = JSON.parse(raw) as ApiCredentials;
      if (parsed.key_id && parsed.secret) credentials = parsed;
    } catch {
      ss?.removeItem(SS_CREDS);
    }
  }
  return {
    mode,
    endpoint: {
      origin: origin || undefined,
      port: Number.isFinite(port) ? port : defaultPort,
    },
    credentials,
  };
}

export function saveTransportMode(mode: TransportMode): void {
  safeLocal()?.setItem(LS_MODE, mode);
}

export function saveEndpoint(endpoint: RemoteEndpoint): void {
  const ls = safeLocal();
  if (!ls) return;
  ls.setItem(LS_ORIGIN, endpoint.origin ?? "");
  ls.setItem(LS_PORT, String(endpoint.port ?? 3031));
}

/** Store credentials for this tab session only. */
export function saveCredentials(creds: ApiCredentials | null): void {
  const ss = safeSession();
  if (!ss) return;
  if (creds) ss.setItem(SS_CREDS, JSON.stringify(creds));
  else ss.removeItem(SS_CREDS);
}

export function hasCredentials(): boolean {
  return loadGatewayConfig().credentials !== null;
}

/** Full reset of gateway prefs + session credentials. */
export function clearGatewayConfig(): void {
  safeLocal()?.removeItem(LS_MODE);
  safeLocal()?.removeItem(LS_ORIGIN);
  safeLocal()?.removeItem(LS_PORT);
  safeSession()?.removeItem(SS_CREDS);
}
