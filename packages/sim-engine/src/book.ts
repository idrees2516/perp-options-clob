/**
 * The channel orderbook — price-level ladders with FIFO queues.
 *
 * Mirrors the design commitments of `poc-orderbook`:
 *  - price-time priority, FIFO within a level
 *  - iceberg display affects the *view*, never the matching
 *  - `match` produces fills without applying account effects (pure match,
 *    separate apply — the flood-rs / dYdX v4 discipline)
 *  - STP policies resolved during the walk
 *  - L2 depth aggregated on demand; delta emission tracks dirty levels
 */

import type { Level, SelfTradePrevention, Side, Symbol } from "@perp/types";
import type { SimOrder } from "./order";

export interface Fill {
  maker_order_id: number;
  maker_subaccount: number;
  price_ticks: number;
  qty_lots: number;
  maker_is_bid: boolean;
}

export interface MatchResult {
  fills: Fill[];
  /** Taker remainder was stopped by STP / reduce-only closure — cancel it. */
  stop_taker: boolean;
  stp_canceled_makers: number[];
}

/** A resting order handle with view-layer fields. */
export class Book {
  readonly symbol: Symbol;
  /** tick → FIFO of resting orders. */
  private bids = new Map<number, SimOrder[]>();
  private asks = new Map<number, SimOrder[]>();
  /** Sorted tick ladders (desc bids, asc asks) — the channel skeleton. */
  private bidTicks: number[] = [];
  private askTicks: number[] = [];
  /** Levels changed since the last delta take. */
  private dirtyBids = new Set<number>();
  private dirtyAsks = new Set<number>();
  private seq = 0;

  constructor(symbol: Symbol) {
    this.symbol = symbol;
  }

  get bookSeq(): number {
    return this.seq;
  }

  /* ─────────── placement ─────────── */

  insert(order: SimOrder): void {
    if (order.price_ticks == null) return;
    const map = order.side === "bid" ? this.bids : this.asks;
    const ladder = order.side === "bid" ? this.bidTicks : this.askTicks;
    const tick = order.price_ticks;
    let level = map.get(tick);
    if (!level) {
      level = [];
      map.set(tick, level);
      insertSorted(ladder, tick, order.side === "bid");
    }
    level.push(order);
    this.markDirty(order.side, tick);
    this.bump();
  }

  remove(order: SimOrder): void {
    if (order.price_ticks == null) return;
    const map = order.side === "bid" ? this.bids : this.asks;
    const ladder = order.side === "bid" ? this.bidTicks : this.askTicks;
    const tick = order.price_ticks;
    const level = map.get(tick);
    if (!level) return;
    const idx = level.indexOf(order);
    if (idx >= 0) level.splice(idx, 1);
    if (level.length === 0) {
      map.delete(tick);
      const li = ladder.indexOf(tick);
      if (li >= 0) ladder.splice(li, 1);
    }
    this.markDirty(order.side, tick);
    this.bump();
  }

  hasResting(orderId: number): boolean {
    for (const level of this.bids.values()) if (level.some((o) => o.id === orderId)) return true;
    for (const level of this.asks.values()) if (level.some((o) => o.id === orderId)) return true;
    return false;
  }

  openOrderCount(): number {
    let n = 0;
    for (const l of this.bids.values()) n += l.length;
    for (const l of this.asks.values()) n += l.length;
    return n;
  }

  restingOrders(): SimOrder[] {
    const out: SimOrder[] = [];
    for (const l of this.bids.values()) out.push(...l);
    for (const l of this.asks.values()) out.push(...l);
    return out;
  }

  /* ─────────── matching (pure vs accounts) ─────────── */

  bestBid(): number | null {
    return this.bidTicks.length ? this.bidTicks[0]! : null;
  }

  bestAsk(): number | null {
    return this.askTicks.length ? this.askTicks[0]! : null;
  }

  /**
   * Walk the opposite ladder crossing `limitTicks` (null = market, cross all).
   * Mutates maker orders (fills them) but leaves account effects to the engine.
   */
  match(
    takerSide: Side,
    limitTicks: number | null,
    qtyLots: number,
    takerSubaccount: number,
    stp: SelfTradePrevention,
  ): MatchResult {
    const fills: Fill[] = [];
    const stpCanceledMakers: number[] = [];
    let remaining = qtyLots;
    let stopTaker = false;

    const crossing = takerSide === "bid"
      ? (t: number) => limitTicks == null || t <= limitTicks
      : (t: number) => limitTicks == null || t >= limitTicks;

    const ladder = takerSide === "bid" ? this.askTicks : this.bidTicks;
    const map = takerSide === "bid" ? this.asks : this.bids;

    while (remaining > 0 && !stopTaker) {
      const top = ladder[0];
      if (top == null || !crossing(top)) break;
      const level = map.get(top)!;

      while (level.length > 0 && remaining > 0 && !stopTaker) {
        const maker = level[0]!;
        if (maker.subaccount === takerSubaccount) {
          // self-trade prevention
          switch (stp) {
            case "cancel_newest":
              stopTaker = true;
              break;
            case "cancel_oldest": {
              level.shift();
              maker.state = "canceled";
              stpCanceledMakers.push(maker.id);
              this.markDirty(maker.side, maker.price_ticks!);
              continue;
            }
            case "cancel_both": {
              level.shift();
              maker.state = "canceled";
              stpCanceledMakers.push(maker.id);
              this.markDirty(maker.side, maker.price_ticks!);
              stopTaker = true;
              break;
            }
            case "decrement_and_cancel":
              stopTaker = true;
              break;
          }
          break;
        }
        const makerOpen = maker.qty_lots - maker.filled_lots;
        const take = Math.min(remaining, makerOpen);
        fills.push({
          maker_order_id: maker.id,
          maker_subaccount: maker.subaccount,
          price_ticks: maker.price_ticks!,
          qty_lots: take,
          maker_is_bid: maker.side === "bid",
        });
        maker.filled_lots += take;
        remaining -= take;
        if (maker.qty_lots - maker.filled_lots <= 0) {
          level.shift();
          maker.state = "filled";
          this.markDirty(maker.side, maker.price_ticks!);
        }
      }

      if (level.length === 0) {
        map.delete(top);
        const li = ladder.indexOf(top);
        if (li >= 0) ladder.splice(li, 1);
        this.markDirty(takerSide === "bid" ? "ask" : "bid", top);
      }
    }

    this.bump();
    return { fills, stop_taker: stopTaker, stp_canceled_makers: stpCanceledMakers };
  }

  /* ─────────── L2 views ─────────── */

  /** Aggregated depth — iceberg display lots respected. */
  depth(side: Side, maxLevels: number): Level[] {
    const ladder = side === "bid" ? this.bidTicks : this.askTicks;
    const map = side === "bid" ? this.bids : this.asks;
    const out: Level[] = [];
    const stop = Math.min(ladder.length, maxLevels);
    for (let i = 0; i < stop; i++) {
      const tick = ladder[i]!;
      let lots = 0;
      for (const o of map.get(tick) ?? []) {
        lots += o.display_lots != null ? Math.min(o.display_lots, o.qty_lots - o.filled_lots) : o.qty_lots - o.filled_lots;
      }
      if (lots > 0) out.push({ price_ticks: tick, lots });
    }
    return out;
  }

  /** Depth including hidden liquidity (for internal risk views). */
  depthFull(side: Side, maxLevels: number): Level[] {
    const ladder = side === "bid" ? this.bidTicks : this.askTicks;
    const map = side === "bid" ? this.bids : this.asks;
    const out: Level[] = [];
    for (let i = 0; i < Math.min(ladder.length, maxLevels); i++) {
      const tick = ladder[i]!;
      let lots = 0;
      for (const o of map.get(tick) ?? []) lots += o.qty_lots - o.filled_lots;
      if (lots > 0) out.push({ price_ticks: tick, lots });
    }
    return out;
  }

  /* ─────────── delta emission ─────────── */

  private markDirty(side: Side, tick: number): void {
    (side === "bid" ? this.dirtyBids : this.dirtyAsks).add(tick);
  }

  private bump(): void {
    this.seq += 1;
  }

  /**
   * Take the accumulated delta (changed levels only). Levels with 0 visible
   * lots are removals on the wire. Returns null when nothing changed.
   */
  takeDelta(): { kind: "delta"; seq: number; bids: Level[]; asks: Level[] } | null {
    if (this.dirtyBids.size === 0 && this.dirtyAsks.size === 0) return null;
    const bids: Level[] = [];
    for (const t of this.dirtyBids) {
      let lots = 0;
      for (const o of this.bids.get(t) ?? []) {
        lots += o.display_lots != null ? Math.min(o.display_lots, o.qty_lots - o.filled_lots) : o.qty_lots - o.filled_lots;
      }
      bids.push({ price_ticks: t, lots });
    }
    const asks: Level[] = [];
    for (const t of this.dirtyAsks) {
      let lots = 0;
      for (const o of this.asks.get(t) ?? []) {
        lots += o.display_lots != null ? Math.min(o.display_lots, o.qty_lots - o.filled_lots) : o.qty_lots - o.filled_lots;
      }
      asks.push({ price_ticks: t, lots });
    }
    this.dirtyBids.clear();
    this.dirtyAsks.clear();
    bids.sort((a, b) => b.price_ticks - a.price_ticks);
    asks.sort((a, b) => a.price_ticks - b.price_ticks);
    return { kind: "delta", seq: this.seq, bids, asks };
  }
}

function insertSorted(arr: number[], v: number, desc: boolean): void {
  // small arrays — linear scan is fastest and allocation-free
  let i = 0;
  if (desc) {
    while (i < arr.length && arr[i]! > v) i++;
  } else {
    while (i < arr.length && arr[i]! < v) i++;
  }
  if (arr[i] === v) return; // already present (level exists)
  arr.splice(i, 0, v);
}
