/**
 * GatewayRestClient — the signed REST surface of the venue gateway (G-25).
 *
 * Every call is HMAC-signed (`X-Api-Key` / `X-Nonce` / `X-Signature` /
 * `X-Timestamp` over the canonical `key|nonce|method|path|body_hash`) when an
 * AuthSession is attached; anonymous otherwise (gateway dev mode). Responses
 * use the wire codec — bigint money arrives as bigint.
 *
 * 429 responses feed the AuthSession backoff; 4xx/5xx throw typed errors.
 */

import { parseWire } from "@perp/types";
import type {
  BookUpdateTagged,
  Command,
  GovernanceAction,
  JournalEntry,
  MarketRow,
  Trade,
  TradePrintLike,
  VenueSnapshot,
} from "@perp/types";
import { AuthSession, bodyHashHex, type ApiCredentials, type SignedRequest } from "./auth";
import { endpointRestUrl, type RemoteEndpoint } from "./remote";

export interface GatewayStatsResponse {
  uptimeMs: number;
  connections: number;
  keys: number;
  commandsProcessed: number;
  messagesOut: number;
  rateLimited: number;
  rejectedReplays: number;
  seqDrops: number;
  venue: {
    events: number;
    trades: number;
    ticks: number;
    now: number;
    phase: string;
  };
}

export interface CommandReceipt {
  ok: boolean;
  events: JournalEntry[];
  trades: Trade[];
}

export class GatewayRestError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "GatewayRestError";
  }
}

export class GatewayRestClient {
  private auth: AuthSession | null = null;

  constructor(
    private endpoint: RemoteEndpoint = {},
    credentials?: ApiCredentials | null,
  ) {
    if (credentials) this.auth = new AuthSession(credentials);
  }

  attachCredentials(creds: ApiCredentials | null): void {
    this.auth = creds ? new AuthSession(creds) : null;
  }

  get keyId(): string | null {
    return this.auth?.keyId ?? null;
  }

  /* ── public (unsigned) ── */

  time(): Promise<{ now: number }> {
    return this.request("GET", "/v1/time");
  }

  provision(role?: "trader" | "admin"): Promise<{ key_id: string; secret: string; role: string }> {
    return this.request("POST", "/v1/auth/provision", { role: role ?? "trader" });
  }

  /* ── market data (signed when auth attached) ── */

  snapshot(): Promise<VenueSnapshot> {
    return this.request("GET", "/v1/venue/snapshot");
  }

  markets(): Promise<{ rows: MarketRow[] }> {
    return this.request("GET", "/v1/venue/markets");
  }

  book(symbol: string, depth?: number): Promise<{ update: BookUpdateTagged }> {
    return this.request("GET", `/v1/venue/book/${encodeURIComponent(symbol)}${depth ? `?depth=${depth}` : ""}`);
  }

  prints(sinceSeq?: number, limit?: number): Promise<{ prints: TradePrintLike[] }> {
    const params = new URLSearchParams();
    if (sinceSeq !== undefined) params.set("since", String(sinceSeq));
    if (limit !== undefined) params.set("limit", String(limit));
    const qs = params.toString();
    return this.request("GET", `/v1/venue/prints${qs ? `?${qs}` : ""}`);
  }

  account(id: number): Promise<{ view: VenueSnapshot["accounts"][number] }> {
    return this.request("GET", `/v1/venue/account/${id}`);
  }

  stats(): Promise<GatewayStatsResponse> {
    return this.request("GET", "/v1/stats");
  }

  /* ── write paths (signed when auth attached; role-gated server-side) ── */

  command(cmd: Command): Promise<CommandReceipt> {
    return this.request("POST", "/v1/venue/command", { command: cmd });
  }

  governance(action: GovernanceAction): Promise<{ ok: boolean; error: string | null }> {
    return this.request("POST", "/v1/venue/governance", { action });
  }

  withdrawal(op: {
    op: "request" | "approve" | "cancel" | "settle_due" | "force_queue";
    id?: number;
    subaccount?: number;
    amount_quote_minor?: bigint;
    destination?: string;
  }): Promise<CommandReceipt> {
    return this.request("POST", "/v1/venue/withdrawal", op);
  }

  porBuild(): Promise<{ ok: boolean }> {
    return this.request("POST", "/v1/venue/por_build");
  }

  /* ── engine ── */

  private async request<T>(method: string, pathWithQuery: string, body?: unknown): Promise<T> {
    const [path] = pathWithQuery.split("?");
    const bodyStr = body === undefined ? "" : JSON.stringify(body);
    const headers: Record<string, string> = {
      "Content-Type": "application/json",
    };
    if (this.auth) {
      const signed: SignedRequest = await this.auth.sign(method, path, bodyStr);
      Object.assign(headers, signed);
    }
    const res = await fetch(endpointRestUrl(pathWithQuery, this.endpoint), {
      method,
      headers,
      body: bodyStr || undefined,
    });

    if (res.status === 429) {
      const retryAfter = Number(res.headers.get("Retry-After") ?? "1");
      this.auth?.rateLimited(Math.max(250, (Number.isFinite(retryAfter) ? retryAfter : 1) * 1000));
    }
    if (!res.ok) {
      let code = "gateway_error";
      let message = `HTTP ${res.status}`;
      try {
        const err = (await res.json()) as { error?: string; code?: string };
        code = err.code ?? code;
        message = err.error ?? message;
      } catch {
        /* non-JSON error body */
      }
      throw new GatewayRestError(res.status, code, message);
    }
    const text = await res.text();
    return parseWire<T>(text);
  }
}

/** Convenience: build the canonical body hash exactly as the gateway does. */
export const restBodyHash = bodyHashHex;
