/**
 * @perp/api-client — venue connection layer.
 *
 *  client   — VenueClient facade (sim worker / remote gateway, one contract)
 *  session  — market-data sessions: snapshot → delta, seq gap detection, resync
 *  auth     — G-25 signing: canonical `key|nonce|method|path|body_hash`, HMAC-SHA256,
 *             strictly-increasing nonces, rate-limit backoff, encrypted secret storage
 *  worker   — the embedded deterministic venue (demo mode)
 */

export * from "./client";
export * from "./session";
export * from "./auth";
export type { BookState } from "./session";
