/**
 * VenueHost — the authoritative venue behind the gateway.
 *
 * Owns one SimVenue (the deterministic engine + autonomous ticker) and fans
 * its outputs out to every connected client session:
 *
 *  - per-connection subscriptions with per-connection G-27 book stream
 *    sequencing (snapshots consume a seq; deltas follow +1 — a visible gap
 *    is a real drop, and the client resyncs)
 *  - prints / journal / markets / periodic full snapshots broadcast to all
 *  - commands processed immediately with a synchronous drain so the issuing
 *    client sees its own fills at once (trade_fill receipts)
 *  - demo mode (auth off) grants admin-equivalent control; authenticated
 *    production mode gates venue-wide controls to the admin role
 */

import {
  SimVenue,
  BookFrameSequencer,
  buildMarketRows,
} from "@perp/sim-engine";
import type {
  AccountDelta,
  BookUpdateTagged,
  Command,
  GovernanceAction,
  JournalEntry,
  Trade,
  TradePrintLike,
  VenueMessage,
  VenueSnapshot,
} from "@perp/types";
import type { GatewayConfig } from "./config";
import { log } from "./log";

export type SessionRole = "trader" | "admin" | "demo";

export interface ClientSession {
  id: string;
  role: SessionRole;
  symbols: Set<string>;
  sequencer: BookFrameSequencer;
  send: (msg: VenueMessage) => void;
}

export interface CommandOutcome {
  events: JournalEntry[];
  trades: Trade[];
}

const DRAIN_MS = 80;
const MARKETS_MS = 700;
const SNAPSHOT_MS = 2_500;
const PRINTS_CAP = 500;
const DEFAULT_SYMBOLS = ["BTC-PERP"];

export class VenueHost {
  readonly venue: SimVenue;
  private clients = new Map<string, ClientSession>();
  private drainTimer: ReturnType<typeof setInterval> | null = null;
  private marketTimer: ReturnType<typeof setInterval> | null = null;
  private snapshotTimer: ReturnType<typeof setInterval> | null = null;
  private printsRing: TradePrintLike[] = [];
  private nextPrintSeq = 0;

  stats = {
    commandsProcessed: 0,
    messagesOut: 0,
    rejectedReplays: 0,
    rateLimited: 0,
    startedAt: Date.now(),
  };

  constructor(private config: GatewayConfig) {
    this.venue = new SimVenue({ seed: config.venueSeed });
  }

  start(): void {
    if (this.drainTimer) return;
    this.venue.start();
    this.venue.setSpeed(this.config.venueSpeed);
    this.drainTimer = setInterval(() => this.tick(), DRAIN_MS);
    this.marketTimer = setInterval(() => {
      this.venue.engine.out.marketDirty = true;
    }, MARKETS_MS);
    this.snapshotTimer = setInterval(() => {
      this.broadcast({ channel: "snapshot", snapshot: this.venue.snapshot() });
    }, SNAPSHOT_MS);
    log.info("venue.started", {
      seed: this.config.venueSeed,
      speed: this.config.venueSpeed,
      phase: this.venue.engine.phase,
    });
  }

  stop(): void {
    if (this.drainTimer) clearInterval(this.drainTimer);
    if (this.marketTimer) clearInterval(this.marketTimer);
    if (this.snapshotTimer) clearInterval(this.snapshotTimer);
    this.drainTimer = null;
    this.marketTimer = null;
    this.snapshotTimer = null;
    this.venue.stop();
  }

  /* ── sessions ── */

  get clientCount(): number {
    return this.clients.size;
  }

  attach(session: ClientSession): void {
    this.clients.set(session.id, session);
  }

  detach(id: string): void {
    const had = this.clients.get(id);
    this.clients.delete(id);
    if (had) this.resubscribeUnion();
  }

  /** A fresh session's opening state: bootstrap + default book snapshots. */
  handshake(session: ClientSession): void {
    session.symbols = new Set(DEFAULT_SYMBOLS);
    this.resubscribeUnion();
    session.send({ channel: "bootstrap", snapshot: this.venue.snapshot() });
    this.sendBookSnapshots(session, DEFAULT_SYMBOLS);
  }

  /** Update a session's subscriptions; answer with fresh book snapshots. */
  subscribe(session: ClientSession, symbols: string[]): void {
    session.symbols = new Set(symbols.length > 0 ? symbols : DEFAULT_SYMBOLS);
    this.resubscribeUnion();
    this.sendBookSnapshots(session, [...session.symbols]);
  }

  requestSnapshot(session: ClientSession): void {
    session.send({ channel: "snapshot", snapshot: this.venue.snapshot() });
    this.sendBookSnapshots(session, [...session.symbols]);
  }

  private sendBookSnapshots(session: ClientSession, symbols: string[]): void {
    const updates: Record<string, BookUpdateTagged> = {};
    for (const sym of symbols) {
      const book = this.venue.engine.books.get(sym);
      if (!book) continue;
      updates[sym] = session.sequencer.snapshot(sym, book.depth("bid", 20), book.depth("ask", 20));
      // NOTE: the book's pending delta is NOT consumed here. Other sessions
      // still need it, and level deltas are replace-by-price (idempotent) —
      // replaying it over this snapshot is harmless, stealing it is not.
    }
    if (Object.keys(updates).length > 0) {
      session.send({ channel: "books", updates });
    }
  }

  /** Union of every session's symbols drives the venue's drain filter. */
  private resubscribeUnion(): void {
    const union = new Set<string>();
    for (const s of this.clients.values()) {
      for (const sym of s.symbols) union.add(sym);
    }
    this.venue.subscribe([...union]);
  }

  /* ── controls ── */

  command(cmd: Command): CommandOutcome {
    this.venue.command(cmd);
    this.stats.commandsProcessed++;
    // Synchronous drain: clients see the effect immediately instead of
    // waiting for the next 80ms tick.
    return this.tick();
  }

  governance(action: GovernanceAction): string | null {
    return this.venue.governance(action);
  }

  withdrawalOp(op: {
    op: "request" | "approve" | "cancel" | "settle_due" | "force_queue";
    id?: number;
    subaccount?: number;
    amount_quote_minor?: bigint;
    destination?: string;
  }): void {
    this.venue.withdrawalOp({
      type: "withdrawal",
      op: op.op,
      id: op.id,
      subaccount: op.subaccount,
      amount_quote_minor: op.amount_quote_minor,
      destination: op.destination,
    });
    this.tick();
  }

  porBuild(): void {
    this.venue.porBuild();
    this.broadcast({ channel: "snapshot", snapshot: this.venue.snapshot() });
  }

  oracleInject(provider: string, price: bigint): void {
    this.venue.oracleInject(provider, price);
    this.tick();
  }

  oracleToggleQuarantine(provider: string): void {
    const st = this.venue.engine.oracle;
    st.toggleQuarantine(provider);
    this.venue.engine.noteOracleState();
    this.tick();
  }

  setSpeed(speed: number): void {
    this.venue.setSpeed(speed);
    this.tick();
  }

  pause(): void {
    this.venue.pause();
  }

  resume(): void {
    this.venue.resume();
  }

  /** Advance the venue clock while paused (console power feature). */
  step(ms: number): void {
    this.venue.step(ms);
    this.tick();
  }

  reset(seed: number): void {
    this.venue.reset(seed);
    this.broadcast({ channel: "bootstrap", snapshot: this.venue.snapshot() });
    for (const session of this.clients.values()) {
      session.sequencer.reset();
      this.sendBookSnapshots(session, [...session.symbols]);
    }
    log.info("venue.reset", { seed });
  }

  /* ── data plane ── */

  snapshot(): VenueSnapshot {
    return this.venue.snapshot();
  }

  marketRows(): ReturnType<typeof buildMarketRows> {
    return buildMarketRows(this.venue.engine);
  }

  accountView(id: number): AccountDelta | null {
    const acc = this.venue.engine.accounts.get(id);
    if (!acc) return null;
    const m = this.venue.engine.marginOf(id);
    return {
      id,
      cash_quote_minor: acc.cash_quote_minor,
      equity_quote_minor: m.equity,
      maintenance_quote_minor: m.maintenance,
      initial_quote_minor: m.initial,
      order_margin_quote_minor: m.order_margin,
      health: m.health,
      fees_paid_quote_minor: acc.fees_paid_quote_minor,
      funding_received_quote_minor: acc.funding_pnl_quote_minor,
    };
  }

  bookSnapshot(symbol: string, depth = 20): BookUpdateTagged | null {
    const book = this.venue.engine.books.get(symbol);
    if (!book) return null;
    return { kind: "snapshot", seq: book.bookSeq, bids: book.depth("bid", depth), asks: book.depth("ask", depth) };
  }

  recentPrints(sinceSeq?: number, limit = 200): TradePrintLike[] {
    const from = sinceSeq === undefined ? 0 : sinceSeq + 1;
    return this.printsRing.filter((p) => p.seq >= from).slice(-limit);
  }

  venuePhase(): string {
    return this.venue.engine.phase;
  }

  /* ── fan-out ── */

  private broadcast(msg: VenueMessage): void {
    for (const session of this.clients.values()) session.send(msg);
    this.stats.messagesOut++;
  }

  /** Drain the venue and route outputs. Returns the drained journal+trades. */
  private tick(): CommandOutcome {
    const out = this.venue.drain();

    if (out.prints.length > 0) {
      const prints: TradePrintLike[] = out.prints.map((t) => ({
        seq: ++this.nextPrintSeq,
        ts: t.ts,
        symbol: t.symbol,
        price_ticks: t.price_ticks,
        qty_lots: t.qty_lots,
        maker_side: t.maker_side,
        notional_quote_minor: t.notional_quote_minor,
      }));
      this.printsRing = [...this.printsRing, ...prints].slice(-PRINTS_CAP);
      this.broadcast({ channel: "prints", prints });
    }

    if (out.journal.length > 0) {
      this.broadcast({ channel: "journal", entries: out.journal });
    }

    if (Object.keys(out.books).length > 0) {
      for (const session of this.clients.values()) {
        const updates: Record<string, BookUpdateTagged> = {};
        for (const [sym, frame] of Object.entries(out.books)) {
          if (!session.symbols.has(sym)) continue;
          updates[sym] = session.sequencer.delta(sym, frame);
        }
        if (Object.keys(updates).length > 0) {
          session.send({ channel: "books", updates });
          this.stats.messagesOut++;
        }
      }
    }

    if (out.accountDirty.size > 0) {
      for (const id of out.accountDirty) {
        const view = this.accountView(id);
        if (view) this.broadcast({ channel: "account", view });
      }
    }

    if (out.marketDirty) {
      this.broadcast({ channel: "markets", rows: this.marketRows() });
    }

    return { events: out.journal, trades: out.prints };
  }
}

/** Does this session hold venue-wide control rights? */
export function isAdminLike(role: SessionRole): boolean {
  return role === "admin" || role === "demo";
}
