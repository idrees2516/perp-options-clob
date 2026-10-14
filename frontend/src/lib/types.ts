/**
 * Strict types for every message of docs/0DTE_API.md (v1).
 * Field names mirror the contract EXACTLY — do not rename.
 * All money/prices are integer ticks; quantities are integer contracts.
 */

export type Scope = 'read' | 'trade' | 'admin';
export type Side = 'buy' | 'sell';
export type OrderKind = 'limit' | 'market';
export type Tif = 'gtc' | 'ioc' | 'fok';
export type OptionKind = 'call' | 'put';
export type Phase = 'continuous' | 'auction' | 'expired';
export type Aggressor = 'buy' | 'sell' | 'auction';
export type WsChannel = 'bbo' | 'trade' | 'depth' | 'phase' | 'mark' | 'underlying';
export type CancelReason = 'user' | 'stp' | 'expiry' | 'auction' | 'oco' | 'replace';
export type FillRole = 'taker' | 'maker';

/** Engine reject `reason` strings — contract §2.3. */
export type RejectReason =
  | 'UnknownSymbol'
  | 'UnknownAccount'
  | 'PriceBand'
  | 'PostOnlyCross'
  | 'ReduceOnlyWouldIncrease'
  | 'InsufficientFunds'
  | 'PositionLimit'
  | 'RateLimit'
  | 'Malformed'
  | 'NotContinuous'
  | 'FokDepth'
  | 'UnknownOrder'
  | 'Halted'
  | 'DeltaLimit'
  | 'TooManyOrders'
  | 'QtyTooLarge'
  | 'NotOwner';

// ---------------------------------------------------------------------------
// REST responses
// ---------------------------------------------------------------------------

export interface TimeResp {
  ms: number;
  nanos: number;
}

export interface HealthResp {
  ok: boolean;
  uptime_ms: number;
  shards: number;
  draining: boolean;
}

export interface UnderlyingSnap {
  underlying: number;
  mark: number;
  ref: number;
  fast: boolean;
  halted: boolean;
}

export interface SymbolSnap {
  symbol: number;
  underlying: number;
  kind: OptionKind;
  strike: number;
  expiry_min: number;
  expiry_ms: number;
  phase: Phase;
  mark: number;
  last: number;
  vol: number;
  bid: number;
  bid_qty: number;
  ask: number;
  ask_qty: number;
  open: number;
  high: number;
  low: number;
  n_trades: number;
}

export interface SymbolsResp {
  underlyings: UnderlyingSnap[];
  symbols: SymbolSnap[];
}

/** [price_ticks, qty] */
export type PriceLevel = [number, number];

export interface BookResp {
  symbol: number;
  bids: PriceLevel[];
  asks: PriceLevel[];
}

export interface TapeTrade {
  price: number;
  qty: number;
  aggressor: Aggressor;
  ms: number;
}

export interface TapeResp {
  symbol: number;
  trades: TapeTrade[];
}

export interface Balance {
  underlying: number;
  cash: number;
  hold: number;
  margin_required: number;
  margin_reserved: number;
  available: number;
  fees_paid: number;
  rebates_recv: number;
}

export interface Position {
  symbol: number;
  /** signed: + long / - short */
  qty: number;
  margin: number;
  delta_notional: number;
}

export interface OpenOrder {
  order: number;
  symbol: number;
  side: Side;
  price: number;
  qty: number;
  leaves: number;
  tif: Tif;
  kind: OrderKind;
  post_only: boolean;
  ms: number;
}

export interface AccountResp {
  account: number;
  tier: string;
  balances: Balance[];
  positions: Position[];
  orders: OpenOrder[];
}

// ---------------------------------------------------------------------------
// Order entry (contract §2.3) — field set is EXACTLY the contract's; unknown
// fields are rejected server-side, so nothing extra is ever sent.
// ---------------------------------------------------------------------------

export interface OrderRequest {
  symbol: number;
  side: Side;
  qty: number;
  price: number;
  tif: Tif;
  kind: OrderKind;
  post_only: boolean;
  reduce_only: boolean;
}

export interface OrderFill {
  price: number;
  qty: number;
  fee: number;
  rebate: number;
}

export type PlaceResult =
  | { ok: true; order_id: number; leaves: number; fills: OrderFill[] }
  | { ok: false; code: number; reason: RejectReason };

export interface CancelResp {
  ok: true;
  order_id: number;
  leaves: number;
}

export interface ReplaceRequest {
  order: number;
  symbol: number;
  price: number;
  qty: number;
}

export interface BatchResp {
  results: PlaceResult[];
}

// ---------------------------------------------------------------------------
// Admin (contract §2.4)
// ---------------------------------------------------------------------------

export interface KeyMintRequest {
  account: number;
  scope: Scope;
  tier: string;
}

export interface KeyMintResp {
  /** shown once — never persisted by this terminal */
  key_id: string;
  secret: string;
  account: number;
  scope: Scope;
}

export interface AdminOk {
  ok: true;
}

export interface FundingRequest {
  account: number;
  underlying: number;
  /** integer ticks, u64 */
  amount: number;
}

export interface OracleRequest {
  underlying: number;
  price: number;
}

export interface SessionRequest {
  underlying: number;
  expiry_ms: number;
  strikes: number[];
  kinds: OptionKind[];
}

export interface SessionResp {
  ok: true;
  symbols: number[];
}

export interface ShardAudit {
  shard: number;
  underlying: number;
  live_accounts: number;
  open_positions: number;
  symbols: number;
  book_orders: number;
  house: number;
  deposits: number;
  sum_cash: number;
  state_hash: string;
}

export interface GatewayAudit {
  conns: number;
  ws: number;
  rx: number;
  tx: number;
  auth_fails: number;
  rate_limited: number;
}

export interface AuditResp {
  conservation: boolean;
  shards: ShardAudit[];
  gateway: GatewayAudit;
}

export interface ShardMetrics extends ShardAudit {
  orders: number;
  rejects: number;
  trades: number;
  volume: number;
  premium: number;
  fees: number;
  cancels: number;
  cmds: number;
  expiries: number;
  auctions: number;
  queue_depth: number;
  journal_bytes: number;
  tps_ema: number;
}

export interface GatewayMetrics extends GatewayAudit {
  tps: number;
}

export interface MetricsResp {
  conservation: boolean;
  shards: ShardMetrics[];
  gateway: GatewayMetrics;
}

// ---------------------------------------------------------------------------
// WebSocket stream (contract §3) — server -> client, one JSON object per frame
// ---------------------------------------------------------------------------

export type WsServerMsg =
  | { t: 'hello'; account: number; scope: Scope; ms: number }
  | { t: 'bbo'; symbol: number; bid: number; bid_qty: number; ask: number; ask_qty: number }
  | { t: 'trade'; symbol: number; price: number; qty: number; aggressor: Aggressor; ms: number }
  | { t: 'depth'; symbol: number; bids: PriceLevel[]; asks: PriceLevel[] }
  | { t: 'phase'; symbol: number; phase: Phase }
  | { t: 'mark'; symbol: number; price: number }
  | { t: 'underlying'; underlying: number; price: number; ref: number; fast: boolean; halted: boolean }
  | { t: 'halt'; underlying: number; level: number }
  | { t: 'resumed'; underlying: number }
  | { t: 'ack'; order: number; symbol: number; leaves: number; ms: number }
  | { t: 'reject'; order: number; symbol: number; code: number; reason: RejectReason }
  | { t: 'canceled'; order: number; symbol: number; leaves: number; reason: CancelReason; ms: number }
  | { t: 'fill'; symbol: number; price: number; qty: number; order: number; role: FillRole; fee: number; rebate: number; ms: number }
  | { t: 'exercised'; symbol: number; qty: number; intrinsic: number; cash_flow: number; fee: number; ms: number }
  | {
      t: 'balance';
      underlying: number;
      cash: number;
      hold: number;
      margin_required: number;
      margin_reserved: number;
      available: number;
    }
  | { t: 'pong' };

/** client -> server */
export type WsClientMsg =
  | { op: 'sub' | 'unsub'; channel: Exclude<WsChannel, 'underlying'>; symbol: number }
  | { op: 'sub' | 'unsub'; channel: 'underlying'; underlying: number }
  | { op: 'ping' };

const WS_TAGS = new Set<string>([
  'hello',
  'bbo',
  'trade',
  'depth',
  'phase',
  'mark',
  'underlying',
  'halt',
  'resumed',
  'ack',
  'reject',
  'canceled',
  'fill',
  'exercised',
  'balance',
  'pong',
]);

/** Light runtime guard: unknown/invalid frames are dropped, never crash a panel. */
export function asWsServerMsg(raw: unknown): WsServerMsg | null {
  if (typeof raw === 'object' && raw !== null && 't' in raw) {
    const tag = (raw as { t: unknown }).t;
    if (typeof tag === 'string' && WS_TAGS.has(tag)) {
      return raw as WsServerMsg;
    }
  }
  return null;
}
