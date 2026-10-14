/**
 * Human-readable text for engine reject reasons, HTTP error codes and
 * cancel reasons, per docs/0DTE_API.md §2 / §3.
 */
import type { CancelReason, RejectReason, Scope } from './types';

export const REJECT_HUMAN: Record<RejectReason, string> = {
  UnknownSymbol: 'Symbol is not listed (unknown or already expired)',
  UnknownAccount: 'Account does not exist',
  PriceBand: 'Price outside the allowed band for this symbol',
  PostOnlyCross: 'Post-only order would cross the book',
  ReduceOnlyWouldIncrease: 'Reduce-only would increase the position',
  InsufficientFunds: 'Insufficient available balance',
  PositionLimit: 'Position limit reached',
  RateLimit: 'Order rate limit — slow down',
  Malformed: 'Order rejected as malformed',
  NotContinuous: 'Market not in continuous phase (auction or expired)',
  FokDepth: 'Insufficient depth to fill FOK order',
  UnknownOrder: 'Order already filled or canceled',
  Halted: 'Underlying is halted',
  DeltaLimit: 'Portfolio delta limit exceeded',
  TooManyOrders: 'Too many open orders',
  QtyTooLarge: 'Quantity above maximum (10000)',
  NotOwner: 'Order belongs to another account',
};

export const ERROR_HUMAN: Record<string, string> = {
  BadSignature: 'Signature rejected — check the API secret',
  StaleTimestamp: 'Timestamp outside 5s window — device clock drift',
  BadNonce: 'Nonce race detected — retrying',
  UnknownKey: 'API key not recognized by the engine',
  ScopeDenied: 'Key scope insufficient for this action',
  Malformed: 'Request rejected as malformed',
  NotFound: 'Not found',
  UnknownOrder: 'Order already gone',
  TooLarge: 'Request too large',
  RateLimited: 'Rate limited — wait and retry',
};

export const CANCEL_HUMAN: Record<CancelReason, string> = {
  user: 'canceled by user',
  stp: 'self-trade prevention',
  expiry: 'session expiry',
  auction: 'auction transition',
  oco: 'linked OCO order',
  replace: 'replaced',
};

export const SCOPE_DESC: Record<Scope, string> = {
  read: 'read-only market data',
  trade: 'order entry + market data',
  admin: 'full venue control',
};

export function describeError(e: unknown): string {
  const name = (e as { name?: string } | null)?.name;
  const msg = e instanceof Error ? e.message : String(e);
  if (name === 'NetworkError') return msg || 'engine unreachable';
  if (name === 'ApiError') {
    const err = e as { code?: string; msg?: string; status?: number };
    const human = err.code ? ERROR_HUMAN[err.code] : undefined;
    return human ?? err.msg ?? `HTTP ${err.status ?? 'error'}`;
  }
  return msg;
}
