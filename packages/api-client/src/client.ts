/**
 * VenueClient — the frontend's single connection to the venue.
 *
 * Two transports, one contract (VenueControl / VenueMessage):
 *  - SimWorkerConnection: the embedded deterministic simulator in a Web
 *    Worker (demo mode — zero backend, works anywhere the app is hosted).
 *  - RemoteConnection: the production socket.io gateway. Same frames.
 *
 * The market-data session (snapshot/delta + gap detection + resync) applies
 * regardless of transport — it is the protocol, not the pipe.
 */

import type {
  AccountDelta,
  BookUpdateTagged,
  JournalEntry,
  MarketRow,
  TradePrintLike,
  VenueControl,
  VenueMessage,
  VenueSnapshot,
} from "@perp/types";
import { MarketDataSession, type BookState } from "./session";

export type ConnectionStatus = "idle" | "connecting" | "live" | "error";

export interface VenueEvents {
  bootstrap: (snapshot: VenueSnapshot) => void;
  snapshot: (snapshot: VenueSnapshot) => void;
  journal: (entries: JournalEntry[]) => void;
  books: (updates: Record<string, BookUpdateTagged>) => void;
  prints: (prints: TradePrintLike[]) => void;
  account: (view: AccountDelta) => void;
  markets: (rows: MarketRow[]) => void;
  error: (message: string) => void;
}

type Handler<K extends keyof VenueEvents> = (payload: Parameters<VenueEvents[K]>[0]) => void;

export interface VenueTransport {
  send(msg: VenueControl): void;
  terminate(): void;
  readonly kind: "sim-worker" | "remote";
}

/* ─────────────────────────── sim worker ─────────────────────────── */

export class SimWorkerConnection implements VenueTransport {
  readonly kind = "sim-worker" as const;
  private worker: Worker;

  constructor(
    private onMessage: (msg: VenueMessage) => void,
    private onError?: (message: string) => void,
  ) {
    this.worker = new Worker(new URL("./venue.worker.ts", import.meta.url), { type: "module" });
    this.worker.onmessage = (ev: MessageEvent<VenueMessage>) => onMessage(ev.data);
    this.worker.onerror = (ev) => onError?.(ev.message || "venue worker crashed");
  }

  send(msg: VenueControl): void {
    this.worker.postMessage(msg);
  }

  terminate(): void {
    this.worker.terminate();
  }
}

/* ─────────────────────────── remote gateway ─────────────────────────── */

/**
 * Production transport over socket.io. Constructed with a pre-connected
 * socket so the client package stays framework- and transport-agnostic.
 */
export class RemoteConnection implements VenueTransport {
  readonly kind = "remote" as const;

  constructor(
    private socket: {
      emit(event: "venue-control", msg: VenueControl): void;
      on(event: "venue-message", cb: (msg: VenueMessage) => void): void;
      disconnect(): void;
    },
  ) {}

  send(msg: VenueControl): void {
    this.socket.emit("venue-control", msg);
  }

  terminate(): void {
    this.socket.disconnect();
  }
}

/* ─────────────────────────── client facade ─────────────────────────── */

export class VenueClient {
  private transport: VenueTransport | null = null;
  readonly books = new MarketDataSession((symbol) => this.resyncBook(symbol));
  private handlers: { [K in keyof VenueEvents]?: Set<Handler<K>> } = {};
  status: ConnectionStatus = "idle";
  snapshot: VenueSnapshot | null = null;

  on<K extends keyof VenueEvents>(event: K, handler: Handler<K>): () => void {
    const set = (this.handlers[event] ??= new Set());
    set.add(handler);
    return () => set.delete(handler);
  }

  private emit<K extends keyof VenueEvents>(event: K, payload: Parameters<VenueEvents[K]>[0]): void {
    for (const h of this.handlers[event] ?? []) {
      try {
        (h as Handler<K>)(payload);
      } catch (err) {
        console.error(`venue handler for ${event} failed`, err);
      }
    }
  }

  connect(transport: VenueTransport, seed?: number, speed?: number): void {
    this.terminate();
    this.transport = transport;
    this.status = "connecting";
    this.send({ type: "connect", seed, speed });
  }

  send(msg: VenueControl): void {
    this.transport?.send(msg);
  }

  terminate(): void {
    if (this.transport) {
      this.transport.terminate();
      this.transport = null;
    }
    this.books.reset();
    this.status = "idle";
  }

  /** Feed a venue message into the client (called by transports). */
  receive(msg: VenueMessage): void {
    switch (msg.channel) {
      case "bootstrap":
        this.status = "live";
        this.snapshot = msg.snapshot;
        this.emit("bootstrap", msg.snapshot);
        break;
      case "snapshot":
        this.snapshot = msg.snapshot;
        this.emit("snapshot", msg.snapshot);
        break;
      case "journal":
        this.emit("journal", msg.entries);
        break;
      case "books": {
        for (const [symbol, update] of Object.entries(msg.updates)) {
          this.books.apply(update, symbol);
        }
        this.emit("books", msg.updates);
        break;
      }
      case "prints":
        this.emit("prints", msg.prints);
        break;
      case "account":
        this.emit("account", msg.view);
        break;
      case "markets":
        this.emit("markets", msg.rows);
        break;
      case "error":
        this.emit("error", msg.message);
        break;
      case "trade_fill":
      case "pong":
        break;
    }
  }

  /** Gap detected — request fresh book snapshots. */
  private resyncBook(symbol: string): void {
    const current = this.subscribedSymbols();
    this.send({ type: "subscribe", symbols: current.includes(symbol) ? current : [...current, symbol] });
  }

  private subscribedSymbols(): string[] {
    return [...this.subscribed];
  }

  private subscribed = new Set<string>();

  subscribe(symbols: string[]): void {
    this.subscribed = new Set(symbols);
    this.send({ type: "subscribe", symbols });
  }

  bookState(symbol: string): BookState | undefined {
    return this.books.of(symbol);
  }
}

/** Module-scoped singleton (workers are expensive; StrictMode-safe). */
let singleton: VenueClient | null = null;

export function getVenueClient(): VenueClient {
  if (!singleton) singleton = new VenueClient();
  return singleton;
}
