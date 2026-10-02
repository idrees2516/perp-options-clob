/**
 * VenueTicker — drives the autonomous market: the same command surface a
 * real gateway would push.
 *
 * Pacing discipline:
 *  - market liveliness (taker arrivals, MM refresh, oracle prints) follows
 *    REAL time so the tape feels alive at any sim speed;
 *  - economic time (funding, epochs, expiries, reviews) follows SIM time
 *    and therefore accelerates with the speed multiplier;
 *  - scripted demo beats (oracle defense, liquidation cascade) fire once,
 *    on the real-time line.
 */

import type {
  Command,
  JournalEntry,
  MarketRow,
  SubaccountId,
  Symbol,
} from "@perp/types";
import { limitOrder } from "@perp/types";
import type { SimEngine } from "./engine";
import { hashSeed } from "./rng";

const PERP = "BTC-PERP";

/** Minimal account shape used by settlement helpers. */
interface SimAccountLike {
  id: number;
  cash_quote_minor: bigint;
  funding_pnl_quote_minor: bigint;
  positions: Map<Symbol, { symbol: Symbol; signed_lots: number; avg_entry_quote_minor: bigint; realized_pnl_quote_minor: bigint }>;
}

function absBig(x: bigint): bigint {
  return x < 0n ? -x : x;
}

function baseScaleOf(e: { instruments: Map<Symbol, { base_decimals: number }> }, symbol: Symbol): bigint {
  return 10n ** BigInt(e.instruments.get(symbol)?.base_decimals ?? 0);
}

function baseDecimalsOf(e: { instruments: Map<Symbol, { base_decimals: number }> }, symbol: Symbol): number {
  return e.instruments.get(symbol)?.base_decimals ?? 0;
}

export class VenueTicker {
  private engine: SimEngine;
  private lastOracleWall = 0;
  private lastMmWall = 0;
  private lastRfqBeatWall = 0;
  private lastWithdrawalWall = 0;
  private lastMarketRowsWall = 0;
  private lastRewardSim = 0;
  private lastListSim = 0;
  private rewardScores = new Map<SubaccountId, number>();
  private optionSettleWindows = new Map<Symbol, { sum_intrinsic: bigint; n: number }>();
  /** Spot regime target (null = pure GBM). */
  private spotTarget: bigint | null = null;
  private beatIndex = 0;
  private rfqQueue: { id: number; respondAtWall: number }[] = [];
  private rfqAutoExecuteAt = new Map<number, number>();
  private breakerWatch = new Map<Symbol, number>();

  constructor(engine: SimEngine) {
    this.engine = engine;
    this.scheduleBeats();
  }

  /* ═══════════════════ scripted demo beats ═══════════════════ */

  private beats: { at: number; label: string; fn: () => void }[] = [];

  private scheduleBeats(): void {
    const e = this.engine;
    this.beats = [
      {
        at: 20_000,
        label: "market-making",
        fn: () => {
          e.phase = "market-making";
          // MMs enroll in the tier program.
          e.process({ type: "mm_tier_enroll", subaccount: 1, now: e.now });
          e.process({ type: "mm_tier_enroll", subaccount: 2, now: e.now });
          // Trader-5 seeds the LP vault (G-16 demo flow).
          e.process({ type: "vault_subscribe", vault_id: 1, subaccount: 5, amount_quote_minor: 50_000_00n, now: e.now });
        },
      },
      {
        at: 45_000,
        label: "taker-flow",
        fn: () => {
          e.phase = "taker-flow";
        },
      },
      {
        at: 150_000,
        label: "oracle-defense",
        fn: () => {
          // Compromised feed prints $160k while spot is ~$80k —
          // deviation quarantine trips, the median mark never moves.
          e.phase = "oracle-defense";
          e.emitProviderObservation("apis3", 160_000_00n);
        },
      },
      {
        at: 360_000,
        label: "shock",
        fn: () => {
          // Degen-6 bids 8,000 lots at 80,100 (~20× leverage on $45k).
          e.phase = "shock";
          e.process({
            type: "place",
            request: limitOrder(6, PERP, "bid", 80_100, 8_000),
            now: e.now,
          });
        },
      },
      {
        at: 372_000,
        label: "shock-walk",
        fn: () => {
          // Price walks down in ≤3% steps → liquidation cascade.
          this.spotTarget = 71_200_00n;
        },
      },
      {
        at: 480_000,
        label: "recovery",
        fn: () => {
          e.phase = "recovery";
          this.spotTarget = 84_000_00n;
        },
      },
      {
        at: 660_000,
        label: "long-run",
        fn: () => {
          e.phase = "long-run";
          this.spotTarget = null;
        },
      },
    ];
  }

  /* ═══════════════════ step ═══════════════════ */

  step(dtRealMs: number): void {
    const e = this.engine;
    if (!e.running) return;
    const wall = Date.now();
    const dtSim = dtRealMs * e.speed;
    e.now += dtSim;
    e.ticks += 1;

    this.runBeats();

    // ── wall-clock cadences (liveliness independent of sim speed) ──
    if (wall - this.lastOracleWall >= 1_200) {
      this.lastOracleWall = wall;
      this.oracleStep();
    }
    if (wall - this.lastMmWall >= 1_500) {
      this.lastMmWall = wall;
      this.mmRefresh();
    }
    this.takerArrivals(dtRealMs);
    if (wall - this.lastRfqBeatWall >= 90_000 && e.ticks > 30) {
      this.lastRfqBeatWall = wall;
      this.rfqBeat();
    }
    if (wall - this.lastWithdrawalWall >= 5_000) {
      this.lastWithdrawalWall = wall;
      this.withdrawalSweep();
    }
    if (wall - this.lastMarketRowsWall >= 600) {
      this.lastMarketRowsWall = wall;
      e.out.marketDirty = true;
    }

    // ── sim-time systems (accelerate with speed) ──
    this.fundingTwapSample();
    this.fundingBoundaries();
    this.everlastingRolls();
    this.expiries();
    this.gtdSweep();
    this.stopTriggers();
    this.twapSlices();
    this.exerciseSettlements();
    this.rewards();
    this.mmSampling();
    this.mmReviews();
    e.settleVaultEpochs(e.now);
    this.blocksPrint();
    this.liquidationSweep();
    this.breakerChecks();
    this.autoList();
    this.rfqAutoQuote();
    this.rfqAutoExecute();
    this.pruneVolumes();
  }

  private runBeats(): void {
    const e = this.engine;
    const elapsed = Date.now() - e.startedAt;
    while (this.beatIndex < this.beats.length && elapsed >= this.beats[this.beatIndex]!.at) {
      const beat = this.beats[this.beatIndex]!;
      this.beatIndex += 1;
      try {
        beat.fn();
      } catch (err) {
        console.warn("beat failed", beat.label, err);
      }
    }
  }

  /* ═══════════════════ oracle ═══════════════════ */

  private oracleStep(): void {
    const e = this.engine;
    // Spot process: GBM + optional scripted walk target.
    let spotNum = Number(e.spot_quote_minor);
    const vol = 0.0000316 * Math.sqrt(1200); // per 1.2s step
    const drift = this.spotTarget != null
      ? Math.sign(Number(this.spotTarget) - spotNum) * Math.min(Math.abs(Number(this.spotTarget) - spotNum), spotNum * 0.03)
      : e.rng.normal() * vol;
    spotNum *= 1 + drift;
    if (this.spotTarget != null && Math.abs(spotNum - Number(this.spotTarget)) < 100) {
      this.spotTarget = null;
    }
    const base = BigInt(Math.round(spotNum));
    for (const p of e.config.providers) {
      const jitter = 1 + e.rng.normal() * 0.0003; // ±3bp
      e.emitProviderObservation(p, BigInt(Math.round(Number(base) * jitter)));
    }
    e.noteOracleState();
    if (e.oracle.halted) {
      e.journalEvent({ type: "market_halted", base_symbol: e.config.base_symbol, ts: e.now });
    }
  }

  /* ═══════════════════ market makers ═══════════════════ */

  private mmRefresh(): void {
    const e = this.engine;
    this.mmLiquidityProgram();
    const mark = e.spot_quote_minor;
    const markTicks = Number(mark / 100n); // $1 ticks

    // Perp: MM-1 wide, MM-2 tight — the demo ladder.
    this.quoteAround(1, PERP, markTicks, 100, 4_000);
    this.quoteAround(2, PERP, markTicks, 50, 4_000);

    // Options: near-ATM slice of the chain.
    const near = this.nearSymbols(14);
    let i = 0;
    for (const sym of near) {
      const inst = e.instruments.get(sym);
      if (!inst || inst.kind !== "option") continue;
      const markTicksOpt = Number(e.markOf(sym) / inst.tick_size_quote_minor);
      if (i % 2 === 0) this.quoteAround(1, sym, markTicksOpt, 100, 120);
      this.quoteAround(2, sym, markTicksOpt, 60, 150);
      i += 1;
    }
  }

  /**
   * Liquidity program — long-running venues keep their makers capitalized.
   *
   * A demo venue that runs for wall-hours at high speed grinds the MMs down
   * (inventory marks, funding, fees); once their quotes stop passing the
   * margin gate the book goes dark and every terminal view empties. This is
   * the venue-side MM incentive program: when a maker's equity dips under
   * half its initial allocation, credit it back up to the program level.
   * The deposits journal explicitly, so the flow stays auditable.
   */
  private mmLiquidityProgram(): void {
    const e = this.engine;
    const target = 5_000_000_00n;
    const floor = 2_500_000_00n;
    for (const sub of [1, 2] as const) {
      const equity = e.marginOf(sub).equity;
      if (equity >= floor) continue;
      const topUp = target - equity;
      if (topUp <= 0n) continue;
      e.process({ type: "deposit", subaccount: sub, amount_quote_minor: topUp });
    }
  }

  private quoteAround(sub: number, symbol: Symbol, markTicks: number, halfSpreadTicks: number, sizeLots: number): void {
    const e = this.engine;
    e.cancelAll(sub, symbol);
    const jitter = Math.floor(e.rng.next() * 3);
    const size = sizeLots + Math.floor(e.rng.next() * 200) - 100;
    if (size <= 0) return;
    e.process({
      type: "place",
      request: limitOrder(sub, symbol, "bid", Math.max(1, markTicks - halfSpreadTicks - jitter), size, { post_only: true }),
      now: e.now,
    });
    e.process({
      type: "place",
      request: limitOrder(sub, symbol, "ask", markTicks + halfSpreadTicks + jitter, size, { post_only: true }),
      now: e.now,
    });
  }

  /** Option symbols nearest ATM across tenors. */
  private nearSymbols(n: number): Symbol[] {
    const e = this.engine;
    const opts: { sym: Symbol; dist: number }[] = [];
    for (const [sym, inst] of e.instruments) {
      if (inst.kind !== "option") continue;
      const dist = Math.abs(Number(inst.strike_quote_minor) - Number(e.spot_quote_minor));
      opts.push({ sym, dist });
    }
    opts.sort((a, b) => a.dist - b.dist);
    return opts.slice(0, n).map((o) => o.sym);
  }

  /* ═══════════════════ takers ═══════════════════ */

  private takerArrivals(dtRealMs: number): void {
    const e = this.engine;
    const rate = 1.1 / 1000; // per ms
    let expected = rate * dtRealMs;
    while (expected > 0) {
      if (e.rng.next() > expected) break;
      expected -= 1;
      this.spawnTaker();
    }
  }

  private spawnTaker(): void {
    const e = this.engine;
    const sub = e.rng.pick([3, 4, 5, 3, 4]);
    const isOption = e.rng.chance(0.3);
    let symbol: Symbol = PERP;
    if (isOption) {
      const near = this.nearSymbols(6);
      if (near.length) symbol = e.rng.pick(near);
    }
    const inst = e.instruments.get(symbol);
    const book = e.books.get(symbol);
    if (!inst || !book) return;
    // Cross the live book (not the mark) so takers actually lift liquidity.
    const bb = book.bestBid();
    const ba = book.bestAsk();
    const markTicks = Number(e.markOf(symbol) / inst.tick_size_quote_minor);
    // inventory mean-reversion bias
    let bias = 0;
    for (const s of [3, 4, 5]) {
      bias += e.accounts.get(s)?.positions.get(PERP)?.signed_lots ?? 0;
    }
    const side = e.rng.chance(0.5 + Math.max(-0.3, Math.min(0.3, -bias / 6000))) ? "bid" : "ask";
    const cross = e.rng.int(0, 3);
    let price: number;
    if (side === "bid") {
      price = ba != null ? ba + cross : Math.max(1, markTicks + cross + 2);
    } else {
      price = bb != null ? bb - cross : Math.max(1, markTicks - cross - 2);
    }
    price = Math.max(1, price);
    const qty = inst.kind === "option" ? e.rng.int(1, 8) : e.rng.int(5, 45);
    const ioc = e.rng.chance(0.3);
    e.process({
      type: "place",
      request: limitOrder(sub, symbol, side, price, qty, ioc ? { tif: { kind: "ioc" } } : {}),
      now: e.now,
    });
  }

  /* ═══════════════════ funding ═══════════════════ */

  private fundingTwapSample(): void {
    const e = this.engine;
    const t = e.fundingTrackers.get(PERP);
    if (!t) return;
    const mid = e.bookMidOrMark(PERP);
    t.sum_mid += mid;
    t.sum_index += e.spot_quote_minor;
    t.n += 1;
    // Everlasting premium TWAPs.
    for (const [sym, roll] of e.everlastingRoll) {
      const inst = e.instruments.get(sym);
      if (!inst) continue;
      roll.sum_premium += e.markOf(sym);
      roll.n += 1;
      void inst;
    }
    // Option settlement windows (last 30 minutes before expiry).
    for (const [sym, inst] of e.instruments) {
      if (inst.kind !== "option" || inst.variant !== "dated") continue;
      if (e.now >= inst.expiry_ts_ms - 30 * 60 * 1000 && e.now < inst.expiry_ts_ms) {
        const spot = Number(e.spot_quote_minor);
        const strike = Number(inst.strike_quote_minor);
        const intrinsic = Math.max(0, inst.kind_of === "call" ? spot - strike : strike - spot);
        const w = this.optionSettleWindows.get(sym) ?? { sum_intrinsic: 0n, n: 0 };
        w.sum_intrinsic += BigInt(Math.round(intrinsic));
        w.n += 1;
        this.optionSettleWindows.set(sym, w);
      }
    }
  }

  private fundingBoundaries(): void {
    const e = this.engine;
    const t = e.fundingTrackers.get(PERP);
    if (!t) return;
    if (e.now - t.last_boundary < 8 * 3600 * 1000) return;
    t.last_boundary = e.now;
    if (t.n === 0) return;
    const markTwap = t.sum_mid / BigInt(t.n);
    const indexTwap = t.sum_index / BigInt(t.n);
    const premiumBps = Number(((markTwap - indexTwap) * 10_000n) / indexTwap);
    const clamped = Math.max(-5, Math.min(5, premiumBps));
    const rate = Math.max(-75, Math.min(75, 1 + clamped));
    t.last_rate_bps = rate;
    t.premium_bps = clamped;
    t.sum_mid = 0n;
    t.sum_index = 0n;
    t.n = 0;
    e.stats.funding_intervals += 1;
    e.journalEvent({ type: "funding", payload: { symbol: PERP, rate_bps: rate, ts: e.now } });
    e.fundingHistory.push({
      symbol: PERP,
      ts: e.now,
      rate_bps: rate,
      premium_bps: clamped,
      interest_bps: 1,
      twap_mark_quote_minor: markTwap,
      twap_index_quote_minor: indexTwap,
    });
    if (e.fundingHistory.length > 300) e.fundingHistory.shift();
    // Zero-sum application: longs pay shorts when rate > 0.
    // Rounding residual is assigned to the largest payer so Σ credits == 0.
    const inst = e.instruments.get(PERP)!;
    const scale = 10n ** BigInt(inst.base_decimals);
    const payments: { acc: SimAccountLike; credit: bigint }[] = [];
    let residual = 0n;
    let biggest = -1;
    let biggestAbs = 0n;
    for (const acc of e.accounts.values()) {
      const pos = acc.positions.get(PERP);
      if (!pos || pos.signed_lots === 0) continue;
      const payment = (BigInt(pos.signed_lots) * inst.lot_size_base_minor * indexTwap * BigInt(rate)) / (scale * 10_000n);
      const credit = -payment;
      residual += credit;
      if (absBig(credit) > biggestAbs) {
        biggestAbs = absBig(credit);
        biggest = payments.length;
      }
      payments.push({ acc, credit });
    }
    if (payments.length > 0 && biggest >= 0 && residual !== 0n) {
      payments[biggest]!.credit -= residual;
    }
    for (const { acc, credit } of payments) {
      acc.cash_quote_minor += credit;
      acc.funding_pnl_quote_minor += credit;
      e.out.accountDirty.add(acc.id);
      e.journalEvent({ type: "funding_flow", payload: { subaccount: acc.id, symbol: PERP, credit_quote_minor: credit } });
    }
  }

  private everlastingRolls(): void {
    const e = this.engine;
    for (const [sym, roll] of e.everlastingRoll) {
      if (e.now - roll.last_roll < 3600 * 1000) continue;
      roll.last_roll = e.now;
      if (roll.n === 0) continue;
      const premiumTwap = roll.sum_premium / BigInt(roll.n);
      roll.sum_premium = 0n;
      roll.n = 0;
      const inst = e.instruments.get(sym);
      if (!inst || inst.kind !== "option") continue;
      const rateBps = Number((premiumTwap * 10_000n) / e.spot_quote_minor);
      roll.last_rate_bps = rateBps;
      e.journalEvent({ type: "funding", payload: { symbol: sym, rate_bps: rateBps, ts: e.now } });
      e.fundingHistory.push({
        symbol: sym,
        ts: e.now,
        rate_bps: rateBps,
        premium_bps: rateBps,
        interest_bps: 0,
        twap_mark_quote_minor: premiumTwap,
        twap_index_quote_minor: e.spot_quote_minor,
      });
      if (e.fundingHistory.length > 300) e.fundingHistory.shift();
      const scale = 10n ** BigInt(inst.base_decimals);
      const flows: { acc: SimAccountLike; credit: bigint }[] = [];
      let residual = 0n;
      let biggest = -1;
      let biggestAbs = 0n;
      for (const acc of e.accounts.values()) {
        const pos = acc.positions.get(sym);
        if (!pos || pos.signed_lots === 0) continue;
        const payment = (BigInt(pos.signed_lots) * inst.lot_size_base_minor * premiumTwap) / scale;
        const credit = -payment;
        residual += credit;
        if (absBig(credit) > biggestAbs) {
          biggestAbs = absBig(credit);
          biggest = flows.length;
        }
        flows.push({ acc, credit });
      }
      if (flows.length > 0 && biggest >= 0 && residual !== 0n) {
        flows[biggest]!.credit -= residual;
      }
      for (const { acc, credit } of flows) {
        acc.cash_quote_minor += credit;
        acc.funding_pnl_quote_minor += credit;
        e.out.accountDirty.add(acc.id);
        e.journalEvent({ type: "funding_flow", payload: { subaccount: acc.id, symbol: sym, credit_quote_minor: credit } });
      }
    }
  }

  /* ═══════════════════ expiry ═══════════════════ */

  private expiries(): void {
    const e = this.engine;
    const expired: Symbol[] = [];
    for (const [sym, inst] of e.instruments) {
      if (inst.kind !== "option" || inst.variant !== "dated") continue;
      if (e.now >= inst.expiry_ts_ms) expired.push(sym);
    }
    for (const sym of expired) {
      const inst = e.instruments.get(sym)!;
      if (inst.kind !== "option") continue;
      const w = this.optionSettleWindows.get(sym);
      const settlement = w && w.n > 0 ? w.sum_intrinsic / BigInt(w.n) : 0n;
      this.optionSettleWindows.delete(sym);
      for (const acc of e.accounts.values()) {
        const pos = acc.positions.get(sym);
        if (!pos || pos.signed_lots === 0) continue;
        const signedLots = pos.signed_lots;
        this.applySettlement(acc, sym, signedLots, settlement, inst.lot_size_base_minor, inst.base_decimals);
        e.stats.options_settled += 1;
        e.journalEvent({
          type: "option_expiry",
          payload: {
            subaccount: acc.id,
            symbol: sym,
            signed_lots: signedLots,
            settlement_quote_minor: settlement,
            payout_quote_minor: (BigInt(signedLots) * inst.lot_size_base_minor * settlement) / 10n ** BigInt(inst.base_decimals),
          },
        });
        e.out.accountDirty.add(acc.id);
      }
      // Delist.
      e.instruments.delete(sym);
      e.books.delete(sym);
      e.sessionOpen.delete(sym);
      e.ivs.delete(sym);
      e.journalEvent({ type: "option_delisted", symbol: sym });
    }
  }

  /** Reduce a position by `lots` at `priceMinorPerBase`, banking realized PnL. */
  private applySettlement(
    acc: { positions: Map<Symbol, { symbol: Symbol; signed_lots: number; avg_entry_quote_minor: bigint; realized_pnl_quote_minor: bigint }>; cash_quote_minor: bigint },
    symbol: Symbol,
    lots: number,
    priceMinorPerBase: bigint,
    lotBaseMinor: bigint,
    baseDecimals: number,
  ): void {
    const pos = acc.positions.get(symbol);
    if (!pos || lots === 0) return;
    // A settlement always reduces the existing position toward zero.
    const isReduction = Math.sign(lots) === -Math.sign(pos.signed_lots);
    if (!isReduction) return;
    const closed = Math.min(Math.abs(lots), Math.abs(pos.signed_lots));
    const sign = Math.sign(pos.signed_lots);
    const pnl = (BigInt(closed) * lotBaseMinor * (priceMinorPerBase - pos.avg_entry_quote_minor) * BigInt(sign)) / (10n ** BigInt(baseDecimals));
    pos.realized_pnl_quote_minor += pnl;
    acc.cash_quote_minor += pnl;
    pos.signed_lots += lots; // settlement callers pass exact closes
    if (pos.signed_lots === 0) {
      pos.avg_entry_quote_minor = 0n;
      acc.positions.delete(symbol);
    }
  }

  /* ═══════════════════ gtd / stops / twap ═══════════════════ */

  private gtdSweep(): void {
    const e = this.engine;
    for (const o of [...e.orders.values()]) {
      if (o.tif.kind === "gtd" && o.tif.until <= e.now) e.cancelOrder(o.id, "expired");
    }
    for (const [id, stop] of [...e.pendingStops]) {
      if (stop.order.tif.kind === "gtd" && stop.order.tif.until <= e.now) e.cancelOrder(id, "expired");
    }
  }

  private stopTriggers(): void {
    const e = this.engine;
    for (const [id, stop] of [...e.pendingStops]) {
      const req = stop.order;
      const inst = e.instruments.get(req.symbol);
      if (!inst) {
        e.pendingStops.delete(id);
        continue;
      }
      const mark = e.markOf(req.symbol);
      const markTicks = Number(mark / inst.tick_size_quote_minor);
      const ot = req.order_type;
      let triggered = false;
      let armedOrder: typeof req | null = null;
      if (ot.kind === "stop_market" || ot.kind === "stop_limit") {
        if (req.side === "bid" && markTicks >= ot.trigger_price) triggered = true;
        if (req.side === "ask" && markTicks <= ot.trigger_price) triggered = true;
        armedOrder =
          ot.kind === "stop_market"
            ? { ...req, order_type: { kind: "market" }, price_ticks: null }
            : { ...req, order_type: { kind: "limit" }, price_ticks: ot.limit_price };
      } else if (ot.kind === "trailing_stop_market" || ot.kind === "trailing_stop_limit") {
        // Track the extreme since placement.
        if (stop.extreme == null) stop.extreme = mark;
        else stop.extreme =
          req.side === "bid"
            ? stop.extreme < mark ? mark : stop.extreme
            : stop.extreme > mark ? mark : stop.extreme;
        const extremeTicks = Number(stop.extreme / inst.tick_size_quote_minor);
        if (req.side === "bid" && extremeTicks - markTicks >= ot.offset_ticks) triggered = true;
        if (req.side === "ask" && markTicks - extremeTicks >= ot.offset_ticks) triggered = true;
        armedOrder =
          ot.kind === "trailing_stop_market"
            ? { ...req, order_type: { kind: "market" }, price_ticks: null }
            : { ...req, order_type: { kind: "limit" }, price_ticks: ot.limit_ticks };
      }
      if (triggered && armedOrder) {
        e.pendingStops.delete(id);
        e.journalEvent({ type: "trailing_updated", order_id: id, subaccount: req.subaccount, symbol: req.symbol, extreme_quote_minor: stop.extreme ?? mark, ts: e.now });
        e.process({ type: "place", request: armedOrder, now: e.now });
      }
    }
  }

  private twapSlices(): void {
    const e = this.engine;
    for (const p of [...e.twapParents.values()]) {
      if (p.state !== "running" || e.now < p.next_slice_ts) continue;
      const sliceLots = Math.ceil(p.total_lots / p.slices);
      const remaining = p.total_lots - p.lots_placed;
      const lots = Math.min(sliceLots, remaining);
      if (lots > 0) {
        const markTicks = Number(e.markOf(p.symbol) / (e.instruments.get(p.symbol)?.tick_size_quote_minor ?? 100n));
        e.process({
          type: "place",
          request: limitOrder(p.sub, p.symbol, p.side, p.limit_ticks ?? Math.max(1, markTicks), lots),
          now: e.now,
        });
        p.placed += 1;
        p.lots_placed += lots;
        p.next_slice_ts = e.now + p.slice_interval_ms;
        e.journalEvent({ type: "twap_sliced", parent_id: p.parent_id, request: limitOrder(p.sub, p.symbol, p.side, p.limit_ticks ?? markTicks, lots), slice_index: p.placed, ts: e.now });
      }
      if (p.lots_placed >= p.total_lots) {
        p.state = "completed";
        e.journalEvent({ type: "twap_closed", parent_id: p.parent_id, subaccount: p.sub, reason: "completed", placed_lots: p.lots_placed, ts: e.now });
      }
    }
  }

  /* ═══════════════════ exercises ═══════════════════ */

  private exerciseSettlements(): void {
    const e = this.engine;
    for (const [reqId, ex] of [...e.pendingExercises]) {
      const inst = e.instruments.get(ex.symbol);
      if (!inst || inst.kind !== "option") {
        e.pendingExercises.delete(reqId);
        continue;
      }
      // TWAP intrinsic sampling through the window.
      const spot = Number(e.spot_quote_minor);
      const strike = Number(inst.strike_quote_minor);
      const intrinsic = Math.max(0, inst.kind_of === "call" ? spot - strike : strike - spot);
      ex.twap_sum += BigInt(Math.round(intrinsic));
      ex.twap_n += 1;
      if (e.now < ex.settle_at) continue;
      e.pendingExercises.delete(reqId);

      const scale = 10n ** BigInt(inst.base_decimals);
      const intrinsicTwap = ex.twap_n > 0 ? ex.twap_sum / BigInt(ex.twap_n) : 0n;
      const acc = e.accounts.get(ex.subaccount)!;
      const pos = acc.positions.get(ex.symbol);
      if (!pos || pos.signed_lots < ex.lots) continue;
      const gross = (BigInt(ex.lots) * inst.lot_size_base_minor * intrinsicTwap) / scale;
      const fee = (gross * 5n) / 10_000n; // 5 bps
      this.applySettlement(acc, ex.symbol, ex.lots, intrinsicTwap, inst.lot_size_base_minor, inst.base_decimals);
      acc.cash_quote_minor -= fee;
      e.stats.exercises_settled += 1;

      // Assign matching shorts pro-rata with largest-remainder distribution.
      const shorts: { sub: SubaccountId; lots: number }[] = [];
      for (const [sid, a] of e.accounts) {
        const sp = a.positions.get(ex.symbol);
        if (sp && sp.signed_lots < 0) shorts.push({ sub: sid, lots: -sp.signed_lots });
      }
      const assignments: { subaccount: SubaccountId; lots: number; charge_quote_minor: bigint }[] = [];
      const totalShort = shorts.reduce((s, x) => s + x.lots, 0);
      if (totalShort > 0) {
        const raw = shorts.map((s) => ({ sub: s.sub, exact: (s.lots * ex.lots) / totalShort, rem: (s.lots * ex.lots) % totalShort }));
        let assigned = raw.reduce((s, r) => s + r.exact, 0);
        const order = raw.map((r, i) => ({ i, rem: r.rem })).sort((a, b) => b.rem - a.rem);
        let oi = 0;
        while (assigned < ex.lots && order.length > 0) {
          raw[order[oi % order.length]!.i]!.exact += 1;
          assigned += 1;
          oi += 1;
        }
        for (const r of raw) {
          if (r.exact <= 0) continue;
          const shortAcc = e.accounts.get(r.sub)!;
          const charge = (BigInt(r.exact) * inst.lot_size_base_minor * intrinsicTwap) / scale;
          this.applySettlement(shortAcc, ex.symbol, -r.exact, intrinsicTwap, inst.lot_size_base_minor, inst.base_decimals);
          assignments.push({ subaccount: r.sub, lots: r.exact, charge_quote_minor: charge });
          e.out.accountDirty.add(r.sub);
        }
      }

      e.journalEvent({
        type: "option_exercised",
        payload: {
          request_id: reqId,
          subaccount: ex.subaccount,
          symbol: ex.symbol,
          requested_lots: ex.lots,
          settled_lots: ex.lots,
          settlement_quote_minor: intrinsicTwap,
          intrinsic_per_lot_quote_minor: intrinsicTwap,
          gross_payout_quote_minor: gross,
          exercise_fee_quote_minor: fee,
          assignments,
          ts: e.now,
        },
      });
      e.out.accountDirty.add(ex.subaccount);
    }
  }

  /* ═══════════════════ rewards / MM ═══════════════════ */

  private rewards(): void {
    const e = this.engine;
    const last = (this as unknown as { _lastRewardAt?: number })._lastRewardAt ?? e.startedAt;
    if (e.now - last < 3600 * 1000) return;
    (this as unknown as { _lastRewardAt?: number })._lastRewardAt = e.now;
    const scores = [...this.rewardScores.entries()].filter(([, s]) => s > 0);
    const total = scores.reduce((a, [, s]) => a + s, 0);
    if (total <= 0) return;
    for (const [sub, s] of scores) {
      const amount = (e.config.reward_budget_per_hour_quote_minor * BigInt(Math.round(s))) / BigInt(total);
      if (amount <= 0n) continue;
      const acc = e.accounts.get(sub);
      if (!acc) continue;
      acc.cash_quote_minor += amount;
      e.journalEvent({ type: "reward", payload: { subaccount: sub, amount_quote_minor: amount } });
      this.rewardScores.set(sub, 0);
      e.out.accountDirty.add(sub);
    }
    e.journalEvent({ type: "rewards_settled" });
  }

  private mmSampling(): void {
    const e = this.engine;
    for (const [sub, st] of e.mmStates) {
      if (!st.enrolled || e.now < st.next_sample_at) continue;
      st.next_sample_at = e.now + 30_000 + Math.floor(e.rng.next() * 300_000);
      st.total_ticks += 1;
      // Observe the maker's perp quoting at a random instant.
      const book = e.books.get(PERP);
      if (!book) continue;
      const bids = book.restingOrders().filter((o) => o.subaccount === sub && o.side === "bid");
      const asks = book.restingOrders().filter((o) => o.subaccount === sub && o.side === "ask");
      const twoSided = bids.length > 0 && asks.length > 0;
      let spreadBps = 999;
      let size = 0;
      if (twoSided) {
        const bestBid = Math.max(...bids.map((o) => o.price_ticks ?? 0));
        const bestAsk = Math.min(...asks.map((o) => o.price_ticks ?? Number.MAX_SAFE_INTEGER));
        const mid = e.spot_quote_minor;
        spreadBps = Number((((BigInt(bestAsk) - BigInt(bestBid)) * e.instruments.get(PERP)!.tick_size_quote_minor) * 10_000n) / mid);
        size = Math.min(bids.reduce((s, o) => s + o.qty_lots - o.filled_lots, 0), asks.reduce((s, o) => s + o.qty_lots - o.filled_lots, 0));
      }
      const ok = twoSided && spreadBps <= 250 && size >= 1;
      st.samples.push({ ok, spread_bps: spreadBps, size });
      if (st.samples.length > 200) st.samples.shift();
      e.journalEvent({
        type: "liquidity_scored",
        observations: [{ subaccount: sub, size_lots: size, spread_bps: spreadBps, two_sided: twoSided, side: "bid" }],
      });
      // Reward scoring: two-sided tight quoting earns the pool.
      const score = ok ? 1 + (spreadBps <= 50 ? 1 : 0) : 0;
      this.rewardScores.set(sub, (this.rewardScores.get(sub) ?? 0) + score);
    }
  }

  private mmReviews(): void {
    const e = this.engine;
    for (const [sub, st] of e.mmStates) {
      if (!st.enrolled || e.now < st.next_review_at) continue;
      st.next_review_at = e.now + 24 * 3600 * 1000;
      const n = st.samples.length;
      if (n === 0) continue;
      const ok = st.samples.filter((s) => s.ok).length;
      const uptime = Math.floor((ok * 1000) / n);
      st.uptime_permille = uptime;
      // Tightest tier earned by the window.
      const tiers = [
        { tier: "MM-1", up: 980, spread: 50, size: 5, discount: 2000 },
        { tier: "MM-2", up: 950, spread: 100, size: 3, discount: 1200 },
        { tier: "MM-3", up: 900, spread: 250, size: 1, discount: 600 },
      ];
      let assigned: string | null = null;
      let discount = 0;
      for (const t of tiers) {
        const meetsSpread = st.samples.every((s) => s.spread_bps <= t.spread);
        const meetsSize = st.samples.every((s) => s.size >= t.size);
        if (uptime >= t.up && meetsSpread && meetsSize) {
          assigned = t.tier;
          discount = t.discount;
          break;
        }
      }
      st.tier = assigned;
      st.discount_bps = discount;
      st.samples = [];
      e.journalEvent({
        type: "mm_tier_adjusted",
        subaccount: sub,
        tier: assigned,
        fee_discount_bps: discount,
        uptime_permille: uptime,
        ticks: n,
        ts: e.now,
      });
    }
  }

  /* ═══════════════════ withdrawals / blocks ═══════════════════ */

  private withdrawalSweep(): void {
    const e = this.engine;
    e.withdrawals.settleDue(e.now, (sub, amount) => {
      const acc = e.accounts.get(sub);
      if (!acc || acc.cash_quote_minor < amount) return false;
      acc.cash_quote_minor -= amount;
      e.out.accountDirty.add(sub);
      return true;
    });
  }

  private blocksPrint(): void {
    const e = this.engine;
    for (const b of e.blocks) {
      if (!b.printed && e.now >= b.broadcast_ts) {
        b.printed = true;
        e.journalEvent({ type: "block_printed", block_id: b.block_id });
      }
    }
  }

  /* ═══════════════════ liquidations ═══════════════════ */

  private liquidationSweep(): void {
    const e = this.engine;
    for (const acc of e.accounts.values()) {
      if (acc.positions.size === 0) continue;
      const m = e.marginOf(acc.id);
      if (m.equity >= m.maintenance) continue;
      this.liquidateAccount(acc.id, m.maintenance - m.equity);
    }
  }

  private liquidateAccount(sub: SubaccountId, deficit: bigint): void {
    const e = this.engine;
    const acc = e.accounts.get(sub);
    if (!acc) return;
    // Rank legs by maintenance contribution (notional) — largest first.
    const legs = [...acc.positions.values()].sort((a, b) => Math.abs(b.signed_lots) - Math.abs(a.signed_lots));
    let closedValue = 0n;
    for (const pos of legs) {
      if (closedValue >= deficit * 3n) break; // healthy buffer restored
      const inst = e.instruments.get(pos.symbol);
      if (!inst) continue;
      const mark = e.markOf(pos.symbol);
      const closeLots = Math.max(1, Math.ceil(Math.abs(pos.signed_lots) / 2));
      this.liquidateLeg(sub, pos.symbol, pos.signed_lots > 0 ? "ask" : "bid", closeLots, mark, inst.lot_size_base_minor);
      closedValue += (mark * BigInt(closeLots) * inst.lot_size_base_minor) / 10n ** BigInt(inst.base_decimals);
    }
    // After closing: if equity is still negative, the insurance fund
    // backstops it; an exhausted fund triggers ADL against the most
    // profitable counterparties.
    const m = e.marginOf(sub);
    if (m.equity < 0n) {
      const need = -m.equity;
      const cover = e.insurance_balance >= need ? need : e.insurance_balance;
      e.insurance_balance -= cover;
      acc.cash_quote_minor += cover;
      let residual = need - cover;
      if (residual > 0n) {
        // ADL: force-close the most profitable counterparties at the
        // bankruptcy price — the last resort every venue ships.
        const profitable = [...e.accounts.values()]
          .filter((a) => a.id !== sub)
          .map((a) => ({ acc: a, eq: e.marginOf(a.id).equity }))
          .filter((x) => x.eq > 0n)
          .sort((a, b) => (b.eq > a.eq ? 1 : -1));
        for (const cp of profitable) {
          if (residual <= 0n) break;
          const take = cp.eq / 2n > residual ? residual : cp.eq / 2n;
          cp.acc.cash_quote_minor -= take;
          acc.cash_quote_minor += take;
          residual -= take;
          e.stats.adls += 1;
          e.adl_count += 1;
          e.journalEvent({
            type: "adl",
            payload: {
              liquidated_subaccount: sub,
              counterparty_subaccount: cp.acc.id,
              symbol: "BTC-PERP",
              lots: 0,
              price_quote_minor: (e.spot_quote_minor * 19n) / 20n,
              closing_side_is_ask: true,
            },
          });
          e.out.accountDirty.add(cp.acc.id);
        }
      }
      e.out.accountDirty.add(sub);
    }
  }

  private liquidateLeg(sub: SubaccountId, symbol: Symbol, side: "bid" | "ask", lots: number, mark: bigint, lotBase: bigint): void {
    const e = this.engine;
    // 1) Cross whatever book liquidity exists at prices better than the penalized mark.
    const book = e.books.get(symbol);
    const remaining = { lots };
    if (book && book.bestBid() != null) {
      // Reduce-only IOC market order through the standard command path.
      e.process({
        type: "place",
        request: {
          subaccount: sub,
          symbol,
          side,
          order_type: { kind: "market" },
          price_ticks: null,
          qty_lots: lots,
          tif: { kind: "ioc" },
          post_only: false,
          reduce_only: true,
          stp: "cancel_newest",
          display_lots: null,
          oco_group: null,
          client_ts: e.now,
        },
        now: e.now,
      });
      const after = e.accounts.get(sub)?.positions.get(symbol);
      remaining.lots = after ? Math.abs(after.signed_lots) : 0;
    }
    if (remaining.lots <= 0) return;

    // 2) Insurance fund: buyer of last resort at the penalized price.
    const coverage = e.coveragePermille();
    const boost = coverage < 500 ? 1.5 : 1;
    const penaltyBps = Math.round(125 * boost);
    const penaltyPrice = side === "ask" ? (mark * BigInt(10_000 - penaltyBps)) / 10_000n : (mark * BigInt(10_000 + penaltyBps)) / 10_000n;
    // 2) Insurance fund: buyer of last resort at the penalized price.
    //    The position closes at penaltyPrice (PnL banking); the fund takes
    //    the inventory; the penalty spread is the backstop's compensation.
    const acc = e.accounts.get(sub)!;
    const pos = acc.positions.get(symbol);
    const penaltySpread = ((mark - penaltyPrice) * BigInt(remaining.lots) * lotBase * (side === "ask" ? 1n : -1n)) / baseScaleOf(e, symbol);
    this.applySettlement(acc, symbol, pos?.signed_lots ?? 0, penaltyPrice, lotBase, baseDecimalsOf(e, symbol));
    e.insurance_inventory.set(symbol, (e.insurance_inventory.get(symbol) ?? 0) + (side === "ask" ? -remaining.lots : remaining.lots));
    e.insurance_absorbed_total += penaltySpread > 0n ? penaltySpread : 0n;
    e.stats.liquidations += 1;
    e.liquidation_count += 1;
    e.journalEvent({
      type: "liquidation",
      payload: {
        subaccount: sub,
        symbol,
        lots: remaining.lots,
        price_quote_minor: penaltyPrice,
        to_insurance: true,
        penalty_quote_minor: penaltySpread,
        absorbed_quote_minor: 0n,
        closing_side_is_ask: side === "ask",
      },
    });
    e.out.accountDirty.add(sub);
  }

  /* ═══════════════════ breakers / listing ═══════════════════ */

  private breakerChecks(): void {
    const e = this.engine;
    for (const [sym, book] of e.books) {
      const inst = e.instruments.get(sym);
      if (!inst) continue;
      // Release expired breakers FIRST — a halted market's book is empty
      // (makers are rejected while halted), so the release check must not
      // sit behind the two-sided-book guard below or halts become permanent.
      const existing = e.breakers.get(sym);
      if (existing && e.now >= existing.until) {
        e.breakers.delete(sym);
        e.journalEvent({ type: "breaker_released", kind: existing.kind, symbol: sym, ts: e.now });
      }
      const bb = book.bestBid();
      const ba = book.bestAsk();
      if (bb == null || ba == null) {
        this.breakerWatch.delete(sym);
        continue;
      }
      const mid = (BigInt(bb) + BigInt(ba)) * inst.tick_size_quote_minor / 2n;
      // Dislocation is measured against the instrument's OWN fair value:
      // perp mid vs spot; option mid vs its model mark. (Comparing an
      // option premium against spot would trip every chain instantly.)
      const ref = inst.kind === "option" ? e.markOf(sym) : e.spot_quote_minor;
      if (ref <= 0n) {
        this.breakerWatch.delete(sym);
        continue;
      }
      const dislocation = Number(((mid - ref) * 10_000n) / ref);
      if (Math.abs(dislocation) > 500) {
        const since = this.breakerWatch.get(sym) ?? e.now;
        this.breakerWatch.set(sym, since);
        if (e.now - since > 10_000 && !e.breakers.has(sym)) {
          e.breakers.set(sym, { kind: "price-dislocation", tripped_at: e.now, until: e.now + 60_000 });
          e.journalEvent({ type: "breaker_tripped", kind: "price-dislocation", symbol: sym, ts: e.now });
        }
      } else {
        this.breakerWatch.delete(sym);
      }
    }
  }

  private autoList(): void {
    const e = this.engine;
    const last = (this as unknown as { _lastListAt?: number })._lastListAt ?? e.now;
    if (e.now - last < 10 * 60 * 1000) return;
    (this as unknown as { _lastListAt?: number })._lastListAt = e.now;
    // List a new strike column when spot leaves the current range.
    const spot = Number(e.spot_quote_minor);
    let minStrike = Infinity;
    let maxStrike = -Infinity;
    for (const inst of e.instruments.values()) {
      if (inst.kind !== "option") continue;
      minStrike = Math.min(minStrike, Number(inst.strike_quote_minor));
      maxStrike = Math.max(maxStrike, Number(inst.strike_quote_minor));
    }
    const step = 4_000 * 100;
    if (spot < minStrike - step * 0.5) {
      for (const ms of [7, 30, 60]) {
        e.listOption(BigInt(Math.round((minStrike - step) / 100) * 100), e.now + ms * 24 * 3600 * 1000, "call", "dated", ms === 30 ? "american" : "european", "dated");
        e.listOption(BigInt(Math.round((minStrike - step) / 100) * 100), e.now + ms * 24 * 3600 * 1000, "put", "dated", ms === 30 ? "american" : "european", "dated");
      }
    }
    if (spot > maxStrike + step * 0.5) {
      for (const ms of [7, 30, 60]) {
        e.listOption(BigInt(Math.round((maxStrike + step) / 100) * 100), e.now + ms * 24 * 3600 * 1000, "call", "dated", ms === 30 ? "american" : "european", "dated");
        e.listOption(BigInt(Math.round((maxStrike + step) / 100) * 100), e.now + ms * 24 * 3600 * 1000, "put", "dated", ms === 30 ? "american" : "european", "dated");
      }
    }
  }

  /* ═══════════════════ RFQ beats ═══════════════════ */

  private rfqBeat(): void {
    const e = this.engine;
    const near = this.nearSymbols(4).filter((s) => {
      const inst = e.instruments.get(s);
      return inst?.kind === "option";
    });
    if (!near.length) return;
    const sym = e.rng.pick(near);
    const legs: { symbol: Symbol; side: "bid" | "ask"; qty_lots: number }[] = [
      { symbol: sym, side: "bid", qty_lots: e.rng.int(10, 40) },
    ];
    if (e.rng.chance(0.4)) {
      legs.push({ symbol: PERP, side: "ask", qty_lots: e.rng.int(20, 80) });
    }
    const rfqId = e.nextRfqIdPeek();
    e.process({
      type: "rfq_create",
      taker: 4,
      legs,
      counterparties: [1, 2],
      min_total_cost_quote_minor: null,
      max_total_cost_quote_minor: null,
      ttl_ms: 30_000,
      now: e.now,
    });
    this.rfqQueue.push({ id: rfqId, respondAtWall: Date.now() + 800 });
    this.rfqAutoExecuteAt.set(rfqId, Date.now() + 4_000);
  }

  private rfqAutoQuote(): void {
    const e = this.engine;
    const wall = Date.now();
    const keep: { id: number; respondAtWall: number }[] = [];
    for (const item of this.rfqQueue) {
      if (wall < item.respondAtWall) {
        keep.push(item);
        continue;
      }
      const rfq = e.rfqs.get(item.id);
      if (!rfq || rfq.status !== "open") continue;
      for (const maker of [1, 2]) {
        const legPrices: number[] = [];
        let valid = true;
        for (const leg of rfq.legs) {
          const inst = e.instruments.get(leg.symbol);
          if (!inst) {
            valid = false;
            break;
          }
          const markTicks = Number(e.markOf(leg.symbol) / inst.tick_size_quote_minor);
          const edge = e.rng.int(2, 6);
          legPrices.push(leg.side === "bid" ? markTicks + edge : Math.max(1, markTicks - edge));
        }
        if (!valid) continue;
        e.process({ type: "rfq_quote", maker, rfq_id: item.id, leg_prices_ticks: legPrices, ttl_ms: 20_000, now: e.now });
      }
    }
    this.rfqQueue = keep;
  }

  private rfqAutoExecute(): void {
    const e = this.engine;
    for (const [id, atWall] of [...this.rfqAutoExecuteAt]) {
      if (Date.now() < atWall) continue;
      this.rfqAutoExecuteAt.delete(id);
      const rfq = e.rfqs.get(id);
      if (!rfq || rfq.status !== "quoted" || rfq.quotes.length === 0) continue;
      const best = rfq.quotes.reduce((a, b) => (b.total_cost_quote_minor < a.total_cost_quote_minor ? b : a));
      e.process({ type: "rfq_execute", taker: rfq.taker, rfq_id: id, quote_id: best.quote_id, now: e.now });
    }
  }

  /* ═══════════════════ misc ═══════════════════ */

  private pruneVolumes(): void {
    const e = this.engine;
    const cutoff = e.now - 24 * 3600 * 1000;
    for (const [sym, ring] of e.volume24) {
      const pruned = ring.filter((x) => x.ts >= cutoff);
      if (pruned.length !== ring.length) e.volume24.set(sym, pruned);
    }
  }
}
