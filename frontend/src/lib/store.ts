/**
 * TerminalStore — the live market/account state behind the /trade terminal.
 *
 * Design: plain mutable Maps + a per-section version counter. Panels subscribe
 * to a section via `useStoreSection` (React's useSyncExternalStore over the
 * version number), so a burst of depth/trade/bbo frames only re-renders the
 * panels that consume that section. No external state library — a ~200-line
 * class and React primitives.
 *
 * All values remain integer ticks; intrinsic/P&L math is integer math.
 */
import { useCallback, useEffect, useState, useSyncExternalStore } from 'react';
import type {
  AccountResp,
  Balance,
  BookResp,
  OpenOrder,
  Phase,
  Position,
  PriceLevel,
  Scope,
  SymbolsResp,
  SymbolSnap,
  TapeResp,
  TapeTrade,
  UnderlyingSnap,
  WsServerMsg,
} from './types';
import type { WsStatus } from './api';

export type StoreSection =
  | 'chain' // symbol rows (bbo/mark/phase/...)
  | 'chain-rev' // symbol LIST membership (REST refresh only)
  | 'underlying'
  | 'depth'
  | 'tape'
  | 'orders'
  | 'positions'
  | 'balances'
  | 'account'
  | 'conn'
  | 'clock';

export type SymbolLive = SymbolSnap;
export type UnderlyingLive = UnderlyingSnap & { haltLevel?: number };
export interface DepthFrame {
  bids: PriceLevel[];
  asks: PriceLevel[];
  ms: number;
  src: 'rest' | 'ws';
}
export type OrderLive = OpenOrder & {
  /** true when created from a WS ack without full details (foreign session) */
  partial?: boolean;
};
export type PositionLive = Position;
export type BalanceLive = Balance;

export type ToastKind = 'info' | 'success' | 'warn' | 'error';
export interface ToastPayload {
  kind: ToastKind;
  title: string;
  detail?: string;
}

/** Side effects a single inbound WS frame wants from the provider. */
export interface DispatchOutcome {
  needsAccountRefresh: boolean;
  scope?: Scope;
  toasts: ToastPayload[];
}

const TAPE_CAP = 100;

export class TerminalStore {
  readonly symbols = new Map<number, SymbolLive>();
  readonly underlyings = new Map<number, UnderlyingLive>();
  readonly depth = new Map<number, DepthFrame>();
  readonly tape = new Map<number, TapeTrade[]>();
  readonly orders = new Map<number, OrderLive>();
  readonly positions = new Map<number, PositionLive>();
  readonly balances = new Map<number, BalanceLive>();

  account: { account: number; tier: string } | null = null;
  connStatus: WsStatus = 'idle';
  offline = false;
  /** engine wall-clock minus local clock, ms (from /v1/time and WS hello) */
  clockOffsetMs = 0;
  /** symbol whose depth/tape panels are focused (single focus keeps memory tight) */
  focusedSymbol: number | null = null;

  private versions: Record<StoreSection, number> = {
    chain: 0,
    'chain-rev': 0,
    underlying: 0,
    depth: 0,
    tape: 0,
    orders: 0,
    positions: 0,
    balances: 0,
    account: 0,
    conn: 0,
    clock: 0,
  };
  private listeners = new Map<StoreSection, Set<() => void>>();

  // -- subscription plumbing ---------------------------------------------------

  subscribe = (section: StoreSection, cb: () => void): (() => void) => {
    let set = this.listeners.get(section);
    if (!set) {
      set = new Set();
      this.listeners.set(section, set);
    }
    set.add(cb);
    return () => {
      set!.delete(cb);
    };
  };

  version = (section: StoreSection): number => this.versions[section];

  private bump(...sections: StoreSection[]): void {
    for (const s of sections) {
      this.versions[s] += 1;
      const set = this.listeners.get(s);
      if (set) for (const cb of set) cb();
    }
  }

  // -- clock ---------------------------------------------------------------------

  serverNow(): number {
    return Date.now() + this.clockOffsetMs;
  }

  setClockOffset(offsetMs: number): void {
    this.clockOffsetMs = offsetMs;
    this.bump('clock');
  }

  // -- connection / reachability ----------------------------------------------------

  setConnStatus(s: WsStatus): void {
    this.connStatus = s;
    this.bump('conn');
  }

  markOnline(): void {
    if (!this.offline) return;
    this.offline = false;
    this.bump('conn');
  }

  markOffline(): void {
    if (this.offline) return;
    this.offline = true;
    this.bump('conn');
  }

  // -- REST snapshots ----------------------------------------------------------------

  applySymbols(snap: SymbolsResp): void {
    this.underlyings.clear();
    for (const u of snap.underlyings) {
      const prev = this.underlyings.get(u.underlying);
      this.underlyings.set(u.underlying, prev ? { ...prev, ...u } : { ...u });
    }
    const live = new Set<number>();
    for (const s of snap.symbols) {
      live.add(s.symbol);
      const prev = this.symbols.get(s.symbol);
      this.symbols.set(s.symbol, prev ? { ...prev, ...s } : { ...s });
    }
    for (const id of [...this.symbols.keys()]) {
      if (!live.has(id)) this.symbols.delete(id);
    }
    this.bump('chain', 'chain-rev', 'underlying');
  }

  applyAccount(acc: AccountResp): void {
    this.account = { account: acc.account, tier: acc.tier };
    this.balances.clear();
    for (const b of acc.balances) this.balances.set(b.underlying, { ...b });
    this.positions.clear();
    for (const p of acc.positions) this.positions.set(p.symbol, { ...p });
    this.orders.clear();
    for (const o of acc.orders) this.orders.set(o.order, { ...o });
    this.bump('account', 'balances', 'positions', 'orders');
  }

  applyBook(book: BookResp): void {
    // REST book is the baseline until the first WS depth frame replaces it.
    this.depth.set(book.symbol, { bids: book.bids, asks: book.asks, ms: Date.now(), src: 'rest' });
    this.bump('depth');
  }

  applyTapeTail(tape: TapeResp): void {
    this.tape.set(tape.symbol, tape.trades.slice(0, TAPE_CAP));
    this.bump('tape');
  }

  setFocusSymbol(sym: number | null): void {
    if (this.focusedSymbol === sym) return;
    const prev = this.focusedSymbol;
    this.focusedSymbol = sym;
    if (prev !== null) {
      this.depth.delete(prev);
      this.tape.delete(prev);
    }
    this.bump('depth', 'tape');
  }

  // -- order entry support ---------------------------------------------------------

  /**
   * Register the locally-known parameters of an order we just submitted, so a
   * WS `ack` that races ahead of the REST response still shows full detail.
   */
  attachLocalOrder(orderId: number, o: OpenOrder): void {
    const existing = this.orders.get(orderId);
    this.orders.set(orderId, { ...o, order: orderId, partial: false, ...(existing ? { leaves: existing.leaves } : {}) });
    this.bump('orders');
  }

  // -- WS dispatch --------------------------------------------------------------------

  dispatch(msg: WsServerMsg): DispatchOutcome {
    const out: DispatchOutcome = { needsAccountRefresh: false, toasts: [] };
    switch (msg.t) {
      case 'hello': {
        this.setClockOffset(msg.ms - Date.now());
        out.scope = msg.scope;
        break;
      }
      case 'bbo': {
        const s = this.symbols.get(msg.symbol);
        if (s) {
          s.bid = msg.bid;
          s.bid_qty = msg.bid_qty;
          s.ask = msg.ask;
          s.ask_qty = msg.ask_qty;
          this.bump('chain');
        }
        break;
      }
      case 'trade': {
        const rows = this.tape.get(msg.symbol) ?? [];
        rows.unshift({ price: msg.price, qty: msg.qty, aggressor: msg.aggressor, ms: msg.ms });
        if (rows.length > TAPE_CAP) rows.length = TAPE_CAP;
        this.tape.set(msg.symbol, rows);
        this.bump('tape');
        break;
      }
      case 'depth': {
        this.depth.set(msg.symbol, { bids: msg.bids, asks: msg.asks, ms: Date.now(), src: 'ws' });
        this.bump('depth');
        break;
      }
      case 'phase': {
        const s = this.symbols.get(msg.symbol);
        if (s) {
          s.phase = msg.phase as Phase;
          this.bump('chain');
        }
        break;
      }
      case 'mark': {
        const s = this.symbols.get(msg.symbol);
        if (s) {
          s.mark = msg.price;
          this.bump('chain');
        }
        break;
      }
      case 'underlying': {
        const prev = this.underlyings.get(msg.underlying);
        this.underlyings.set(msg.underlying, {
          underlying: msg.underlying,
          mark: msg.price,
          ref: msg.ref,
          fast: msg.fast,
          halted: msg.halted,
          haltLevel: prev?.haltLevel,
        });
        this.bump('underlying', 'positions'); // positions' live intrinsic moves with the mark
        break;
      }
      case 'halt': {
        const u = this.underlyings.get(msg.underlying);
        if (u) {
          u.halted = true;
          u.haltLevel = msg.level;
        }
        this.bump('underlying');
        break;
      }
      case 'resumed': {
        const u = this.underlyings.get(msg.underlying);
        if (u) {
          u.halted = false;
          delete u.haltLevel;
        }
        this.bump('underlying');
        break;
      }
      case 'ack': {
        const existing = this.orders.get(msg.order);
        if (existing) {
          existing.leaves = msg.leaves;
          existing.ms = msg.ms;
          existing.partial = false;
        } else {
          // foreign order (another window/session of the same account) — details unknown
          this.orders.set(msg.order, {
            order: msg.order,
            symbol: msg.symbol,
            side: 'buy',
            price: 0,
            qty: 0,
            leaves: msg.leaves,
            tif: 'gtc',
            kind: 'limit',
            post_only: false,
            ms: msg.ms,
            partial: true,
          });
          out.needsAccountRefresh = true;
        }
        this.bump('orders');
        break;
      }
      case 'reject': {
        out.toasts.push({
          kind: 'warn',
          title: `Order #${msg.order} rejected`,
          detail: `${msg.reason} (code ${msg.code})`,
        });
        break;
      }
      case 'canceled': {
        const o = this.orders.get(msg.order);
        if (o) {
          o.leaves = msg.leaves;
          if (o.leaves <= 0) this.orders.delete(msg.order);
        }
        if (msg.reason !== 'user') {
          out.toasts.push({ kind: 'info', title: `Order #${msg.order} canceled`, detail: msg.reason });
        }
        this.bump('orders');
        break;
      }
      case 'fill': {
        const o = this.orders.get(msg.order);
        if (o) {
          o.leaves = Math.max(0, o.leaves - msg.qty);
          if (o.leaves <= 0) this.orders.delete(msg.order);
        }
        out.needsAccountRefresh = true; // positions/balances resettle server-side
        out.toasts.push({
          kind: 'success',
          title: `Filled ${msg.qty} @ ${(msg.price / 100).toFixed(2)}`,
          detail: `${msg.role}${msg.fee > 0 ? ` · fee ${msg.fee}` : ''}${msg.rebate > 0 ? ` · rebate ${msg.rebate}` : ''}`,
        });
        this.bump('orders');
        break;
      }
      case 'exercised': {
        out.needsAccountRefresh = true;
        out.toasts.push({
          kind: 'info',
          title: `Exercised ${msg.qty}`,
          detail: `intrinsic ${(msg.intrinsic / 100).toFixed(2)} · cash flow ${(msg.cash_flow / 100).toFixed(2)} · fee ${(msg.fee / 100).toFixed(2)}`,
        });
        break;
      }
      case 'balance': {
        this.balances.set(msg.underlying, {
          underlying: msg.underlying,
          cash: msg.cash,
          hold: msg.hold,
          margin_required: msg.margin_required,
          margin_reserved: msg.margin_reserved,
          available: msg.available,
          fees_paid: this.balances.get(msg.underlying)?.fees_paid ?? 0,
          rebates_recv: this.balances.get(msg.underlying)?.rebates_recv ?? 0,
        });
        this.bump('balances');
        break;
      }
      case 'pong': {
        break;
      }
    }
    return out;
  }
}

// -- React hooks ---------------------------------------------------------------------

/** Re-render the calling component whenever `section`'s version changes. */
export function useStoreSection(store: TerminalStore, section: StoreSection): number {
  const subscribe = useCallback((cb: () => void) => store.subscribe(section, cb), [store, section]);
  return useSyncExternalStore(
    subscribe,
    () => store.version(section),
    () => 0,
  );
}

/** Ticking server clock; re-renders every `intervalMs` (countdowns, ages). */
export function useServerClock(store: TerminalStore, intervalMs = 1000): number {
  const [, setTick] = useState(0);
  useEffect(() => {
    const id = setInterval(() => setTick(Date.now()), intervalMs);
    return () => clearInterval(id);
  }, [intervalMs]);
  return store.serverNow();
}
