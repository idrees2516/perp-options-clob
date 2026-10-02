/**
 * SimEngine — the deterministic venue core.
 *
 * Mirrors the discipline of `poc-engine`: commands in → events out →
 * state applied. One `process()` entrypoint; every state change journals an
 * Event. The autonomous market ticker (venue-ticker.ts) drives the same
 * command surface a real gateway would.
 */

import type {
  AccountDescriptor,
  Command,
  EngineStats,
  Event,
  FillRow,
  Instrument,
  InstrumentKind,
  JournalEntry,
  MarketRow,
  MoneyMinor,
  OpenOrderRow,
  OptionMarket,
  OrderRequest,
  PerpMarket,
  PositionRow,
  QtyLots,
  Rejection,
  RfqView,
  SignedMoneyMinor,
  Side,
  SubaccountId,
  Symbol,
  Trade,
  TimestampMs,
  BookView,
} from "@perp/types";
import {
  DEFAULT_FUNDING_PARAMS,
  bpsOfUp,
  opposite,
} from "@perp/types";
import { Book, type Fill } from "./book";
import {
  accountEquity,
  applyPositionDelta,
  newAccount,
  orderMarginFor,
  sfpmMargin,
  type LegMark,
  type SimAccount,
} from "./accounts";
import { computeFillFee, feeTierFor, newRevenueLedger, routeFee, dayKey, trailingVolume } from "./fees";
import { OracleEngine } from "./oracle";
import { GovernanceEngine } from "./governance";
import { WithdrawalPipeline } from "./withdrawals";
import { Rng, hashSeed } from "./rng";
import { americanPrice, markOption, optionGreeks } from "./pricing";
import { effectiveTteYears } from "@perp/types";
import { bsPrice } from "./pricing";

export interface EngineConfig {
  seed: number;
  base_symbol: string;
  genesis_spot_quote_minor: MoneyMinor;
  providers: string[];
  reward_budget_per_hour_quote_minor: MoneyMinor;
  insurance_seed_quote_minor: MoneyMinor;
  coverage_target_permille: number;
  quote_decimals: number;
  base_decimals: number;
  perp_tick_size_quote_minor: MoneyMinor;
  perp_lot_size_base_minor: MoneyMinor;
  option_tick_size_quote_minor: MoneyMinor;
  option_lot_size_base_minor: MoneyMinor;
}

export const DEFAULT_ENGINE_CONFIG: EngineConfig = {
  seed: 20260930,
  base_symbol: "BTC",
  genesis_spot_quote_minor: 80_000_00n,
  providers: ["pyth", "chainlink", "apis3"],
  reward_budget_per_hour_quote_minor: 500_00n,
  insurance_seed_quote_minor: 100_000_00n,
  coverage_target_permille: 2000,
  quote_decimals: 2,
  base_decimals: 5,
  perp_tick_size_quote_minor: 100n, // $1.00
  perp_lot_size_base_minor: 100n, // 0.001 BTC
  option_tick_size_quote_minor: 50n, // $0.50
  option_lot_size_base_minor: 1_000n, // 0.01 BTC
};

export const ACCOUNT_DESCRIPTORS: AccountDescriptor[] = [
  { id: 1, name: "MM-1", role: "Market maker", initial_deposit_quote_minor: 5_000_000_00n, color: "#2dd4bf" },
  { id: 2, name: "MM-2", role: "Market maker", initial_deposit_quote_minor: 5_000_000_00n, color: "#34d399" },
  { id: 3, name: "Trader-3", role: "Directional taker", initial_deposit_quote_minor: 2_000_000_00n, color: "#f5c24b" },
  { id: 4, name: "Trader-4", role: "Directional taker", initial_deposit_quote_minor: 2_000_000_00n, color: "#fb923c" },
  { id: 5, name: "Trader-5", role: "Vol trader", initial_deposit_quote_minor: 2_000_000_00n, color: "#e879f9" },
  { id: 6, name: "Degen-6", role: "Leverage case study", initial_deposit_quote_minor: 45_000_00n, color: "#f87171" },
  { id: 7, name: "You", role: "Operator console", initial_deposit_quote_minor: 250_000_00n, color: "#38bdf8" },
];

interface PendingStop {
  order: OrderRequest;
  id: number;
  armed: boolean;
  /** Trailing-stop extreme (mark high/low since placement). */
  extreme: bigint | null;
}

interface PendingExercise {
  request_id: number;
  subaccount: SubaccountId;
  symbol: Symbol;
  lots: QtyLots;
  requested_at: TimestampMs;
  settle_at: TimestampMs;
  twap_sum: bigint;
  twap_n: number;
}

interface SimVault {
  vault_id: number;
  revenue_share_bps: number;
  total_shares: bigint;
  nav_quote_minor: bigint;
  epoch: number;
  last_epoch_ts: number;
  pending_subs: { sub: SubaccountId; amount: bigint }[];
  pending_redeems: { sub: SubaccountId; shares: bigint }[];
  lifetime_credit: bigint;
}

export interface EngineOutputs {
  journal: JournalEntry[];
  prints: Trade[];
  bookDirty: Set<Symbol>;
  accountDirty: Set<SubaccountId>;
  marketDirty: boolean;
}

const MAX_OPEN_ORDERS = 200;
const MAX_POSITION_LOTS = 10_000;
const PRICE_BAND_BPS_PERP = 1_000;
const PRICE_BAND_BPS_OPTION = 2_000;
const JOURNAL_CAP = 4000;
const TAKER_ARRIVAL_PER_SEC = 1.1;
const EXERCISE_TWAP_MS = 30 * 60 * 1000;

export class SimEngine {
  readonly config: EngineConfig;
  rng: Rng;
  now: number;
  startedAt: number;
  speed = 600;
  running = true;
  ticks = 0;
  phase = "genesis";

  instruments = new Map<Symbol, Instrument>();
  books = new Map<Symbol, Book>();
  orders = new Map<number, OrderRequest & { id: number; filled_lots: number; state: string; engine_ts: number }>();
  accounts = new Map<SubaccountId, SimAccount>();
  oracle: OracleEngine;
  governance = new GovernanceEngine();
  withdrawals = new WithdrawalPipeline();

  spot_quote_minor: bigint;
  /** Governed IV surface: symbol → iv_bps. */
  ivs = new Map<Symbol, number>();

  journal: JournalEntry[] = [];
  stats: EngineStats;
  revenue = newRevenueLedger();
  insurance_balance: bigint;
  insurance_inventory = new Map<Symbol, number>(); // signed lots
  insurance_absorbed_total: bigint = 0n;
  liquidation_count = 0;
  adl_count = 0;

  vaults = new Map<number, SimVault>();
  mmStates = new Map<SubaccountId, { enrolled: boolean; tier: string | null; discount_bps: number; uptime_permille: number; samples: { ok: boolean; spread_bps: number; size: number }[]; total_ticks: number; next_sample_at: number; next_review_at: number }>();
  mmpWindows = new Map<SubaccountId, { window_start: number; fill_lots: number; net_delta: number; frozen_until: number; config: { interval_ms: number; frozen_ms: number; amount_limit: number; delta_limit: number; base: string } | null }>();
  twapParents = new Map<number, { parent_id: number; sub: SubaccountId; symbol: Symbol; side: Side; total_lots: number; slices: number; placed: number; lots_placed: number; slice_interval_ms: number; limit_ticks: number | null; next_slice_ts: number; state: "running" | "completed" | "canceled" }>();
  pendingStops = new Map<number, PendingStop>();
  pendingExercises = new Map<number, PendingExercise>();
  rfqs = new Map<number, RfqView & { quotes_ttl: Map<number, number> }>();
  blocks: { block_id: number; taker: SubaccountId; maker: SubaccountId; legs: { symbol: Symbol; side: Side; qty_lots: number; price_ticks: number }[]; total_notional: bigint; registered_ts: number; broadcast_ts: number; printed: boolean }[] = [];
  breakers = new Map<Symbol, { kind: "price-dislocation" | "cascade-velocity"; tripped_at: number; until: number }>();

  fundingTrackers = new Map<Symbol, { sum_mid: bigint; sum_index: bigint; n: number; last_boundary: number; last_rate_bps: number; premium_bps: number }>();
  fundingHistory: import("@perp/types").FundingRecord[] = [];
  everlastingRoll = new Map<Symbol, { sum_premium: bigint; n: number; last_roll: number; last_rate_bps: number }>();
  volume24 = new Map<Symbol, { ts: number; notional: bigint }[]>();
  sessionOpen = new Map<Symbol, bigint>();
  ocoGroups = new Map<number, [number, number]>();

  private journalSeq = 0;
  private tradeSeq = 0;
  private nextOrderId = 1;
  private nextRfqId = 1;
  private nextBlockId = 1;
  private nextRequestId = 1;
  private nextGroupId = 1;
  private nextVaultId = 1;
  private porNonce = 0;
  /** Last built proof-of-reserves report + rows (rebuilt on demand). */
  porCached: { report: import("@perp/types").PorReport | null; rows: import("@perp/types").PorLiabilityRow[] } = { report: null, rows: [] };

  porNonceBump(): void {
    this.porNonce += 1;
  }

  porNonceGet(): number {
    return this.porNonce;
  }

  out: EngineOutputs;

  constructor(config: Partial<EngineConfig> = {}) {
    this.config = { ...DEFAULT_ENGINE_CONFIG, ...config };
    this.rng = new Rng(this.config.seed);
    this.startedAt = Date.now();
    this.now = this.startedAt;
    this.spot_quote_minor = this.config.genesis_spot_quote_minor;
    this.oracle = new OracleEngine(this.config.base_symbol, this.config.providers);
    this.insurance_balance = this.config.insurance_seed_quote_minor;
    this.stats = {
      events: 0,
      trades: 0,
      lots_traded: 0,
      notional_traded_quote_minor: 0n,
      revenue: this.revenue,
      insurance_balance: this.insurance_balance,
      funding_intervals: 0,
      options_settled: 0,
      exercises_settled: 0,
      liquidations: 0,
      adls: 0,
    };
    this.out = { journal: [], prints: [], bookDirty: new Set(), accountDirty: new Set(), marketDirty: false };
    this.genesis();
  }

  /* ═════════════════════════ genesis ═════════════════════════ */

  private genesis(): void {
    const c = this.config;

    // Accounts + deposits.
    for (const d of ACCOUNT_DESCRIPTORS) {
      const acc = newAccount(d.id);
      acc.cash_quote_minor = d.initial_deposit_quote_minor;
      this.accounts.set(d.id, acc);
      this.journalEvent({ type: "deposit", subaccount: d.id, amount_quote_minor: d.initial_deposit_quote_minor, ts: this.now });
    }
    // Insurance seed counts as its own reserve (not a user deposit).
    this.insurance_balance = c.insurance_seed_quote_minor;

    // Oracle wiring.
    this.oracle.onQuarantine = (provider, price, median) => {
      this.journalEvent({
        type: "provider_observed",
        base_symbol: c.base_symbol,
        provider,
        ts: this.now,
        price_quote_minor: price,
      });
    };
    for (const p of c.providers) {
      this.oracle.observe(p, this.spot_quote_minor, this.now);
    }
    this.spot_quote_minor = this.oracle.mark(this.now) ?? this.spot_quote_minor;

    // Instruments: the perp + a full option chain.
    this.listPerp(`${c.base_symbol}-PERP`);
    this.listChain();

    // LP vault (G-16): 40% of the insurance share routes to underwriters.
    this.createVault(4000, this.now);
  }

  private listPerp(symbol: Symbol): void {
    const c = this.config;
    const perp: PerpMarket = {
      kind: "perp",
      symbol,
      base_symbol: c.base_symbol,
      quote_decimals: c.quote_decimals,
      base_decimals: c.base_decimals,
      tick_size_quote_minor: c.perp_tick_size_quote_minor,
      lot_size_base_minor: c.perp_lot_size_base_minor,
      initial_margin_ratio_bps: 500,
      maintenance_margin_ratio_bps: 375,
      price_band_bps: PRICE_BAND_BPS_PERP,
      max_order_lots: 100_000,
      funding: { ...DEFAULT_FUNDING_PARAMS },
    };
    this.instruments.set(symbol, perp);
    this.books.set(symbol, new Book(symbol));
    this.fundingTrackers.set(symbol, { sum_mid: 0n, sum_index: 0n, n: 0, last_boundary: this.now, last_rate_bps: 0, premium_bps: 0 });
    this.sessionOpen.set(symbol, this.spot_quote_minor);
    this.journalEvent({ type: "market_listed", instrument: perp, anchor_iv_bps: null });
  }

  /** Dated chain (7/30/60d × strikes around spot) + everlasting strikes + American samples. */
  private listChain(): void {
    const spot = Number(this.spot_quote_minor) / 100;
    const step = 4_000;
    const base = Math.round(spot / step) * step;
    const strikes = [-4, -3, -2, -1, 0, 1, 2, 3, 4].map((k) => (base + k * step) * 100);
    const now = this.now;
    const tenors = [
      { label: "7D", ms: 7 * 24 * 3600 * 1000, style: "european" as const },
      { label: "30D", ms: 30 * 24 * 3600 * 1000, style: "american" as const },
      { label: "60D", ms: 60 * 24 * 3600 * 1000, style: "european" as const },
    ];
    for (const t of tenors) {
      for (const strike of strikes) {
        this.listOption(BigInt(strike), now + t.ms, "call", "dated", t.style, t.label);
        this.listOption(BigInt(strike), now + t.ms, "put", "dated", t.style, t.label);
      }
    }
    // Everlasting chain around ATM (hourly roll, ~1d effective maturity).
    for (const strike of [base - step, base, base + step]) {
      this.listOption(BigInt(strike * 100), now + 365 * 24 * 3600 * 1000, "call", "everlasting", "european", "EVER");
      this.listOption(BigInt(strike * 100), now + 365 * 24 * 3600 * 1000, "put", "everlasting", "european", "EVER");
    }
  }

  listOption(
    strikeQuoteMinor: bigint,
    expiryTs: number,
    kind: "call" | "put",
    variant: "dated" | "everlasting",
    style: "european" | "american",
    label: string,
  ): Symbol {
    const c = this.config;
    const y = new Date(expiryTs);
    const stamp = `${y.getUTCFullYear()}${String(y.getUTCMonth() + 1).padStart(2, "0")}${String(y.getUTCDate()).padStart(2, "0")}`;
    const strike = Number(strikeQuoteMinor) / 100;
    const symbol = `${c.base_symbol}-${variant === "everlasting" ? "EVER" : stamp}-${strike}-${kind === "call" ? "C" : "P"}`;
    const market: OptionMarket = {
      kind: "option",
      symbol,
      base_symbol: c.base_symbol,
      kind_of: kind,
      strike_quote_minor: strikeQuoteMinor,
      expiry_ts_ms: expiryTs,
      variant,
      exercise_style: style,
      american: { settlement_twap_ms: EXERCISE_TWAP_MS, exercise_fee_bps: 5 },
      everlasting: { interval_ms: 3600 * 1000, maturity_multiple: 24 },
      quote_decimals: c.quote_decimals,
      base_decimals: c.base_decimals,
      tick_size_quote_minor: c.option_tick_size_quote_minor,
      lot_size_base_minor: c.option_lot_size_base_minor,
      price_band_bps: PRICE_BAND_BPS_OPTION,
      max_order_lots: 10_000,
      margin: { short_option_min_bps: 500, liquidation_fee_bps: 125 },
    };
    this.instruments.set(symbol, market);
    this.books.set(symbol, new Book(symbol));
    const anchor = this.anchorIv(market);
    this.ivs.set(symbol, anchor);
    this.sessionOpen.set(symbol, this.markOf(symbol));
    if (variant === "everlasting") {
      this.everlastingRoll.set(symbol, { sum_premium: 0n, n: 0, last_roll: this.now, last_rate_bps: 0 });
    }
    this.journalEvent({ type: "market_listed", instrument: market, anchor_iv_bps: anchor });
    return symbol;
  }

  /** Governed surface: anchor 55% with a deterministic moneyness smile. */
  anchorIv(o: OptionMarket): number {
    const spot = Number(this.spot_quote_minor);
    const strike = Number(o.strike_quote_minor);
    const m = Math.log(strike / spot);
    const smile = 1 + 0.35 * m * m - 0.18 * m; // skew: puts richer
    return Math.round(5500 * Math.max(0.4, smile));
  }

  /* ═════════════════════════ marks ═════════════════════════ */

  markOf(symbol: Symbol): bigint {
    const inst = this.instruments.get(symbol);
    if (!inst) return 0n;
    if (inst.kind === "perp") return this.spot_quote_minor;
    const iv = this.ivs.get(symbol) ?? 5500;
    const { premium_quote_minor_per_base } = markOption(inst, this.spot_quote_minor, iv, this.now);
    return premium_quote_minor_per_base;
  }

  /** Mid of the book if liquid, else the theoretical mark. */
  bookMidOrMark(symbol: Symbol): bigint {
    const book = this.books.get(symbol);
    const inst = this.instruments.get(symbol);
    if (!book || !inst) return this.markOf(symbol);
    const bb = book.bestBid();
    const ba = book.bestAsk();
    if (bb != null && ba != null) {
      return (BigInt(bb) + BigInt(ba)) * inst.tick_size_quote_minor / 2n;
    }
    return this.markOf(symbol);
  }

  /* ═════════════════════════ journal ═════════════════════════ */

  journalEvent(event: Event): void {
    const entry: JournalEntry = { seq: ++this.journalSeq, ts: this.now, event };
    this.journal.push(entry);
    if (this.journal.length > JOURNAL_CAP) this.journal.splice(0, this.journal.length - JOURNAL_CAP);
    this.out.journal.push(entry);
    if (this.out.journal.length > 400) this.out.journal.splice(0, this.out.journal.length - 400);
    this.stats.events += 1;
    this.out.marketDirty = true;
  }

  /* ═════════════════════════ command surface ═════════════════════════ */

  process(cmd: Command): void {
    switch (cmd.type) {
      case "deposit": {
        const acc = this.account(cmd.subaccount);
        if (!acc) return;
        acc.cash_quote_minor += cmd.amount_quote_minor;
        this.journalEvent({ type: "deposit", subaccount: cmd.subaccount, amount_quote_minor: cmd.amount_quote_minor, ts: this.now });
        this.out.accountDirty.add(cmd.subaccount);
        return;
      }
      case "withdraw": {
        const acc = this.account(cmd.subaccount);
        if (!acc) return;
        const spendable = this.spendableCash(cmd.subaccount);
        const res = this.withdrawals.request(cmd.subaccount, cmd.amount_quote_minor, "internal", spendable, this.now);
        if (typeof res === "string") {
          this.journalEvent({ type: "withdraw_rejected", subaccount: cmd.subaccount, requested: cmd.amount_quote_minor, reason: res, ts: this.now });
        }
        // No immediate event on success — pipeline state shows it.
        return;
      }
      case "place":
        this.placeOrder(cmd.request, cmd.now);
        return;
      case "cancel":
        this.cancelOrder(cmd.order_id, "canceled");
        return;
      case "cancel_all":
        this.cancelAll(cmd.subaccount, cmd.symbol ?? null);
        return;
      case "oracle_update":
        this.oracle.observe(cmd.provider, cmd.price_quote_minor, cmd.ts);
        this.spot_quote_minor = this.oracle.mark(this.now) ?? this.spot_quote_minor;
        this.journalEvent({ type: "provider_observed", base_symbol: cmd.base_symbol, provider: cmd.provider, ts: cmd.ts, price_quote_minor: cmd.price_quote_minor });
        return;
      case "tick":
        // Manual ticks are honored (the ticker also calls tick() on cadence).
        return;
      case "transfer": {
        const from = this.account(cmd.from);
        const to = this.account(cmd.to);
        if (!from || !to) return;
        if (from.cash_quote_minor < cmd.amount_quote_minor) {
          this.journalEvent({ type: "transfer_rejected", from: cmd.from, to: cmd.to, requested: cmd.amount_quote_minor, reason: "insufficient_balance", ts: this.now });
          return;
        }
        from.cash_quote_minor -= cmd.amount_quote_minor;
        to.cash_quote_minor += cmd.amount_quote_minor;
        this.journalEvent({ type: "transfer_executed", from: cmd.from, to: cmd.to, amount_quote_minor: cmd.amount_quote_minor, ts: this.now });
        this.out.accountDirty.add(cmd.from);
        this.out.accountDirty.add(cmd.to);
        return;
      }
      case "exercise":
        this.requestExercise(cmd.subaccount, cmd.symbol, cmd.lots);
        return;
      case "set_mmp":
        this.mmpWindows.set(cmd.subaccount, {
          window_start: this.now,
          fill_lots: 0,
          net_delta: 0,
          frozen_until: 0,
          config: {
            interval_ms: cmd.interval_ms,
            frozen_ms: cmd.frozen_time_ms,
            amount_limit: cmd.amount_limit_lots,
            delta_limit: cmd.delta_limit_lots,
            base: cmd.base_symbol,
          },
        });
        this.journalEvent({
          type: "mmp_configured",
          subaccount: cmd.subaccount,
          base_symbol: cmd.base_symbol,
          interval_ms: cmd.interval_ms,
          frozen_time_ms: cmd.frozen_time_ms,
          amount_limit_lots: cmd.amount_limit_lots,
          delta_limit_lots: cmd.delta_limit_lots,
        });
        return;
      case "set_cod":
        this.journalEvent({ type: "cod_changed", subaccount: cmd.subaccount, enabled: cmd.enabled, ts: this.now });
        return;
      case "session_dropped":
        this.journalEvent({ type: "session_disconnected", subaccount: cmd.subaccount, canceled_orders: [], ts: this.now });
        return;
      case "place_batch":
        for (const r of cmd.requests) this.placeOrder(r, cmd.now);
        return;
      case "cancel_batch":
        for (const id of cmd.order_ids) this.cancelOrder(id, "canceled");
        return;
      case "amend":
        this.amendOrder(cmd.subaccount, cmd.order_id, cmd.new_price_ticks, cmd.new_open_lots);
        return;
      case "begin_auction":
        this.journalEvent({ type: "auction_opened", symbol: cmd.symbol, uncross_at: cmd.uncross_at, ts: this.now });
        return;
      case "deposit_collateral": {
        const acc = this.account(cmd.subaccount);
        if (!acc) return;
        acc.collateral.set(cmd.currency, (acc.collateral.get(cmd.currency) ?? 0n) + cmd.amount_minor);
        this.journalEvent({ type: "collateral_moved", payload: { subaccount: cmd.subaccount, currency: cmd.currency, amount_minor: cmd.amount_minor, ts: this.now } });
        this.out.accountDirty.add(cmd.subaccount);
        return;
      }
      case "withdraw_collateral": {
        const acc = this.account(cmd.subaccount);
        if (!acc) return;
        const bal = acc.collateral.get(cmd.currency) ?? 0n;
        if (bal < cmd.amount_minor) {
          this.journalEvent({ type: "collateral_rejected", subaccount: cmd.subaccount, currency: cmd.currency, requested_minor: cmd.amount_minor, reason: "insufficient", ts: this.now });
          return;
        }
        acc.collateral.set(cmd.currency, bal - cmd.amount_minor);
        this.journalEvent({ type: "collateral_moved", payload: { subaccount: cmd.subaccount, currency: cmd.currency, amount_minor: -cmd.amount_minor, ts: this.now } });
        this.out.accountDirty.add(cmd.subaccount);
        return;
      }
      case "convert_collateral":
        this.convertCollateral(cmd.subaccount, cmd.from, cmd.to, cmd.from_amount_minor);
        return;
      case "place_oco":
        this.placeOco(cmd.first, cmd.second, cmd.now);
        return;
      case "place_twap":
        this.placeTwap(cmd);
        return;
      case "cancel_twap":
        this.cancelTwap(cmd.parent_id);
        return;
      case "vault_create":
        this.createVault(cmd.revenue_share_bps, this.now);
        return;
      case "vault_subscribe":
        this.vaultSubscribe(cmd.vault_id, cmd.subaccount, cmd.amount_quote_minor);
        return;
      case "vault_redeem":
        this.vaultRedeem(cmd.vault_id, cmd.subaccount, cmd.shares);
        return;
      case "mm_tier_enroll":
        this.mmEnroll(cmd.subaccount);
        return;
      case "rfq_create":
        this.rfqCreate(cmd);
        return;
      case "rfq_quote":
        this.rfqQuote(cmd);
        return;
      case "rfq_execute":
        this.rfqExecute(cmd.taker, cmd.rfq_id, cmd.quote_id);
        return;
      case "rfq_cancel":
        this.rfqCancel(cmd.subaccount, cmd.rfq_id, cmd.quote_id);
        return;
      case "block_trade":
        this.registerBlock(cmd);
        return;
    }
  }

  account(id: SubaccountId): SimAccount | undefined {
    return this.accounts.get(id);
  }

  spendableCash(id: SubaccountId): bigint {
    const acc = this.account(id);
    if (!acc) return 0n;
    return acc.cash_quote_minor;
  }

  /* ═════════════════════════ orders ═════════════════════════ */

  placeOrder(req: OrderRequest, clientTs: number): void {
    const inst = this.instruments.get(req.symbol);
    const acc = this.accounts.get(req.subaccount);
    if (!inst) {
      this.journalEvent({ type: "order_rejection", payload: { request: req, reason: { kind: "unknown_instrument" }, order_id: this.peekOrderId() } });
      return;
    }
    if (!acc) {
      this.journalEvent({ type: "order_rejection", payload: { request: req, reason: { kind: "unknown_account" }, order_id: this.peekOrderId() } });
      return;
    }
    if (this.oracle.halted) {
      this.reject(req, { kind: "market_halted" });
      return;
    }
    if (this.breakers.has(req.symbol)) {
      this.reject(req, { kind: "market_halted" });
      return;
    }
    if (req.qty_lots <= 0 || req.qty_lots > inst.max_order_lots) {
      this.reject(req, { kind: "invalid_order", reason: "qty out of range" });
      return;
    }
    if (acc.open_orders.size >= MAX_OPEN_ORDERS) {
      this.reject(req, { kind: "too_many_open_orders", current: acc.open_orders.size, max: MAX_OPEN_ORDERS });
      return;
    }

    const isLimit = req.order_type.kind === "limit" || req.order_type.kind === "stop_limit" || req.order_type.kind === "trailing_stop_limit";
    const mark = this.markOf(req.symbol);

    // Price band (limit orders only — reduce-only orders are risk-reducing
    // and exempt, mirroring the Restricted-account policy).
    if (isLimit && req.price_ticks != null && !req.reduce_only) {
      const price = BigInt(req.price_ticks) * inst.tick_size_quote_minor;
      const band = inst.kind === "perp" ? PRICE_BAND_BPS_PERP : PRICE_BAND_BPS_OPTION;
      const limit = (mark * BigInt(10_000 + band)) / 10_000n;
      const floor = (mark * BigInt(10_000 - band)) / 10_000n;
      if (price > limit || price < floor) {
        this.reject(req, { kind: "outside_price_band", price_quote_minor: price, mark_quote_minor: mark });
        return;
      }
    }

    // Stop-family orders park until triggered.
    if (req.order_type.kind.startsWith("stop") || req.order_type.kind.startsWith("trailing")) {
      const id = this.nextOrderId++;
      this.pendingStops.set(id, { order: req, id, armed: false, extreme: null });
      this.journalEvent({ type: "order_resting", order: this.snapshotOrder(id, req, 0), margin_reserved_quote_minor: 0n });
      return;
    }

    // Reduce-only sanity: the order side must shrink an existing position.
    const pos = acc.positions.get(req.symbol);
    const posLots = pos?.signed_lots ?? 0;
    if (req.reduce_only) {
      const reduces = posLots !== 0 && Math.sign(posLots) === (req.side === "bid" ? -1 : 1);
      if (!reduces) {
        this.reject(req, { kind: "reduce_only_would_increase" });
        return;
      }
    }

    // Position limit.
    const projected = posLots + (req.side === "bid" ? req.qty_lots : -req.qty_lots);
    if (Math.abs(projected) > MAX_POSITION_LOTS) {
      this.reject(req, { kind: "position_limit_exceeded", projected_lots: projected, max_lots: MAX_POSITION_LOTS });
      return;
    }

    // Post-only would cross?
    const book = this.books.get(req.symbol)!;
    if (req.post_only) {
      const bb = book.bestBid();
      const ba = book.bestAsk();
      const crosses =
        req.side === "bid" ? ba != null && req.price_ticks != null && req.price_ticks >= ba : bb != null && req.price_ticks != null && req.price_ticks <= bb;
      if (crosses) {
        this.reject(req, { kind: "post_only_would_cross" });
        return;
      }
    }

    // FOK pre-check: can the book absorb the full quantity?
    if (req.tif.kind === "fok") {
      const available = this.crossingLiquidity(req.symbol, req.side, req.price_ticks);
      if (available < req.qty_lots) {
        this.reject(req, { kind: "invalid_order", reason: "fok: insufficient liquidity" });
        return;
      }
    }

    // Margin gate — reserve order margin against free equity.
    // Reduce-only orders release risk; they never consume margin.
    if (!req.reduce_only) {
      const orderMargin = orderMarginFor(inst, req.qty_lots, req.price_ticks, mark);
      const free = this.freeEquity(req.subaccount);
      if (orderMargin > free) {
        this.reject(req, { kind: "insufficient_margin", shortfall: orderMargin - free, equity_after: free - orderMargin });
        return;
      }
    }

    // Match.
    const id = this.nextOrderId++;
    const limitTicks = isLimit ? req.price_ticks : null;
    const result = book.match(req.side, limitTicks, req.qty_lots, req.subaccount, req.stp);

    if (result.stp_canceled_makers.length > 0) {
      this.journalEvent({ type: "stp_cancels", maker_ids: result.stp_canceled_makers, ts: this.now });
      for (const mid of result.stp_canceled_makers) this.cleanupMaker(mid, "canceled");
    }

    let filled = 0;
    for (const fill of result.fills) {
      this.applyFill(id, req, fill, inst, mark);
      filled += fill.qty_lots;
    }

    const remainder = req.qty_lots - filled;

    if (remainder > 0 && !result.stop_taker) {
      if (req.tif.kind === "ioc" || req.order_type.kind === "market") {
        this.journalEvent({
          type: "order_closed",
          order_id: id,
          subaccount: req.subaccount,
          symbol: req.symbol,
          order: this.snapshotOrder(id, req, filled),
          reason: "ioc_remainder",
        });
        return;
      }
      if (isLimit) {
        // Rest.
        const reserving = orderMarginFor(inst, remainder, req.price_ticks, mark);
        acc.open_orders.set(id, {
          order_id: id,
          symbol: req.symbol,
          side: req.side,
          price_ticks: req.price_ticks,
          open_lots: remainder,
          margin_reserved_quote_minor: reserving,
          is_option: inst.kind === "option",
        });
        book.insert(this.liveOrder(id, req, filled, "open"));
        this.journalEvent({ type: "order_resting", order: this.snapshotOrder(id, req, filled), margin_reserved_quote_minor: reserving });
        this.out.accountDirty.add(req.subaccount);
      }
    } else if (filled > 0) {
      this.journalEvent({
        type: "order_closed",
        order_id: id,
        subaccount: req.subaccount,
        symbol: req.symbol,
        order: this.snapshotOrder(id, req, filled),
        reason: "filled",
      });
    }
  }

  private reject(req: OrderRequest, reason: Rejection): void {
    this.journalEvent({ type: "order_rejection", payload: { request: req, reason, order_id: this.peekOrderId() } });
  }

  private peekOrderId(): number {
    return this.nextOrderId;
  }

  crossingLiquidity(symbol: Symbol, side: Side, limitTicks: number | null): number {
    const book = this.books.get(symbol);
    if (!book) return 0;
    const full = side === "bid" ? book.depthFull("ask", 10_000) : book.depthFull("bid", 10_000);
    let total = 0;
    for (const lvl of full) {
      if (limitTicks == null || (side === "bid" ? lvl.price_ticks <= limitTicks : lvl.price_ticks >= limitTicks)) {
        total += lvl.lots;
      }
    }
    return total;
  }

  private liveOrder(id: number, req: OrderRequest, filled: number, state: string): import("@perp/types").Order {
    const o: import("@perp/types").Order = {
      ...req,
      id,
      filled_lots: filled,
      state: state as import("@perp/types").OrderStateLite,
      engine_ts: this.now,
      trailing_extreme_quote_minor: null,
    };
    this.orders.set(id, o);
    return o;
  }

  private snapshotOrder(id: number, req: OrderRequest, filled: number): import("@perp/types").Order {
    return {
      id,
      subaccount: req.subaccount,
      symbol: req.symbol,
      side: req.side,
      order_type: req.order_type,
      price_ticks: req.price_ticks,
      qty_lots: req.qty_lots,
      filled_lots: filled,
      tif: req.tif,
      post_only: req.post_only,
      reduce_only: req.reduce_only,
      stp: req.stp,
      display_lots: req.display_lots,
      trailing_extreme_quote_minor: null,
      oco_group: req.oco_group,
      client_ts: req.client_ts,
      engine_ts: this.now,
      state: "open",
    };
  }

  private cleanupMaker(orderId: number, reason: "canceled" | "filled" | "expired" | "oco_sibling"): void {
    const o = this.orders.get(orderId);
    if (!o) return;
    const acc = this.accounts.get(o.subaccount);
    const book = this.books.get(o.symbol);
    if (book && o.price_ticks != null) {
      const still = book.restingOrders().find((r) => r.id === orderId);
      if (still) book.remove(still);
    }
    if (acc) {
      const info = acc.open_orders.get(orderId);
      acc.open_orders.delete(orderId);
      void info;
    }
    o.state = reason === "filled" ? "filled" : reason === "oco_sibling" ? "canceled" : reason;
    this.orders.delete(orderId);
    this.journalEvent({
      type: "order_closed",
      order_id: orderId,
      subaccount: o.subaccount,
      symbol: o.symbol,
      order: this.snapshotOrder(orderId, o, o.filled_lots),
      reason: reason === "oco_sibling" ? "oco_sibling" : reason === "filled" ? "filled" : reason === "expired" ? "expired" : "canceled",
    });
    // OCO sibling pull.
    if (o.oco_group != null) {
      const pair = this.ocoGroups.get(o.oco_group);
      if (pair) {
        const [a, b] = pair;
        const sibling = orderId === a ? b : a;
        if (this.orders.has(sibling)) this.cancelOrder(sibling, "oco_sibling");
      }
    }
  }

  cancelOrder(orderId: number, reason: "canceled" | "expired" | "oco_sibling"): void {
    const o = this.orders.get(orderId);
    if (!o) {
      // Maybe a pending stop.
      const stop = this.pendingStops.get(orderId);
      if (stop) {
        this.pendingStops.delete(orderId);
        this.journalEvent({
          type: "order_closed",
          order_id: orderId,
          subaccount: stop.order.subaccount,
          symbol: stop.order.symbol,
          order: this.snapshotOrder(orderId, stop.order, 0),
          reason: reason === "canceled" ? "canceled" : reason,
        });
      }
      return;
    }
    this.cleanupMaker(orderId, reason);
  }

  cancelAll(sub: SubaccountId, symbol: Symbol | null): void {
    const ids = [...this.orders.values()].filter((o) => o.subaccount === sub && (symbol == null || o.symbol === symbol)).map((o) => o.id);
    for (const id of ids) this.cancelOrder(id, "canceled");
    for (const [id, stop] of this.pendingStops) {
      if (stop.order.subaccount === sub && (symbol == null || stop.order.symbol === symbol)) {
        this.cancelOrder(id, "canceled");
      }
    }
  }

  amendOrder(sub: number, orderId: number, newPrice: number | null, newOpenLots: number | null): void {
    const o = this.orders.get(orderId);
    if (!o || o.subaccount !== sub) return;
    const priceChange = newPrice != null && newPrice !== o.price_ticks;
    const openNow = o.qty_lots - o.filled_lots;
    const sizeChange = newOpenLots != null ? newOpenLots - openNow : 0;
    if (priceChange || (sizeChange > 0)) {
      // Cancel-replace.
      this.cancelOrder(orderId, "canceled");
      const replaced: OrderRequest = {
        ...o,
        price_ticks: newPrice ?? o.price_ticks,
        qty_lots: Math.max(0, o.filled_lots + (newOpenLots ?? openNow)),
        client_ts: o.client_ts,
      };
      this.placeOrder(replaced, this.now);
      return;
    }
    if (sizeChange < 0) {
      // Pure decrease keeps priority.
      const book = this.books.get(o.symbol);
      const resting = book?.restingOrders().find((r) => r.id === orderId);
      if (resting) {
        resting.qty_lots = o.filled_lots + (newOpenLots ?? openNow);
        const acc = this.accounts.get(sub)!;
        const info = acc.open_orders.get(orderId);
        if (info) info.open_lots = newOpenLots ?? openNow;
      }
      this.journalEvent({
        type: "order_amended",
        payload: { order_id: orderId, subaccount: sub, symbol: o.symbol, new_open_lots: newOpenLots ?? openNow, ts: this.now },
      });
    }
  }

  private placeOco(first: OrderRequest, second: OrderRequest, now: number): void {
    const group = this.nextGroupId++;
    const before = this.nextOrderId;
    first.oco_group = group;
    second.oco_group = group;
    this.placeOrder(first, now);
    const firstId = this.nextOrderId === before + 1 ? before : null;
    const before2 = this.nextOrderId;
    this.placeOrder(second, now);
    const secondId = this.nextOrderId === before2 + 1 ? before2 : null;
    if (firstId != null && secondId != null) {
      this.ocoGroups.set(group, [firstId, secondId]);
      this.journalEvent({ type: "oco_linked", group, first: firstId, second: secondId, ts: this.now });
    }
  }

  private placeTwap(cmd: Extract<Command, { type: "place_twap" }>): void {
    const parent_id = this.nextOrderId++;
    this.twapParents.set(parent_id, {
      parent_id,
      sub: cmd.subaccount,
      symbol: cmd.symbol,
      side: cmd.side,
      total_lots: cmd.total_lots,
      slices: cmd.slices,
      placed: 0,
      lots_placed: 0,
      slice_interval_ms: cmd.slice_interval_ms,
      limit_ticks: cmd.limit_ticks,
      next_slice_ts: this.now + cmd.slice_interval_ms,
      state: "running",
    });
    this.journalEvent({
      type: "twap_opened",
      payload: {
        parent_id,
        subaccount: cmd.subaccount,
        symbol: cmd.symbol,
        side: cmd.side,
        total_lots: cmd.total_lots,
        slices: cmd.slices,
        slice_interval_ms: cmd.slice_interval_ms,
        limit_ticks: cmd.limit_ticks,
        next_slice_ts: this.now + cmd.slice_interval_ms,
        slices_placed: 0,
        lots_placed: 0,
        opened_ts: this.now,
      },
    });
  }

  private cancelTwap(parentId: number): void {
    const p = this.twapParents.get(parentId);
    if (!p || p.state !== "running") return;
    p.state = "canceled";
    this.journalEvent({
      type: "twap_closed",
      parent_id: parentId,
      subaccount: p.sub,
      reason: "canceled",
      placed_lots: p.lots_placed,
      ts: this.now,
    });
  }

  /* ═════════════════════════ fills ═════════════════════════ */

  private applyFill(takerId: number, takerReq: OrderRequest, fill: Fill, inst: Instrument, mark: bigint): void {
    const makerOrder = this.orders.get(fill.maker_order_id);
    const takerAcc = this.accounts.get(takerReq.subaccount)!;
    const makerAcc = this.accounts.get(fill.maker_subaccount)!;
    const priceMinor = BigInt(fill.price_ticks) * inst.tick_size_quote_minor;
    const lotBase = inst.lot_size_base_minor;
    const baseScale = 10n ** BigInt(inst.base_decimals);
    const notional = (BigInt(fill.qty_lots) * lotBase * priceMinor) / baseScale;

    // Fees.
    const takerTier = feeTierFor(trailingVolume(takerAcc.volume_buckets, this.now));
    const makerTier = feeTierFor(trailingVolume(makerAcc.volume_buckets, this.now));
    const takerDiscount = this.mmStates.get(takerReq.subaccount)?.discount_bps ?? 0;
    const makerDiscount = this.mmStates.get(fill.maker_subaccount)?.discount_bps ?? 0;
    const isOption = inst.kind === "option";
    const takerFee = computeFillFee("taker", isOption, notional, notional, takerTier, takerDiscount);
    const makerFee = computeFillFee("maker", isOption, notional, notional, makerTier, makerDiscount);

    // Positions (+ realized PnL into cash).
    const takerSigned = takerReq.side === "bid" ? fill.qty_lots : -fill.qty_lots;
    const makerSigned = -takerSigned;
    const beforeT = takerAcc.positions.get(takerReq.symbol)?.realized_pnl_quote_minor ?? 0n;
    const beforeM = makerAcc.positions.get(takerReq.symbol)?.realized_pnl_quote_minor ?? 0n;
    applyPositionDelta(takerAcc, takerReq.symbol, takerSigned, priceMinor, lotBase, inst.base_decimals);
    applyPositionDelta(makerAcc, takerReq.symbol, makerSigned, priceMinor, lotBase, inst.base_decimals);
    const afterT = takerAcc.positions.get(takerReq.symbol)?.realized_pnl_quote_minor ?? 0n;
    const afterM = makerAcc.positions.get(takerReq.symbol)?.realized_pnl_quote_minor ?? 0n;
    takerAcc.cash_quote_minor += afterT - beforeT;
    makerAcc.cash_quote_minor += afterM - beforeM;

    // Fees to cash.
    takerAcc.cash_quote_minor -= takerFee.fee_quote_minor;
    if (makerFee.fee_quote_minor >= 0n) {
      makerAcc.cash_quote_minor -= makerFee.fee_quote_minor;
      makerAcc.fees_paid_quote_minor += makerFee.fee_quote_minor;
    } else {
      makerAcc.cash_quote_minor += -makerFee.fee_quote_minor;
      makerAcc.maker_rebates_quote_minor += -makerFee.fee_quote_minor;
    }
    takerAcc.fees_paid_quote_minor += takerFee.fee_quote_minor > 0n ? takerFee.fee_quote_minor : 0n;

    // Maker order bookkeeping.
    if (makerOrder) {
      makerOrder.filled_lots += fill.qty_lots;
      if (makerOrder.qty_lots - makerOrder.filled_lots <= 0) {
        this.cleanupMaker(makerOrder.id, "filled");
      } else {
        const info = makerAcc.open_orders.get(makerOrder.id);
        if (info) info.open_lots = makerOrder.qty_lots - makerOrder.filled_lots;
      }
    }
    // Release taker-side order margin reservation on the filled portion.
    // (Resting remainder keeps its reservation — recomputed at rest time.)

    // Volume ledgers + venue stats.
    const dk = dayKey(this.now);
    takerAcc.volume_buckets.set(dk, (takerAcc.volume_buckets.get(dk) ?? 0n) + notional);
    makerAcc.volume_buckets.set(dk, (makerAcc.volume_buckets.get(dk) ?? 0n) + notional);
    this.stats.trades += 1;
    this.stats.lots_traded += fill.qty_lots;
    this.stats.notional_traded_quote_minor += notional;
    const ring = this.volume24.get(takerReq.symbol) ?? [];
    ring.push({ ts: this.now, notional });
    this.volume24.set(takerReq.symbol, ring);

    // Route fee income (positive only).
    const netFee = (takerFee.fee_quote_minor > 0n ? takerFee.fee_quote_minor : 0n) + (makerFee.fee_quote_minor > 0n ? makerFee.fee_quote_minor : 0n);
    if (netFee > 0n) {
      routeFee(this.revenue, netFee, { takeFeeAllocation: (alloc) => this.vaultTakeAllocation(alloc) }, this.coveragePermille(), this.config.coverage_target_permille);
    }

    // MMP windows.
    this.bumpMmp(takerReq.subaccount, inst, takerSigned, fill.qty_lots);
    this.bumpMmp(fill.maker_subaccount, inst, makerSigned, fill.qty_lots);

    // Journal + tape.
    const trade: Trade = {
      seq: ++this.tradeSeq,
      symbol: takerReq.symbol,
      taker_order_id: takerId,
      maker_order_id: fill.maker_order_id,
      taker_subaccount: takerReq.subaccount,
      maker_subaccount: fill.maker_subaccount,
      maker_side: fill.maker_is_bid ? "bid" : "ask",
      price_ticks: fill.price_ticks,
      qty_lots: fill.qty_lots,
      notional_quote_minor: notional,
      taker_fee_quote_minor: takerFee.fee_quote_minor,
      maker_fee_quote_minor: makerFee.fee_quote_minor,
      ts: this.now,
    };
    this.journalEvent({ type: "trade_executed", payload: trade });
    this.out.prints.push(trade);
    if (this.out.prints.length > 300) this.out.prints.splice(0, this.out.prints.length - 300);

    this.out.bookDirty.add(takerReq.symbol);
    this.out.accountDirty.add(takerReq.subaccount);
    this.out.accountDirty.add(fill.maker_subaccount);
    void mark;
  }

  private bumpMmp(sub: SubaccountId, inst: Instrument, signedLots: number, lots: number): void {
    const w = this.mmpWindows.get(sub);
    if (!w || !w.config || w.config.base !== inst.base_symbol) return;
    if (w.frozen_until > this.now) return;
    w.fill_lots += lots;
    w.net_delta += signedLots;
    if (w.fill_lots > w.config.amount_limit || Math.abs(w.net_delta) > w.config.delta_limit) {
      // Trip: cancel resting orders + freeze the currency.
      this.cancelAll(sub, null);
      w.frozen_until = this.now + w.config.frozen_ms;
      w.fill_lots = 0;
      w.net_delta = 0;
      this.journalEvent({ type: "mmp_tripped", subaccount: sub, base_symbol: w.config.base, ts: this.now });
    }
  }

  /* ═════════════════════════ margin ═════════════════════════ */

  legMarksOf(sub: SubaccountId): LegMark[] {
    const acc = this.accounts.get(sub);
    if (!acc) return [];
    const out: LegMark[] = [];
    for (const [symbol, pos] of acc.positions) {
      const inst = this.instruments.get(symbol);
      if (!inst) continue;
      out.push({
        symbol,
        is_option: inst.kind === "option",
        signed_lots: pos.signed_lots,
        value_quote_minor_per_base: inst.kind === "perp" ? this.spot_quote_minor : this.markOf(symbol),
        lot_size_base_minor: inst.lot_size_base_minor,
        base_decimals: inst.base_decimals,
        avg_entry_quote_minor: pos.avg_entry_quote_minor,
        option: inst.kind === "option" ? inst : undefined,
      });
    }
    return out;
  }

  collateralValueOf(sub: SubaccountId): bigint {
    const acc = this.accounts.get(sub);
    if (!acc) return 0n;
    let value = 0n;
    // BTC collateral at 20% haircut, oracle value.
    const btc = acc.collateral.get("BTC") ?? 0n;
    if (btc > 0n) value += (btc * this.spot_quote_minor) / 10n ** 8n * 80n / 100n;
    return value;
  }

  marginOf(sub: SubaccountId): { equity: bigint; initial: bigint; maintenance: bigint; order_margin: bigint; health: "healthy" | "restricted" | "liquidation" } {
    const acc = this.accounts.get(sub);
    if (!acc) return { equity: 0n, initial: 0n, maintenance: 0n, order_margin: 0n, health: "healthy" };
    const legs = this.legMarksOf(sub);
    const collateral = this.collateralValueOf(sub);
    const equity = accountEquity(acc, legs, collateral);
    const ivs: Record<string, number> = {};
    for (const s of this.ivs.keys()) ivs[s] = this.ivs.get(s)!;
    const { initial_quote_minor, maintenance_quote_minor } = sfpmMargin(legs, this.spot_quote_minor, ivs, this.now);
    let orderMargin = 0n;
    for (const info of acc.open_orders.values()) orderMargin += info.margin_reserved_quote_minor;
    const health = equity >= initial_quote_minor ? "healthy" : equity >= maintenance_quote_minor ? "restricted" : "liquidation";
    return { equity, initial: initial_quote_minor, maintenance: maintenance_quote_minor, order_margin: orderMargin, health };
  }

  freeEquity(sub: SubaccountId): bigint {
    const m = this.marginOf(sub);
    return m.equity - m.order_margin - m.initial;
  }

  coveragePermille(): number {
    const liab = this.totalLiabilities();
    if (liab === 0n) return 10_000;
    return Math.round((Number(this.insurance_balance) / Number(liab)) * 1000);
  }

  totalLiabilities(): bigint {
    let total = 0n;
    for (const acc of this.accounts.values()) {
      const m = this.marginOf(acc.id);
      total += m.equity > 0n ? m.equity : 0n;
    }
    return total;
  }

  /* ═════════════════════════ vaults ═════════════════════════ */

  createVault(revenueShareBps: number, now: number): void {
    const vault_id = this.nextVaultId++;
    this.vaults.set(vault_id, {
      vault_id,
      revenue_share_bps: revenueShareBps,
      total_shares: 0n,
      nav_quote_minor: 0n,
      epoch: 0,
      last_epoch_ts: now,
      pending_subs: [],
      pending_redeems: [],
      lifetime_credit: 0n,
    });
    this.journalEvent({ type: "vault_opened", vault_id, revenue_share_bps: revenueShareBps, ts: now });
  }

  vaultSubscribe(vault_id: number, sub: SubaccountId, amount: bigint): void {
    const v = this.vaults.get(vault_id);
    const acc = this.accounts.get(sub);
    if (!v || !acc || amount <= 0n || acc.cash_quote_minor < amount) return;
    acc.cash_quote_minor -= amount;
    v.nav_quote_minor += amount; // assets move immediately; shares mint at epoch
    v.pending_subs.push({ sub, amount });
    this.journalEvent({ type: "vault_queued", vault_id, subaccount: sub, is_subscribe: true, amount, ts: this.now });
    this.out.accountDirty.add(sub);
  }

  vaultRedeem(vault_id: number, sub: SubaccountId, shares: bigint): void {
    const v = this.vaults.get(vault_id);
    const acc = this.accounts.get(sub);
    const held = acc?.vault_positions.get(vault_id);
    if (!v || !acc || !held || shares <= 0n || held.shares < shares) return;
    held.shares -= shares;
    v.pending_redeems.push({ sub, shares });
    this.journalEvent({ type: "vault_queued", vault_id, subaccount: sub, is_subscribe: false, amount: shares, ts: this.now });
    this.out.accountDirty.add(sub);
  }

  /** LP underwriters take their bps of the insurance allocation (G-16). */
  vaultTakeAllocation(allocation: bigint): bigint {
    let taken = 0n;
    for (const v of [...this.vaults.values()].sort((a, b) => a.vault_id - b.vault_id)) {
      const take = bpsOfUp(allocation, v.revenue_share_bps);
      v.nav_quote_minor += take;
      v.lifetime_credit += take;
      taken += take;
    }
    return taken;
  }

  settleVaultEpochs(now: number): void {
    for (const v of this.vaults.values()) {
      const navPer = v.total_shares > 0n ? v.nav_quote_minor / v.total_shares : 1_00n;
      const flows: { subaccount: SubaccountId; amount: bigint }[] = [];
      let subShares = 0n;
      let subQuote = 0n;
      let redeemedShares = 0n;
      let redeemedQuote = 0n;
      // Subscriptions settle at current NAV (cash already in the vault).
      for (const s of v.pending_subs) {
        const shares = navPer > 0n ? s.amount / navPer : s.amount / 1_00n;
        v.total_shares += shares;
        subShares += shares;
        subQuote += s.amount;
        const acc = this.accounts.get(s.sub);
        acc?.vault_positions.set(v.vault_id, {
          shares: (acc.vault_positions.get(v.vault_id)?.shares ?? 0n) + shares,
          last_claim_quote_minor: acc.vault_positions.get(v.vault_id)?.last_claim_quote_minor ?? 0n,
        });
        flows.push({ subaccount: s.sub, amount: shares });
      }
      // Redemptions settle at current NAV.
      for (const r of v.pending_redeems) {
        const amount = r.shares * navPer;
        v.total_shares -= r.shares;
        v.nav_quote_minor -= amount;
        redeemedShares += r.shares;
        redeemedQuote += amount;
        const acc = this.accounts.get(r.sub);
        if (acc) {
          acc.cash_quote_minor += amount;
          this.out.accountDirty.add(r.sub);
        }
        flows.push({ subaccount: r.sub, amount: -amount });
      }
      v.pending_subs = [];
      v.pending_redeems = [];
      v.epoch += 1;
      v.last_epoch_ts = now;
      const afterPer = v.total_shares > 0n ? v.nav_quote_minor / v.total_shares : navPer;
      this.journalEvent({
        type: "vault_epoch_settled",
        payload: {
          vault_id: v.vault_id,
          epoch: v.epoch,
          nav_per_share_quote_minor: afterPer,
          subscribed_shares: subShares,
          subscribed_quote_minor: subQuote,
          redeemed_shares: redeemedShares,
          redeemed_quote_minor: redeemedQuote,
          insurance_credit_quote_minor: 0n,
          nav_after_quote_minor: v.nav_quote_minor,
          flows,
          ts: now,
        },
      });
    }
  }

  /* ═════════════════════════ MM program ═════════════════════════ */

  mmEnroll(sub: SubaccountId): void {
    this.mmStates.set(sub, {
      enrolled: true,
      tier: null,
      discount_bps: 0,
      uptime_permille: 0,
      samples: [],
      total_ticks: 0,
      next_sample_at: this.now + 30_000 + Math.floor(this.rng.next() * 300_000),
      next_review_at: this.now + 24 * 3600 * 1000,
    });
    this.journalEvent({ type: "mm_enrolled", subaccount: sub, ts: this.now });
  }

  /* ═════════════════════════ collateral ═════════════════════════ */

  convertCollateral(sub: number, from: string, to: string, amount: bigint): void {
    const acc = this.accounts.get(sub);
    if (!acc || amount <= 0n) return;
    if (from === "USD" && to === "BTC") {
      if (amount > acc.cash_quote_minor) return;
      // quote minor → sats at the oracle spot (zero fee, G-17).
      const toAmount = (amount * 10n ** 8n) / this.spot_quote_minor;
      acc.cash_quote_minor -= amount;
      acc.collateral.set("BTC", (acc.collateral.get("BTC") ?? 0n) + toAmount);
      this.journalConversion(sub, from, to, amount, toAmount);
    } else if (from === "BTC" && to === "USD") {
      const bal = acc.collateral.get("BTC") ?? 0n;
      if (amount > bal) return;
      const toAmount = (amount * this.spot_quote_minor) / 10n ** 8n;
      acc.collateral.set("BTC", bal - amount);
      acc.cash_quote_minor += toAmount;
      this.journalConversion(sub, from, to, amount, toAmount);
    }
  }

  private journalConversion(sub: number, from: string, to: string, amount: bigint, toAmount: bigint): void {
    const rate = to === "USD" ? this.spot_quote_minor : (10n ** 10n) / this.spot_quote_minor;
    this.journalEvent({
      type: "collateral_conversion",
      payload: { subaccount: sub, from, to, from_amount_minor: amount, to_amount_minor: toAmount, rate_quote_minor_per_unit: rate, ts: this.now },
    });
    this.out.accountDirty.add(sub);
  }

  /* ═════════════════════════ RFQ ═════════════════════════ */

  rfqCreate(cmd: Extract<Command, { type: "rfq_create" }>): void {
    const rfq_id = this.nextRfqId++;
    this.rfqs.set(rfq_id, {
      rfq_id,
      taker: cmd.taker,
      legs: cmd.legs.map((l) => ({ symbol: l.symbol, side: l.side, qty_lots: l.qty_lots })),
      counterparties: cmd.counterparties.length ? cmd.counterparties : [1, 2],
      min_total_cost_quote_minor: cmd.min_total_cost_quote_minor,
      max_total_cost_quote_minor: cmd.max_total_cost_quote_minor,
      created_at: this.now,
      expires_at: this.now + cmd.ttl_ms,
      status: "open",
      quotes: [],
      executed_quote_id: null,
      trades: null,
      quotes_ttl: new Map(),
    });
    this.journalEvent({
      type: "rfq_created",
      taker: cmd.taker,
      legs: cmd.legs.map((l) => ({ symbol: l.symbol, side: l.side, qty_lots: l.qty_lots })),
      counterparties: cmd.counterparties,
      min_total_cost_quote_minor: cmd.min_total_cost_quote_minor,
      max_total_cost_quote_minor: cmd.max_total_cost_quote_minor,
      ttl_ms: cmd.ttl_ms,
      ts: this.now,
    });
    // Dealers respond on the ticker (sim MMs quote near mark).
  }

  rfqQuote(cmd: Extract<Command, { type: "rfq_quote" }>): void {
    const rfq = this.rfqs.get(cmd.rfq_id);
    if (!rfq || rfq.status !== "open" && rfq.status !== "quoted") return;
    const quote_id = this.nextOrderId++;
    rfq.status = "quoted";
    let total = 0n;
    rfq.legs.forEach((leg, i) => {
      const priceTicks = cmd.leg_prices_ticks[i] ?? 1;
      const inst = this.instruments.get(leg.symbol);
      const priceMinor = inst ? BigInt(priceTicks) * inst.tick_size_quote_minor : 0n;
      // Taker pays (bid side) or receives (ask side) per leg.
      const cost = (BigInt(leg.qty_lots) * (inst?.lot_size_base_minor ?? 1n) * priceMinor) / 10n ** BigInt(inst?.base_decimals ?? 0);
      total += leg.side === "bid" ? cost : -cost;
    });
    rfq.quotes.push({
      quote_id,
      rfq_id: cmd.rfq_id,
      maker: cmd.maker,
      leg_prices_ticks: cmd.leg_prices_ticks,
      total_cost_quote_minor: total,
      ttl_ms: cmd.ttl_ms,
      expires_at: this.now + cmd.ttl_ms,
    });
    this.journalEvent({
      type: "rfq_quoted",
      rfq_id: cmd.rfq_id,
      maker: cmd.maker,
      leg_prices_ticks: cmd.leg_prices_ticks,
      ttl_ms: cmd.ttl_ms,
      ts: this.now,
    });
  }

  rfqExecute(taker: number, rfq_id: number, quote_id: number): void {
    const rfq = this.rfqs.get(rfq_id);
    const quote = rfq?.quotes.find((q) => q.quote_id === quote_id);
    if (!rfq || !quote || rfq.status === "executed" || rfq.status === "expired" || rfq.status === "cancelled") return;
    if (this.now > quote.expires_at) return;
    rfq.status = "executed";
    rfq.executed_quote_id = quote_id;

    // Execute legs as bilateral fills through the margin path.
    const trades: Trade[] = [];
    const fees: bigint[] = [];
    rfq.legs.forEach((leg, i) => {
      const inst = this.instruments.get(leg.symbol);
      if (!inst) return;
      const priceTicks = quote.leg_prices_ticks[i] ?? 1;
      const priceMinor = BigInt(priceTicks) * inst.tick_size_quote_minor;
      const baseScale = 10n ** BigInt(inst.base_decimals);
      const notional = (BigInt(leg.qty_lots) * inst.lot_size_base_minor * priceMinor) / baseScale;
      const takerAcc = this.accounts.get(taker)!;
      const makerAcc = this.accounts.get(quote.maker)!;
      const takerSigned = leg.side === "bid" ? leg.qty_lots : -leg.qty_lots;
      applyPositionDelta(takerAcc, leg.symbol, takerSigned, priceMinor, inst.lot_size_base_minor, inst.base_decimals);
      applyPositionDelta(makerAcc, leg.symbol, -takerSigned, priceMinor, inst.lot_size_base_minor, inst.base_decimals);
      const tier = feeTierFor(trailingVolume(takerAcc.volume_buckets, this.now));
      const fee = computeFillFee("taker", inst.kind === "option", notional, notional, tier, this.mmStates.get(taker)?.discount_bps ?? 0);
      takerAcc.cash_quote_minor -= fee.fee_quote_minor;
      if (fee.fee_quote_minor > 0n) {
        routeFee(this.revenue, fee.fee_quote_minor, { takeFeeAllocation: (a) => this.vaultTakeAllocation(a) }, this.coveragePermille(), this.config.coverage_target_permille);
      }
      const trade: Trade = {
        seq: ++this.tradeSeq,
        symbol: leg.symbol,
        taker_order_id: 0,
        maker_order_id: 0,
        taker_subaccount: taker,
        maker_subaccount: quote.maker,
        maker_side: leg.side === "bid" ? "ask" : "bid",
        price_ticks: priceTicks,
        qty_lots: leg.qty_lots,
        notional_quote_minor: notional,
        taker_fee_quote_minor: fee.fee_quote_minor,
        maker_fee_quote_minor: 0n,
        ts: this.now,
      };
      trades.push(trade);
      fees.push(fee.fee_quote_minor);
      this.out.prints.push(trade);
      this.out.accountDirty.add(taker);
      this.out.accountDirty.add(quote.maker);
    });
    rfq.trades = trades;
    this.journalEvent({ type: "rfq_settled", rfq_id, quote_id, taker, maker: quote.maker, trades, taker_fees_quote_minor: fees });
  }

  rfqCancel(sub: number, rfq_id: number | null, quote_id: number | null): void {
    if (rfq_id != null) {
      const rfq = this.rfqs.get(rfq_id);
      if (rfq && rfq.taker === sub && (rfq.status === "open" || rfq.status === "quoted")) {
        rfq.status = "cancelled";
        this.journalEvent({ type: "rfq_closed", rfq_id, quote_id, reason: "cancelled" });
      }
    }
  }

  registerBlock(cmd: Extract<Command, { type: "block_trade" }>): void {
    const block_id = this.nextBlockId++;
    let total = 0n;
    for (const leg of cmd.legs) {
      const inst = this.instruments.get(leg.symbol);
      if (!inst) continue;
      total += (BigInt(leg.qty_lots) * inst.lot_size_base_minor * BigInt(leg.price_ticks) * inst.tick_size_quote_minor) / 10n ** BigInt(inst.base_decimals);
      // Positions move now (venue-cleared), tape prints later.
      const takerAcc = this.accounts.get(cmd.taker)!;
      const makerAcc = this.accounts.get(cmd.maker)!;
      const signed = leg.side === "bid" ? leg.qty_lots : -leg.qty_lots;
      const priceMinor = BigInt(leg.price_ticks) * inst.tick_size_quote_minor;
      applyPositionDelta(takerAcc, leg.symbol, signed, priceMinor, inst.lot_size_base_minor, inst.base_decimals);
      applyPositionDelta(makerAcc, leg.symbol, -signed, priceMinor, inst.lot_size_base_minor, inst.base_decimals);
    }
    const broadcast_ts = this.now + 15 * 60 * 1000;
    this.blocks.push({ block_id, taker: cmd.taker, maker: cmd.maker, legs: cmd.legs, total_notional: total, registered_ts: this.now, broadcast_ts, printed: false });
    this.journalEvent({
      type: "block_registered",
      taker: cmd.taker,
      maker: cmd.maker,
      legs: cmd.legs,
      total_notional_quote_minor: total,
      taker_fees_quote_minor: [],
      broadcast_ts,
    });
  }

  /* ═════════════════════════ exercises ═════════════════════════ */

  requestExercise(sub: SubaccountId, symbol: Symbol, lots: QtyLots): void {
    const inst = this.instruments.get(symbol);
    const acc = this.accounts.get(sub);
    const pos = acc?.positions.get(symbol);
    if (!inst || inst.kind !== "option" || inst.exercise_style !== "american") {
      this.journalEvent({ type: "exercise_rejected", subaccount: sub, symbol, requested_lots: lots, reason: "not american-exercisable", ts: this.now });
      return;
    }
    if (!acc || !pos || pos.signed_lots < lots || lots <= 0) {
      this.journalEvent({ type: "exercise_rejected", subaccount: sub, symbol, requested_lots: lots, reason: "insufficient long position", ts: this.now });
      return;
    }
    const request_id = this.nextRequestId++;
    const settle_at = this.now + inst.american.settlement_twap_ms;
    this.pendingExercises.set(request_id, {
      request_id,
      subaccount: sub,
      symbol,
      lots,
      requested_at: this.now,
      settle_at,
      twap_sum: 0n,
      twap_n: 0,
    });
    this.journalEvent({ type: "exercise_queued", request_id, subaccount: sub, symbol, lots, requested_at: this.now, settle_at });
  }

  /* ═════════════════════════ output drain ═════════════════════════ */

  drain(): EngineOutputs {
    const out = this.out;
    this.out = { journal: [], prints: [], bookDirty: new Set(), accountDirty: new Set(), marketDirty: false };
    return out;
  }

  /* ── helpers used by the ticker (venue-ticker.ts) ── */

  bookOf(symbol: Symbol): Book | undefined {
    return this.books.get(symbol);
  }

  emitProviderObservation(provider: string, price: bigint): void {
    this.oracle.observe(provider, price, this.now);
    this.journalEvent({ type: "provider_observed", base_symbol: this.config.base_symbol, provider, ts: this.now, price_quote_minor: price });
  }

  noteOracleState(): void {
    const mark = this.oracle.mark(this.now);
    if (mark != null) this.spot_quote_minor = mark;
  }

  /** Peek the next RFQ id without consuming it (ticker pre-schedules responses). */
  nextRfqIdPeek(): number {
    return this.nextRfqId;
  }

  greeksFor(sub: SubaccountId): { symbol: Symbol; delta: number; gamma: number; vega: number; theta: number }[] {
    const acc = this.accounts.get(sub);
    if (!acc) return [];
    const out = [];
    for (const [symbol, pos] of acc.positions) {
      const inst = this.instruments.get(symbol);
      if (!inst) continue;
      if (inst.kind === "perp") {
        out.push({ symbol, delta: pos.signed_lots, gamma: 0, vega: 0, theta: 0 });
      } else {
        const g = optionGreeks(inst, this.spot_quote_minor, this.ivs.get(symbol) ?? 5500, this.now);
        const scale = pos.signed_lots * Number(inst.lot_size_base_minor);
        out.push({
          symbol,
          delta: g.delta * scale,
          gamma: g.gamma * scale,
          vega: g.vega * scale,
          theta: g.theta * scale,
        });
      }
    }
    return out;
  }
}
