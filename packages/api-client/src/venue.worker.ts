/**
 * The venue worker — hosts the deterministic simulator off the main thread.
 * Commands in, batched market data + journal out. Identical message contract
 * to the production socket.io gateway (VenueControl / VenueMessage).
 */

import type { VenueControl, VenueMessage } from "@perp/types";
import { SimVenue, buildMarketRows } from "@perp/sim-engine";
import type { Trade } from "@perp/types";

let venue: SimVenue | null = null;
let subscribed: string[] = [];
let drainTimer: ReturnType<typeof setInterval> | null = null;
let marketTimer: ReturnType<typeof setInterval> | null = null;
let snapshotTimer: ReturnType<typeof setInterval> | null = null;

const DRAIN_MS = 80;
const MARKET_ROWS_MS = 700;
const SNAPSHOT_MS = 2500;

function ensureVenue(seed?: number): SimVenue {
  if (!venue) {
    venue = new SimVenue({ seed });
    venue.start();
  }
  return venue;
}

function post(msg: VenueMessage): void {
  (self as unknown as Worker).postMessage(msg);
}

function drain(): void {
  if (!venue) return;
  const out: ReturnType<SimVenue["drain"]> = venue.drain();
  if (Object.keys(out.books).length > 0) {
    post({ channel: "books", updates: out.books });
  }
  if (out.prints.length > 0) {
    post({
      channel: "prints",
      prints: (out.prints as Trade[]).map((t) => ({
        seq: t.seq,
        ts: t.ts,
        symbol: t.symbol,
        price_ticks: t.price_ticks,
        qty_lots: t.qty_lots,
        maker_side: t.maker_side,
        notional_quote_minor: t.notional_quote_minor,
      })),
    });
  }
  if (out.journal.length > 0) {
    post({ channel: "journal", entries: out.journal });
  }
  for (const id of out.accountDirty) {
    const acc = venue.engine.accounts.get(id);
    if (!acc) continue;
    const m = venue.engine.marginOf(id);
    post({
      channel: "account",
      view: {
        id,
        cash_quote_minor: acc.cash_quote_minor,
        equity_quote_minor: m.equity,
        maintenance_quote_minor: m.maintenance,
        initial_quote_minor: m.initial,
        order_margin_quote_minor: m.order_margin,
        health: m.health,
        fees_paid_quote_minor: acc.fees_paid_quote_minor,
        funding_received_quote_minor: acc.funding_pnl_quote_minor,
      },
    });
  }
  if (out.marketDirty) {
    post({ channel: "markets", rows: buildMarketRows(venue.engine) });
  }
}

self.onmessage = (ev: MessageEvent<VenueControl>) => {
  const msg = ev.data;
  switch (msg.type) {
    case "connect": {
      ensureVenue(msg.seed);
      subscribed = ["BTC-PERP"];
      post({ channel: "bootstrap", snapshot: venue!.snapshot() });
      post({ channel: "books", updates: venue!.bookSnapshots(subscribed) });
      startDrain();
      break;
    }
    case "reset": {
      if (venue) {
        venue.reset(msg.seed ?? Date.now() % 1_000_000);
      } else {
        ensureVenue(msg.seed);
      }
      post({ channel: "bootstrap", snapshot: venue!.snapshot() });
      post({ channel: "books", updates: venue!.bookSnapshots(subscribed) });
      break;
    }
    case "set_speed": {
      const v = ensureVenue();
      v.setSpeed(msg.speed);
      break;
    }
    case "pause": {
      ensureVenue().pause();
      break;
    }
    case "resume": {
      ensureVenue().resume();
      break;
    }
    case "step": {
      // Manual stepping is a console power feature: advance one step.
      const v = ensureVenue();
      v.pause();
      (v as unknown as { ticker: { step(ms: number): void } }).ticker.step(msg.ms ?? 250);
      drain();
      break;
    }
    case "subscribe": {
      const v = ensureVenue();
      subscribed = msg.symbols;
      v.subscribe(msg.symbols);
      post({ channel: "books", updates: v.bookSnapshots(msg.symbols) });
      break;
    }
    case "command": {
      const v = ensureVenue();
      v.command(msg.command);
      drain();
      break;
    }
    case "governance": {
      const v = ensureVenue();
      const err = v.governance(msg.action);
      if (err) post({ channel: "error", message: `governance: ${err}` });
      break;
    }
    case "withdrawal": {
      const v = ensureVenue();
      v.withdrawalOp(msg);
      break;
    }
    case "por_build": {
      const v = ensureVenue();
      v.porBuild();
      // Push the fresh report through as a full snapshot (simplest channel).
      post({ channel: "snapshot", snapshot: v.snapshot() });
      break;
    }
    case "oracle_inject": {
      const v = ensureVenue();
      v.oracleInject(msg.provider, msg.price_quote_minor);
      break;
    }
    case "oracle_toggle_quarantine": {
      const v = ensureVenue();
      const engine = v.engine;
      const st = engine.oracle;
      st.toggleQuarantine(msg.provider);
      engine.noteOracleState();
      break;
    }
    case "request_snapshot": {
      const v = ensureVenue();
      post({ channel: "snapshot", snapshot: v.snapshot() });
      post({ channel: "books", updates: v.bookSnapshots(subscribed) });
      break;
    }
  }
};

function startDrain(): void {
  if (drainTimer) return;
  drainTimer = setInterval(drain, DRAIN_MS);
  marketTimer = setInterval(() => {
    if (venue) {
      venue.engine.out.marketDirty = true;
    }
  }, MARKET_ROWS_MS);
  // Full state snapshots keep every view (positions, orders, vaults,
  // governance…) fresh. SFPM margin over 7 accounts is a few ms.
  snapshotTimer = setInterval(() => {
    if (venue) {
      post({ channel: "snapshot", snapshot: venue.snapshot() });
    }
  }, SNAPSHOT_MS);
}
