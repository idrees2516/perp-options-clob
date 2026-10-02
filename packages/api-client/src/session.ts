/**
 * Market-data session — the client half of the G-27 contract.
 * Snapshot establishes seq; deltas apply by seq+1; gaps desync and demand
 * a fresh snapshot before further deltas are consumed.
 */

import type { BookUpdateTagged, Level } from "@perp/types";

export type BookState = {
  seq: number;
  desynced: boolean;
  dropped: number;
  bids: Level[];
  asks: Level[];
};

export class MarketDataSession {
  private state = new Map<string, BookState>();
  private onResync: (symbol: string) => void;

  constructor(onResync: (symbol: string) => void) {
    this.onResync = onResync;
  }

  of(symbol: string): BookState | undefined {
    return this.state.get(symbol);
  }

  /** Best bid/ask for a symbol (null when one/both sides empty). */
  bbo(symbol: string): { bid: Level | null; ask: Level | null } {
    const s = this.state.get(symbol);
    return {
      bid: s?.bids.length ? s.bids[0]! : null,
      ask: s?.asks.length ? s.asks[0]! : null,
    };
  }

  apply(update: BookUpdateTagged, symbol: string): void {
    if (update.kind === "snapshot") {
      this.state.set(symbol, {
        seq: update.seq,
        desynced: false,
        dropped: 0,
        bids: update.bids.slice(),
        asks: update.asks.slice(),
      });
      return;
    }
    // delta
    const s = this.state.get(symbol);
    if (!s || s.desynced) {
      if (!s) {
        // Never saw a snapshot for this symbol — request one.
        this.onResync(symbol);
      }
      return;
    }
    if (update.seq !== s.seq + 1) {
      s.desynced = true;
      s.dropped += Math.max(0, update.seq - s.seq - 1);
      this.onResync(symbol);
      return;
    }
    s.seq = update.seq;
    applyLevels(s.bids, update.bids, true);
    applyLevels(s.asks, update.asks, false);
  }

  reset(symbol?: string): void {
    if (symbol) this.state.delete(symbol);
    else this.state.clear();
  }
}

/** Levels REPLACE; zero lots removes. Bids sorted desc, asks asc. */
function applyLevels(current: Level[], delta: Level[], bid: boolean): void {
  if (delta.length === 0) return;
  // Batch replace via map for O(n + m).
  const map = new Map<number, number>();
  for (const l of current) map.set(l.price_ticks, l.lots);
  for (const l of delta) {
    if (l.lots <= 0) map.delete(l.price_ticks);
    else map.set(l.price_ticks, l.lots);
  }
  const merged = [...map.entries()].map(([price_ticks, lots]) => ({ price_ticks, lots }));
  merged.sort((a, b) => (bid ? b.price_ticks - a.price_ticks : a.price_ticks - b.price_ticks));
  current.length = 0;
  current.push(...merged);
}
