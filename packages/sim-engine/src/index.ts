/**
 * @perp/sim-engine — deterministic venue simulator for perp-options-clob.
 *
 * The `SimVenue` facade is what a gateway host embeds: it owns the engine,
 * runs the autonomous ticker on a fixed step, and exposes the same control
 * surface the protocol gateway would (commands, governance, withdrawals,
 * oracle defense, proof-of-reserves).
 */

import type {
  BookUpdateTagged,
  Command,
  GovernanceAction,
  Level,
  Symbol,
  VenueSnapshot,
} from "@perp/types";
import { SimEngine, DEFAULT_ENGINE_CONFIG, type EngineConfig, type EngineOutputs } from "./engine";
import { VenueTicker } from "./venue-ticker";
import { buildSnapshot, buildPor } from "./snapshot";
import { Book } from "./book";

const STEP_MS = 250;

export * from "./engine";
export { VenueTicker } from "./venue-ticker";
export * from "./snapshot";
export { Book } from "./book";
export * from "./rng";
export * from "./sha256";
export * from "./merkle";
export * from "./pricing";
export * from "./accounts";
export * from "./fees";
export { OracleEngine } from "./oracle";
export { GovernanceEngine } from "./governance";
export { WithdrawalPipeline } from "./withdrawals";

export class SimVenue {
  readonly engine: SimEngine;
  private ticker: VenueTicker;
  private timer: ReturnType<typeof setInterval> | null = null;
  private subscribers = new Set<Symbol>();
  private lastWall = 0;

  constructor(config: Partial<EngineConfig> = {}) {
    this.engine = new SimEngine(config);
    this.ticker = new VenueTicker(this.engine);
  }

  start(): void {
    if (this.timer) return;
    this.lastWall = Date.now();
    this.timer = setInterval(() => {
      const wall = Date.now();
      const dt = Math.min(1000, wall - this.lastWall);
      this.lastWall = wall;
      this.ticker.step(dt);
    }, STEP_MS);
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  /* ── controls (VenueControl surface) ── */

  setSpeed(speed: number): void {
    this.engine.speed = speed;
    this.engine.running = speed > 0;
  }

  pause(): void {
    this.engine.running = false;
  }

  resume(): void {
    this.engine.running = true;
  }

  command(cmd: Command): void {
    this.engine.process(cmd);
  }

  governance(action: GovernanceAction): string | null {
    return this.engine.governance.act(action, this.engine.now);
  }

  withdrawalOp(op: {
    type: "withdrawal";
    op: "request" | "approve" | "cancel" | "settle_due" | "force_queue";
    id?: number;
    subaccount?: number;
    amount_quote_minor?: bigint;
    destination?: string;
  }): void {
    const e = this.engine;
    if (op.op === "request" && op.subaccount != null && op.amount_quote_minor != null) {
      const res = e.withdrawals.request(
        op.subaccount,
        op.amount_quote_minor,
        op.destination ?? "internal",
        e.spendableCash(op.subaccount),
        e.now,
      );
      if (typeof res === "string") {
        e.journalEvent({ type: "withdraw_rejected", subaccount: op.subaccount, requested: op.amount_quote_minor, reason: res, ts: e.now });
      }
    } else if (op.op === "approve" && op.id != null) {
      e.withdrawals.approve(op.id);
    } else if (op.op === "cancel" && op.id != null) {
      e.withdrawals.cancel(op.id);
    }
  }

  porBuild(): void {
    this.engine.porCached = buildPor(this.engine);
  }

  oracleInject(provider: string, price: bigint): void {
    this.engine.emitProviderObservation(provider, price);
    this.engine.noteOracleState();
  }

  reset(seed: number): void {
    this.stop();
    this.engine.porCached = { report: null, rows: [] };
    // Rebuild in place: replace the engine through a fresh venue internals.
    const venue = this as unknown as { engine: SimEngine; ticker: VenueTicker };
    const config: Partial<EngineConfig> = { ...this.engine.config, seed };
    venue.engine = new SimEngine(config);
    venue.ticker = new VenueTicker(venue.engine);
    this.start();
  }

  /* ── data plane ── */

  snapshot(): VenueSnapshot {
    return buildSnapshot(this.engine);
  }

  drain(): EngineOutputs & { books: Record<string, BookUpdateTagged> } {
    const out = this.engine.drain();
    const books: Record<string, BookUpdateTagged> = {};
    for (const sym of out.bookDirty) {
      if (!this.subscribers.has(sym)) continue;
      const book: Book | undefined = this.engine.books.get(sym);
      if (!book) continue;
      const delta = book.takeDelta();
      if (delta) books[sym] = delta;
    }
    // Books with no subscribers still need their dirty sets cleared.
    for (const sym of out.bookDirty) {
      if (this.subscribers.has(sym)) continue;
      const book = this.engine.books.get(sym);
      book?.takeDelta();
    }
    return { ...out, books };
  }

  subscribe(symbols: Symbol[]): void {
    this.subscribers = new Set(symbols);
  }

  /** Initial snapshot frames for freshly subscribed symbols. */
  bookSnapshots(symbols: Symbol[], depth = 20): Record<string, BookUpdateTagged> {
    const out: Record<string, BookUpdateTagged> = {};
    for (const sym of symbols) {
      const book = this.engine.books.get(sym);
      if (!book) continue;
      const bids = book.depth("bid", depth);
      const asks = book.depth("ask", depth);
      book.takeDelta(); // clear dirty — snapshot covers it
      out[sym] = { kind: "snapshot", seq: book.bookSeq, bids, asks };
    }
    return out;
  }

  static defaults(): EngineConfig {
    return { ...DEFAULT_ENGINE_CONFIG };
  }
}
