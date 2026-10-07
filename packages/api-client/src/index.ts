/**
 * @perp/api-client — venue connection layer.
 *
 *  client       — VenueClient facade (sim worker / remote gateway, one contract)
 *  session      — market-data sessions: snapshot → delta, seq gap detection, resync
 *  auth         — G-25 signing: canonical `key|nonce|method|path|body_hash`, HMAC-SHA256,
 *                 strictly-increasing nonces, rate-limit backoff, encrypted secret storage
 *  remote       — RemoteConnection: production socket.io transport (G-25 handshake,
 *                 wire codec, heartbeat + RTT, reconnection, XTransformPort proxy mode)
 *  rest         — GatewayRestClient: signed REST surface of the gateway
 *  credentials  — browser connection prefs (localStorage) + session-scoped secrets
 *  worker       — the embedded deterministic venue (demo mode)
 */

export * from "./client";
export * from "./session";
export * from "./auth";
export * from "./remote";
export * from "./rest";
export * from "./credentials";
export type { BookState } from "./session";
