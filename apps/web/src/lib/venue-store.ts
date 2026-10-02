"use client";

/**
 * The venue store — the single reactive bridge between the VenueClient
 * stream and the UI. Fine-grained slices keep re-renders surgical:
 * the book ladder never re-renders because the journal grew.
 *
 * Two transports, one store:
 *  - "sim"     the embedded deterministic simulator (Web Worker)
 *  - "remote"  the production gateway over socket.io (G-25 auth, heartbeat,
 *              reconnection; latency + reconnect counters surfaced here)
 */

import { create } from "zustand";
import type {
  AccountDelta,
  BookUpdateTagged,
  JournalEntry,
  Level,
  MarketRow,
  TradePrintLike,
  VenueSnapshot,
} from "@perp/types";
import {
  getVenueClient,
  SimWorkerConnection,
  RemoteConnection,
  loadGatewayConfig,
  saveTransportMode,
  saveEndpoint,
  saveCredentials,
  type RemoteEndpoint,
  type RemoteStatus,
  type TransportMode,
} from "@perp/api-client";
import type { ApiCredentials } from "@perp/api-client";
import { toast } from "sonner";

export type ViewId =
  | "terminal"
  | "markets"
  | "portfolio"
  | "funding"
  | "risk"
  | "incentives"
  | "vaults"
  | "rfq"
  | "governance"
  | "reserves"
  | "journal"
  | "console";

const JOURNAL_CAP = 600;
const PRINTS_CAP = 160;

export interface GatewayState {
  status: RemoteStatus | "idle";
  latencyMs: number | null;
  reconnects: number;
  error: string | null;
  /** Human-readable endpoint for display. */
  label: string;
}

export interface VenueStoreState {
  connected: boolean;
  snapshot: VenueSnapshot | null;
  journal: JournalEntry[];
  prints: TradePrintLike[];
  markets: MarketRow[];
  /** Books by symbol — live L2 state maintained by the session. */
  books: Record<string, { bids: Level[]; asks: Level[]; seq: number; desynced: boolean }>;
  accountDeltas: Record<number, AccountDelta>;
  view: ViewId;
  activeSymbol: string;
  activeAccount: number;
  speed: number;
  lastError: string | null;
  notifCount: number;

  /* transport */
  mode: TransportMode;
  gateway: GatewayState;
  credentials: ApiCredentials | null;
  connectionOpen: boolean;

  setView: (v: ViewId) => void;
  setActiveSymbol: (s: string) => void;
  setActiveAccount: (id: number) => void;
  setSpeed: (n: number) => void;
  resetVenue: (seed?: number) => void;
  markErrorSeen: () => void;
  refresh: () => void;

  setConnectionOpen: (open: boolean) => void;
  connectSim: () => void;
  connectRemote: (endpoint?: RemoteEndpoint, credentials?: ApiCredentials | null) => void;
  disconnectVenue: () => void;
  setCredentials: (creds: ApiCredentials | null) => void;
}

const GATEWAY_IDLE: GatewayState = {
  status: "idle",
  latencyMs: null,
  reconnects: 0,
  error: null,
  label: "—",
};

function endpointLabel(endpoint: RemoteEndpoint): string {
  if (endpoint.origin) return endpoint.origin;
  return `same-origin :${endpoint.port ?? 3031}`;
}

export const useVenueStore = create<VenueStoreState>((set, get) => ({
  connected: false,
  snapshot: null,
  journal: [],
  prints: [],
  markets: [],
  books: {},
  accountDeltas: {},
  view: "terminal",
  activeSymbol: "BTC-PERP",
  activeAccount: 7,
  speed: 600,
  lastError: null,
  notifCount: 0,

  mode: "sim",
  gateway: GATEWAY_IDLE,
  credentials: null,
  connectionOpen: false,

  setView: (v) => set({ view: v }),
  setActiveSymbol: (s) => {
    const client = getVenueClient();
    client.subscribe([s]);
    set({ activeSymbol: s });
  },
  setActiveAccount: (id) => set({ activeAccount: id }),
  setSpeed: (n) => {
    getVenueClient().send({ type: "set_speed", speed: n });
    set({ speed: n });
  },
  resetVenue: (seed) => {
    getVenueClient().send({ type: "reset", seed });
  },
  markErrorSeen: () => set({ lastError: null, notifCount: 0 }),
  refresh: () => {
    getVenueClient().send({ type: "request_snapshot" });
  },

  setConnectionOpen: (open) => set({ connectionOpen: open }),

  connectSim: () => {
    const client = getVenueClient();
    saveTransportMode("sim");
    set({ mode: "sim", gateway: GATEWAY_IDLE });
    client.connect(
      new SimWorkerConnection(
        (msg) => client.receive(msg),
        (message) => useVenueStore.setState({ lastError: message }),
      ),
    );
  },

  connectRemote: (endpoint, credentials) => {
    const client = getVenueClient();
    const cfg = loadGatewayConfig();
    const ep = endpoint ?? cfg.endpoint;
    const creds = credentials !== undefined ? credentials : cfg.credentials;
    saveTransportMode("remote");
    saveEndpoint(ep);
    saveCredentials(creds);
    set({
      mode: "remote",
      credentials: creds,
      connected: false,
      gateway: { ...GATEWAY_IDLE, status: "connecting", label: endpointLabel(ep) },
    });
    void RemoteConnection.create({
      endpoint: ep,
      credentials: creds,
      onMessage: (msg) => client.receive(msg),
      onError: (message) => useVenueStore.setState({ lastError: message }),
      onStatus: (status, detail) => {
        const g = useVenueStore.getState().gateway;
        useVenueStore.setState({
          gateway: {
            ...g,
            status,
            error: detail ?? g.error,
            reconnects: status === "reconnecting" && g.status !== "reconnecting" ? g.reconnects + 1 : g.reconnects,
          },
        });
      },
      onLatency: (ms) => {
        const g = useVenueStore.getState().gateway;
        useVenueStore.setState({ gateway: { ...g, latencyMs: ms } });
      },
      onResubscribe: () => {
        const st = useVenueStore.getState();
        client.subscribe([st.activeSymbol]);
        client.send({ type: "request_snapshot" });
      },
    })
      .then((conn) => {
        // The transport may have been swapped while the handshake was in flight.
        if (useVenueStore.getState().mode !== "remote") {
          conn.terminate();
          return;
        }
        client.connect(conn);
      })
      .catch((err) => {
        useVenueStore.setState({
          gateway: { ...useVenueStore.getState().gateway, status: "error", error: String(err) },
        });
      });
  },

  disconnectVenue: () => {
    getVenueClient().terminate();
    set({ connected: false, snapshot: null, books: {}, markets: [], gateway: GATEWAY_IDLE });
  },

  setCredentials: (creds) => {
    saveCredentials(creds);
    set({ credentials: creds });
  },
}));

/* ─────────────────────────── wiring ─────────────────────────── */

let wired = false;

/** Connect the venue stream into the store. Idempotent + StrictMode-safe. */
export function connectVenue(): void {
  if (wired) return;
  wired = true;
  const client = getVenueClient();

  client.on("bootstrap", (snapshot) => {
    useVenueStore.setState({
      connected: true,
      snapshot,
      markets: snapshot.markets,
      journal: snapshot ? useVenueStore.getState().journal : [],
    });
    client.subscribe([useVenueStore.getState().activeSymbol]);
  });
  client.on("snapshot", (snapshot) => {
    useVenueStore.setState({ snapshot, markets: snapshot.markets });
  });
  client.on("journal", (entries) => {
    const journal = useVenueStore.getState().journal;
    const merged = entries.length > JOURNAL_CAP ? entries.slice(-JOURNAL_CAP) : [...journal, ...entries].slice(-JOURNAL_CAP);
    useVenueStore.setState({ journal: merged });
  });
  client.on("prints", (prints) => {
    const cur = useVenueStore.getState().prints;
    useVenueStore.setState({ prints: [...cur, ...prints].slice(-PRINTS_CAP) });
  });
  client.on("books", (updates) => {
    const books = { ...useVenueStore.getState().books };
    for (const [symbol, u] of Object.entries(updates)) {
      applyBook(books, symbol, u);
    }
    useVenueStore.setState({ books });
  });
  client.on("account", (view) => {
    const accountDeltas = { ...useVenueStore.getState().accountDeltas };
    accountDeltas[view.id] = view;
    useVenueStore.setState({ accountDeltas });
  });
  client.on("markets", (rows) => {
    useVenueStore.setState({ markets: rows });
  });
  client.on("trade_fill", ({ trade, taker, maker }) => {
    const me = useVenueStore.getState().activeAccount;
    if (trade.taker_subaccount !== me && trade.maker_subaccount !== me) return;
    const side = taker ? "TAKER" : maker ? "MAKER" : "";
    const lots = trade.qty_lots;
    toast.success(`Fill · ${trade.symbol}`, {
      description: `${lots} lots @ tick ${trade.price_ticks.toLocaleString()} — you were the ${side.toLowerCase()}`,
    });
  });
  client.on("error", (message) => {
    useVenueStore.setState({ lastError: message, notifCount: useVenueStore.getState().notifCount + 1 });
  });

  // Route to the persisted transport choice (defaults to the embedded sim).
  const cfg = loadGatewayConfig();
  if (cfg.mode === "remote") {
    useVenueStore.getState().connectRemote(cfg.endpoint, cfg.credentials);
  } else {
    useVenueStore.getState().connectSim();
  }
}

function applyBook(
  books: VenueStoreState["books"],
  symbol: string,
  u: BookUpdateTagged,
): void {
  const client = getVenueClient();
  const state = client.books.of(symbol);
  if (u.kind === "snapshot") {
    books[symbol] = { bids: u.bids.slice(), asks: u.asks.slice(), seq: u.seq, desynced: false };
    return;
  }
  if (state) {
    books[symbol] = {
      bids: state.bids,
      asks: state.asks,
      seq: state.seq,
      desynced: state.desynced,
    };
  } else {
    books[symbol] = { bids: [], asks: [], seq: u.seq, desynced: false };
  }
}

/** Latest market row for a symbol (falls back to snapshot copy). */
export function marketRowFor(markets: MarketRow[], snapshot: VenueSnapshot | null, symbol: string): MarketRow | null {
  const live = markets.find((m) => m.symbol === symbol);
  if (live) return live;
  return snapshot?.markets.find((m) => m.symbol === symbol) ?? null;
}

/** Live account delta merged over the snapshot view. */
export function liveAccount(
  snapshot: VenueSnapshot | null,
  deltas: Record<number, AccountDelta>,
  id: number,
): AccountDelta | null {
  const live = deltas[id];
  if (live) return live;
  const view = snapshot?.accounts.find((a) => a.id === id);
  if (!view) return null;
  return {
    id,
    cash_quote_minor: view.cash_quote_minor,
    equity_quote_minor: view.summary.equity_quote_minor,
    maintenance_quote_minor: view.summary.maintenance_quote_minor,
    initial_quote_minor: view.summary.initial_quote_minor,
    order_margin_quote_minor: view.summary.order_margin_quote_minor,
    health: view.health,
    fees_paid_quote_minor: view.fees_paid_quote_minor,
    funding_received_quote_minor: view.funding_received_quote_minor,
  };
}
