/**
 * Transport contract between the UI and the venue (Web Worker in sim mode,
 * socket.io gateway in production). Messages are structured-clone friendly —
 * bigint passes natively.
 */

import type { Command } from "./command";
import type { JournalEntry, Trade } from "./event";
import type { GovernanceAction } from "./protocol";
import type { Level } from "./market";
import type { MarketRow, VenueSnapshot } from "./venue";

/** ─────────── Main → Venue ─────────── */

export type VenueControl =
  | { type: "connect"; seed?: number; speed?: number }
  | { type: "reset"; seed?: number }
  | { type: "set_speed"; speed: number }
  | { type: "pause" }
  | { type: "resume" }
  | { type: "step"; ms?: number }
  | { type: "subscribe"; symbols: string[] }
  | { type: "command"; command: Command }
  | { type: "governance"; action: GovernanceAction }
  | {
      type: "withdrawal";
      op: "request" | "approve" | "cancel" | "settle_due" | "force_queue";
      id?: number;
      subaccount?: number;
      amount_quote_minor?: bigint;
      destination?: string;
    }
  | { type: "por_build" }
  | { type: "oracle_inject"; provider: string; price_quote_minor: bigint }
  | { type: "oracle_toggle_quarantine"; provider: string }
  | { type: "request_snapshot" };

/** ─────────── Venue → Main ─────────── */

/** Print shape for the tape. */
export interface TradePrintLike {
  seq: number;
  ts: number;
  symbol: string;
  price_ticks: number;
  qty_lots: number;
  maker_side: "bid" | "ask";
  notional_quote_minor: bigint;
}

export type VenueMessage =
  | { channel: "bootstrap"; snapshot: VenueSnapshot }
  | { channel: "snapshot"; snapshot: VenueSnapshot }
  | { channel: "journal"; entries: JournalEntry[] }
  | {
      channel: "books";
      updates: Record<string, BookUpdateTagged>;
    }
  | { channel: "prints"; prints: TradePrintLike[] }
  | {
      channel: "account";
      view: AccountDelta;
    }
  | { channel: "markets"; rows: MarketRow[] }
  | { channel: "trade_fill"; trade: Trade; taker: boolean; maker: boolean }
  | { channel: "error"; message: string; command?: Command }
  | { channel: "pong"; id: number; ts: number };

/** Incremental account delta — hot path, replaces full AccountView per tick. */
export interface AccountDelta {
  id: number;
  cash_quote_minor: bigint;
  equity_quote_minor: bigint;
  maintenance_quote_minor: bigint;
  initial_quote_minor: bigint;
  order_margin_quote_minor: bigint;
  health: "healthy" | "restricted" | "liquidation";
  fees_paid_quote_minor: bigint;
  funding_received_quote_minor: bigint;
}

/** Market-data wire frames — snapshots are tagged explicitly. */
export type BookUpdateTagged =
  | { kind: "snapshot"; seq: number; bids: Level[]; asks: Level[] }
  | { kind: "delta"; seq: number; bids: Level[]; asks: Level[] };

/** Market-data session state — mirrors poc-api session.rs. */
export interface MarketDataSessionState {
  seq: number;
  desynced: boolean;
  dropped: number;
}

/**
 * Gap-detection rule from the protocol: a delta whose seq is not last+1 marks
 * the session desynced; it must re-apply a snapshot before consuming further deltas.
 */
export function canApplyDelta(
  session: Pick<MarketDataSessionState, "seq" | "desynced">,
  deltaSeq: number,
): boolean {
  if (session.desynced) return false;
  return deltaSeq === session.seq + 1;
}
