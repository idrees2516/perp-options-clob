'use client';

/**
 * TerminalProvider — wires the signed REST client, the authenticated
 * WebSocket (WsManager) and the live TerminalStore together, owns the
 * symbol/expiry selection and the shared order-ticket draft.
 *
 * Subscription strategy (contract limit: <= 64 subscriptions/connection,
 * <= 100 client messages / 10 s):
 *   - `underlying` channel for the active underlying (1)
 *   - `bbo` for every symbol of the visible expiry session (capped at 59)
 *   - `depth` + `trade` + `mark` + `phase` for the focused symbol (4)
 */
import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { useRouter } from 'next/navigation';
import { ApiError, NetworkError, VenueClient, WsManager } from '@/lib/api';
import type { OrderKind, Side, Tif, WsChannel } from '@/lib/types';
import { TerminalStore, useStoreSection, type SymbolLive } from '@/lib/store';
import { useSession } from '@/providers/SessionProvider';
import { useToast } from '@/components/Toast';
import { describeError } from '@/lib/messages';

export interface Selection {
  underlying: number | null;
  expiryMs: number | null;
  symbol: number | null;
}

export interface TicketDraft {
  side: Side;
  qtyStr: string;
  priceStr: string;
  kind: OrderKind;
  tif: Tif;
  postOnly: boolean;
  reduceOnly: boolean;
}

const DEFAULT_TICKET: TicketDraft = {
  side: 'buy',
  qtyStr: '1',
  priceStr: '',
  kind: 'limit',
  tif: 'gtc',
  postOnly: false,
  reduceOnly: false,
};

interface TerminalContextValue {
  store: TerminalStore;
  client: VenueClient;
  sel: Selection;
  ready: boolean;
  ticket: TicketDraft;
  setUnderlying(u: number): void;
  setExpiry(ms: number): void;
  selectSymbol(sym: number): void;
  patchTicket(p: Partial<TicketDraft>): void;
  refreshSymbols(): void;
  refreshAccount(): void;
  cancelOrder(orderId: number, symbol: number): Promise<boolean>;
  cancelAll(): Promise<void>;
  /** Marks that the ticket just surfaced a fill/reject inline, so the
   *  matching WS echo toast is suppressed. */
  notifyResultSurfaced(): void;
}

const TerminalContext = createContext<TerminalContextValue | null>(null);

export function TerminalProvider({ children }: { children: ReactNode }) {
  const { session, logout, updateScope } = useSession();
  const { push } = useToast();
  const router = useRouter();

  if (!session) throw new Error('TerminalProvider requires an active session');

  const store = useMemo(() => new TerminalStore(), []);

  const client = useMemo(
    () =>
      new VenueClient(
        { keyId: session.keyId, secretHex: session.secretHex, baseUrl: session.baseUrl },
        {
          onResult: (r) => {
            if (r.ok) store.markOnline();
            else if (r.network) store.markOffline();
          },
        },
      ),
    [session.keyId, session.secretHex, session.baseUrl, store],
  );

  const [sel, setSel] = useState<Selection>({ underlying: null, expiryMs: null, symbol: null });
  const [ticket, setTicket] = useState<TicketDraft>(DEFAULT_TICKET);
  const [ready, setReady] = useState(false);

  const wsRef = useRef<WsManager | null>(null);
  const everLiveRef = useRef(false);
  const accountRefreshTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const lastResultSurfaceRef = useRef(0);
  const defaultsChosenRef = useRef(false);

  const handleAuthz = useCallback(
    (e: unknown): boolean => {
      if (e instanceof ApiError && (e.status === 401 || e.status === 403)) {
        push('error', 'Session rejected', `${e.code} — signing out`);
        logout();
        void router.replace('/');
        return true;
      }
      return false;
    },
    [logout, push, router],
  );

  const refreshAccount = useCallback(async () => {
    try {
      store.applyAccount(await client.getAccount());
    } catch (e) {
      if (handleAuthz(e)) return;
      if (e instanceof NetworkError) {
        store.markOffline();
        return;
      }
      push('error', 'Account refresh failed', describeError(e));
    }
  }, [client, store, handleAuthz, push]);

  const refreshSymbols = useCallback(
    async (chooseDefaults: boolean) => {
      try {
        const snap = await client.getSymbols();
        store.applySymbols(snap);
        setReady(true);
        if (chooseDefaults && !defaultsChosenRef.current) {
          defaultsChosenRef.current = true;
          setSel(pickDefaults(store));
        }
      } catch (e) {
        if (handleAuthz(e)) return;
        if (e instanceof NetworkError) {
          store.markOffline();
          return;
        }
        push('error', 'Symbols refresh failed', describeError(e));
      }
    },
    [client, store, handleAuthz, push],
  );

  const scheduleAccountRefresh = useCallback(() => {
    if (accountRefreshTimer.current !== null) return;
    accountRefreshTimer.current = setTimeout(() => {
      accountRefreshTimer.current = null;
      void refreshAccount();
    }, 1500);
  }, [refreshAccount]);

  // -- bootstrap + WS lifecycle -------------------------------------------------

  useEffect(() => {
    const ws = new WsManager(
      client.creds,
      {
        onStatus: (s) => {
          store.setConnStatus(s);
          if (s === 'live') {
            store.markOnline();
            if (everLiveRef.current) {
              // reconnect: resync REST snapshots (WS replays its subscriptions itself)
              void refreshAccount();
              void refreshSymbols(false);
            }
            everLiveRef.current = true;
          } else if (s === 'reconnecting') {
            // WS path is down — probe whether the engine itself is reachable
            void VenueClient.getHealth(client.creds.baseUrl)
              .then(() => store.markOnline())
              .catch(() => store.markOffline());
          }
        },
        onMessage: (m) => {
          const out = store.dispatch(m);
          if (out.scope !== undefined) updateScope(out.scope);
          if (out.needsAccountRefresh) scheduleAccountRefresh();
          const ownEchoRecent = Date.now() - lastResultSurfaceRef.current < 2500;
          for (const t of out.toasts) {
            const isOwnEcho = t.title.startsWith('Filled') || t.title.includes('rejected');
            if (isOwnEcho && ownEchoRecent) continue;
            push(t.kind, t.title, t.detail);
          }
        },
        onError: (e) => {
          if (e.stage === 'handler') console.warn('[ws] handler error:', e.detail);
        },
      },
      () => client.venueNow(),
    );
    wsRef.current = ws;
    ws.connect();

    void (async () => {
      try {
        store.setClockOffset(await client.syncTime());
      } catch {
        /* clock falls back to local; the login pre-flight already warned */
      }
      await refreshAccount();
      await refreshSymbols(true);
    })();

    const symbolsTimer = setInterval(() => {
      if (typeof document === 'undefined' || !document.hidden) void refreshSymbols(false);
    }, 10_000);
    const accountTimer = setInterval(() => {
      if (typeof document === 'undefined' || !document.hidden) void refreshAccount();
    }, 30_000);
    const clockTimer = setInterval(() => {
      void client
        .syncTime()
        .then((off) => store.setClockOffset(off))
        .catch(() => undefined);
    }, 60_000);

    return () => {
      clearInterval(symbolsTimer);
      clearInterval(accountTimer);
      clearInterval(clockTimer);
      if (accountRefreshTimer.current !== null) {
        clearTimeout(accountRefreshTimer.current);
        accountRefreshTimer.current = null;
      }
      ws.close();
      wsRef.current = null;
    };
  }, [client, store, refreshAccount, refreshSymbols, scheduleAccountRefresh, updateScope, push]);

  // -- channel reconciliation ------------------------------------------------------

  const chainRev = useStoreSection(store, 'chain-rev');
  const sessionSymbols = useMemo(() => {
    if (sel.underlying === null || sel.expiryMs === null) return [] as number[];
    const ids: number[] = [];
    for (const s of store.symbols.values()) {
      if (s.underlying === sel.underlying && s.expiry_ms === sel.expiryMs) ids.push(s.symbol);
    }
    return ids.sort((a, b) => a - b);
  }, [store, sel.underlying, sel.expiryMs, chainRev]);

  useEffect(() => {
    const ws = wsRef.current;
    if (!ws) return;
    const wantBbo = new Set<number>(sessionSymbols.slice(0, 59));
    const wantUnder = new Set<number>(sel.underlying !== null ? [sel.underlying] : []);
    const wantFocus = new Set<number>(sel.symbol !== null ? [sel.symbol] : []);
    const cur = ws.subsSnapshot();
    reconcileSet(ws, 'underlying', wantUnder, cur.get('underlying'));
    reconcileSet(ws, 'bbo', wantBbo, cur.get('bbo'));
    for (const ch of ['depth', 'trade', 'mark', 'phase'] as const) {
      reconcileSet(ws, ch, wantFocus, cur.get(ch));
    }
  }, [sessionSymbols, sel.underlying, sel.symbol]);

  // -- focused symbol: REST baseline + memory focus ---------------------------------

  useEffect(() => {
    store.setFocusSymbol(sel.symbol);
    if (sel.symbol === null) return;
    const sym = sel.symbol;
    void client
      .getBook(sym, 10)
      .then((b) => store.applyBook(b))
      .catch(() => undefined);
    void client
      .getTape(sym, 50)
      .then((t) => store.applyTapeTail(t))
      .catch(() => undefined);
  }, [sel.symbol, client, store]);

  // -- selection + ticket -------------------------------------------------------------

  const setUnderlying = useCallback((u: number) => {
    setSel({ underlying: u, expiryMs: null, symbol: null });
  }, []);

  const setExpiry = useCallback((ms: number) => {
    setSel((prev) => ({ ...prev, expiryMs: ms, symbol: null }));
  }, []);

  const selectSymbol = useCallback(
    (sym: number) => {
      const s = store.symbols.get(sym);
      if (!s) return;
      setSel((prev) =>
        prev.symbol === sym && prev.underlying === s.underlying && prev.expiryMs === s.expiry_ms
          ? prev
          : { underlying: s.underlying, expiryMs: s.expiry_ms, symbol: sym },
      );
    },
    [store],
  );

  const patchTicket = useCallback((p: Partial<TicketDraft>) => {
    setTicket((prev) => ({ ...prev, ...p }));
  }, []);

  const notifyResultSurfaced = useCallback(() => {
    lastResultSurfaceRef.current = Date.now();
  }, []);

  // -- order management ------------------------------------------------------------------

  const cancelOrder = useCallback(
    async (orderId: number, symbol: number): Promise<boolean> => {
      try {
        await client.cancelOrder(orderId, symbol);
        return true;
      } catch (e) {
        if (e instanceof ApiError && (e.code === 'UnknownOrder' || e.status === 409)) {
          push('info', `Order #${orderId} already gone`, 'UnknownOrder (409)');
        } else {
          push('error', `Cancel #${orderId} failed`, describeError(e));
        }
        return false;
      }
    },
    [client, push],
  );

  const cancelAll = useCallback(async () => {
    const rows = [...store.orders.values()];
    if (rows.length === 0) {
      push('info', 'No open orders');
      return;
    }
    let ok = 0;
    for (const o of rows) {
      if (await cancelOrder(o.order, o.symbol)) ok += 1;
    }
    push(ok === rows.length ? 'success' : 'warn', `Canceled ${ok}/${rows.length} orders`);
    scheduleAccountRefresh();
  }, [store, cancelOrder, push, scheduleAccountRefresh]);

  const value = useMemo<TerminalContextValue>(
    () => ({
      store,
      client,
      sel,
      ready,
      ticket,
      setUnderlying,
      setExpiry,
      selectSymbol,
      patchTicket,
      refreshSymbols: () => void refreshSymbols(false),
      refreshAccount: () => void refreshAccount(),
      cancelOrder,
      cancelAll,
      notifyResultSurfaced,
    }),
    [
      store,
      client,
      sel,
      ready,
      ticket,
      setUnderlying,
      setExpiry,
      selectSymbol,
      patchTicket,
      refreshSymbols,
      refreshAccount,
      cancelOrder,
      cancelAll,
      notifyResultSurfaced,
    ],
  );

  return <TerminalContext.Provider value={value}>{children}</TerminalContext.Provider>;
}

export function useTerminal(): TerminalContextValue {
  const ctx = useContext(TerminalContext);
  if (!ctx) throw new Error('useTerminal must be used inside <TerminalProvider>');
  return ctx;
}

// -- helpers --------------------------------------------------------------------------------

function reconcileSet(
  ws: WsManager,
  channel: WsChannel,
  want: ReadonlySet<number>,
  have: ReadonlySet<number> | undefined,
): void {
  for (const id of want) if (!have?.has(id)) ws.subscribe(channel, id);
  if (have) for (const id of have) if (!want.has(id)) ws.unsubscribe(channel, id);
}

/** future sessions first (soonest wins), then the most recently expired. */
function expiryScore(e: number, now: number): number {
  return e >= now ? e - now : 1e12 + (now - e);
}

function pickDefaults(store: TerminalStore): Selection {
  const now = store.serverNow();
  const underlyings = [...store.underlyings.keys()].sort((a, b) => a - b);
  if (underlyings.length === 0) return { underlying: null, expiryMs: null, symbol: null };
  let chosenU = underlyings[0]!;
  let chosenExp: number | null = null;
  let best = Infinity;
  for (const u of underlyings) {
    for (const s of store.symbols.values()) {
      if (s.underlying !== u) continue;
      const score = expiryScore(s.expiry_ms, now);
      if (score < best) {
        best = score;
        chosenU = u;
        chosenExp = s.expiry_ms;
      }
    }
  }
  if (chosenExp === null) return { underlying: chosenU, expiryMs: null, symbol: null };
  const mark = store.underlyings.get(chosenU)?.mark ?? 0;
  const syms = [...store.symbols.values()].filter(
    (s) => s.underlying === chosenU && s.expiry_ms === chosenExp,
  );
  const calls = syms.filter((s) => s.kind === 'call');
  const pool: SymbolLive[] = calls.length > 0 ? calls : syms;
  let atm: SymbolLive | undefined = pool[0];
  if (atm !== undefined) {
    for (const s of pool) {
      if (Math.abs(s.strike - mark) < Math.abs(atm.strike - mark)) atm = s;
    }
  }
  return { underlying: chosenU, expiryMs: chosenExp, symbol: atm?.symbol ?? null };
}
