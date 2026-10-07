/**
 * Sim-engine smoke test — runs the venue for a few (accelerated) seconds and
 * asserts the core invariants the protocol cares about:
 *  - conservation: cash is only created/destroyed by deposits/settlements
 *  - the journal is append-only and sequenced
 *  - trades actually print; books stay consistent
 *  - merkle/PoR primitives verify
 */

import { describe, expect, test } from "bun:test";
import { SimVenue } from "./index";
import { limitOrder } from "@perp/types";
import { sha256Hex } from "./sha256";
import { buildMerkle, proveInclusion, verifyInclusion } from "./merkle";
import { ACCOUNT_DESCRIPTORS } from "./engine";

const DEPOSITS = ACCOUNT_DESCRIPTORS.reduce((s, d) => s + d.initial_deposit_quote_minor, 0n);

function runVenue(seconds: number, speed = 600): SimVenue {
  const v = new SimVenue({ seed: 42 });
  v.engine.speed = speed;
  const step = 250;
  for (let t = 0; t < seconds * 1000; t += step) {
    v["ticker"].step(step);
  }
  return v;
}

describe("sim engine", () => {
  test("sha256 matches known vectors", () => {
    expect(sha256Hex("")).toBe("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    expect(sha256Hex("abc")).toBe("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
  });

  test("merkle proofs verify", () => {
    const leaves = ["aa", "bb", "cc", "dd", "ee"].map((x) => sha256Hex(x));
    const { root } = buildMerkle(leaves);
    for (let i = 0; i < leaves.length; i++) {
      const proof = proveInclusion(leaves, i);
      expect(verifyInclusion(leaves[i]!, proof, root)).toBe(true);
    }
    expect(verifyInclusion(sha256Hex("zz"), proveInclusion(leaves, 0), root)).toBe(false);
  });

  test("venue runs and produces a live market", () => {
    const v = runVenue(20);
    const e = v.engine;
    expect(e.instruments.size).toBeGreaterThan(40);
    expect(e.stats.trades).toBeGreaterThan(0);
    const perp = e.books.get("BTC-PERP")!;
    expect(perp.bestBid()).not.toBeNull();
    expect(perp.bestAsk()).not.toBeNull();
    // Journal sequencing is strictly monotonic.
    let lastSeq = 0;
    for (const entry of e.journal) {
      expect(entry.seq).toBe(lastSeq + 1);
      lastSeq = entry.seq;
    }
    expect(lastSeq).toBeGreaterThan(100);
  });

  test("conservation: trades and funding never mint value", () => {
    const v = runVenue(20);
    const e = v.engine;
    let cash = 0n;
    for (const acc of e.accounts.values()) cash += acc.cash_quote_minor;
    // Unrealized PnL of open positions (margin accounting: cash + uPnL).
    let upnl = 0n;
    for (const acc of e.accounts.values()) {
      for (const [sym, pos] of acc.positions) {
        const inst = e.instruments.get(sym);
        if (!inst) continue;
        const mark = e.markOf(sym);
        const scale = 10n ** BigInt(inst.base_decimals);
        upnl += (BigInt(pos.signed_lots) * inst.lot_size_base_minor * (mark - pos.avg_entry_quote_minor)) / scale;
      }
    }
    let vaultNav = 0n;
    for (const vault of e.vaults.values()) vaultNav += vault.nav_quote_minor;
    const rev = e.revenue;
    const routed =
      rev.house_quote_minor + rev.insurance_quote_minor + rev.buyback_quote_minor + rev.insurance_overflow_quote_minor;
    // Insurance inventory marks (absorbed positions).
    let inventoryValue = 0n;
    for (const [sym, lots] of e.insurance_inventory) {
      const inst = e.instruments.get(sym);
      if (!inst) continue;
      inventoryValue += (BigInt(lots) * inst.lot_size_base_minor * e.markOf(sym)) / 10n ** BigInt(inst.base_decimals);
    }
    const sources = DEPOSITS + e.config.insurance_seed_quote_minor;
    const uses = cash + upnl + vaultNav + routed + e.insurance_balance + inventoryValue;
    // Exact to the minor unit except single-truncation division dust: each
    // fill rounds PnL/premium divisions independently on both sides of the
    // trade, so the bound scales with the fill count (≤1 minor unit per fill
    // plus a small constant). With the option chain actually quoting, the
    // fill count — and therefore the dust — is larger than in a dead market.
    const dust = BigInt(e.stats.trades) + 2n;
    expect(uses - sources >= -dust && uses - sources <= dust).toBe(true);
  });

  test("user order flows through the matching path", () => {
    const v = runVenue(5);
    const e = v.engine;
    const tradesBefore = e.stats.trades;
    const perp = e.books.get("BTC-PERP")!;
    const ask = perp.bestAsk()!;
    // Cross the spread (band-safe: within ±10% of the mark).
    v.command({ type: "place", request: limitOrder(7, "BTC-PERP", "bid", ask + 1, 10), now: e.now });
    expect(e.stats.trades).toBeGreaterThan(tradesBefore);
    const you = e.accounts.get(7)!;
    expect(you.positions.get("BTC-PERP")?.signed_lots ?? 0).toBeGreaterThan(0);
    // Close with a reduce-only ask (price irrelevant — sweeps the book).
    v.command({
      type: "place",
      request: limitOrder(7, "BTC-PERP", "ask", 1, 10, { reduce_only: true, tif: { kind: "ioc" } }),
      now: e.now,
    });
    expect(you.positions.get("BTC-PERP")?.signed_lots ?? 0).toBe(0);
  });

  test("user RFQ auto-quotes, executes and expires on the wall clock", () => {
    const v = runVenue(5);
    const e = v.engine;

    // ── a user-created RFQ receives dealer quotes (ticker-driven) ──
    v.command({
      type: "rfq_create",
      taker: 7,
      legs: [{ symbol: "BTC-PERP", side: "bid", qty_lots: 5 }],
      counterparties: [1, 2],
      min_total_cost_quote_minor: null,
      max_total_cost_quote_minor: null,
      ttl_ms: 30_000,
      now: e.now,
    });
    const rfqId = e.nextRfqIdPeek() - 1;
    // The dealer delay is wall-clock 800ms — actually wait it out.
    const waitWall = (ms: number) => { const t0 = Date.now(); while (Date.now() < t0 + ms) { /* spin */ } };
    waitWall(850);
    v["ticker"].step(1000);
    const rfq = e.rfqs.get(rfqId)!;
    expect(rfq.status).toBe("quoted");
    expect(rfq.quotes.length).toBe(2); // MM-1 and MM-2 both quote
    // TTLs are wall-clock interactive seconds, independent of venue speed.
    expect(rfq.expires_at).toBeGreaterThan(Date.now());

    // ── executing a live quote settles with trades + fill journal events ──
    const quote = rfq.quotes[0]!;
    v.command({ type: "rfq_execute", taker: 7, rfq_id: rfqId, quote_id: quote.quote_id, now: e.now });
    expect(rfq.status).toBe("executed");
    expect(rfq.trades?.length).toBe(1);
    const fillEvents = e.journal.filter((j) => j.event.type === "trade_executed").length;
    expect(fillEvents).toBeGreaterThanOrEqual(1);
    // Fills live in the durable fill log — visible even after journal churn.
    expect(e.fillLog.some((t) => t.taker_subaccount === 7 && t.symbol === "BTC-PERP")).toBe(true);

    // ── an unexecuted RFQ expires via the wall-clock sweep ──
    v.command({
      type: "rfq_create",
      taker: 7,
      legs: [{ symbol: "BTC-PERP", side: "ask", qty_lots: 3 }],
      counterparties: [1, 2],
      min_total_cost_quote_minor: null,
      max_total_cost_quote_minor: null,
      ttl_ms: 5, // 5ms — the sweep fires on the next ticker step
      now: e.now,
    });
    const dying = e.nextRfqIdPeek() - 1;
    expect(e.rfqs.get(dying)!.status).toBe("open");
    waitWall(20);
    v["ticker"].step(1000);
    expect(e.rfqs.get(dying)!.status).toBe("expired");
    const closed = e.journal.filter((j) => j.event.type === "rfq_closed");
    expect(closed.some((j) => (j.event as { reason?: string }).reason === "expired")).toBe(true);

    // ── accepting a dead quote is a visible rejection, not a silent no-op ──
    v.command({
      type: "rfq_create",
      taker: 7,
      legs: [{ symbol: "BTC-PERP", side: "bid", qty_lots: 2 }],
      counterparties: [1, 2],
      min_total_cost_quote_minor: null,
      max_total_cost_quote_minor: null,
      ttl_ms: 20_000,
      now: e.now,
    });
    const stale = e.nextRfqIdPeek() - 1;
    waitWall(850);
    v["ticker"].step(1000);
    const staleRfq = e.rfqs.get(stale)!;
    // Force-age the quote, then try to execute it.
    staleRfq.quotes[0]!.expires_at = Date.now() - 1;
    const rejections = e.journal.filter((j) => j.event.type === "rfq_rejected").length;
    v.command({ type: "rfq_execute", taker: 7, rfq_id: stale, quote_id: staleRfq.quotes[0]!.quote_id, now: e.now });
    expect(e.journal.filter((j) => j.event.type === "rfq_rejected").length).toBe(rejections + 1);
    expect(staleRfq.status).toBe("quoted"); // not executed
  });

  test("snapshot builds a full venue view", () => {
    const v = runVenue(8);
    const snap = v.snapshot();
    expect(snap.instruments.length).toBeGreaterThan(40);
    expect(snap.accounts.length).toBe(7);
    expect(snap.markets.length).toBe(snap.instruments.length);
    expect(snap.oracle.BTC).toBeDefined();
    expect(snap.vaults.length).toBeGreaterThanOrEqual(1);
    v.porBuild();
    expect(v.engine.porCached.report).not.toBeNull();
    expect(v.engine.porCached.rows.length).toBe(7);
  });

  test("same seed → same journal length (command determinism)", () => {
    const a = runVenue(6);
    const b = runVenue(6);
    expect(a.engine.journal.length).toBe(b.engine.journal.length);
    expect(a.engine.stats.trades).toBe(b.engine.stats.trades);
    expect(a.engine.spot_quote_minor).toBe(b.engine.spot_quote_minor);
  });
});
