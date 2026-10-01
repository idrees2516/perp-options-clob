/**
 * Accounts, positions, and portfolio margin.
 *
 * Margin follows the SFPM discipline of poc-margin: the worst-case loss of
 * the *whole book* under a standardized scenario grid (spot ±{1, ½, ¼, 0} ×
 * scan range × vol shifts), with a SOMC tail floor for naked short options.
 * Hedges net out because every leg reprices in every scenario.
 */

import type {
  Instrument,
  MoneyMinor,
  OptionMarket,
  PerpMarket,
  Position,
  SignedMoneyMinor,
  SubaccountId,
  Symbol,
} from "@perp/types";
import { effectiveTteYears } from "@perp/types";
import { bsPrice } from "./pricing";

/** SFPM parameters (poc-margin UnderlyingMarginParams defaults). */
export interface SfpmParams {
  scan_range_bps: number; // 375 default
  initial_multiplier_pct: number; // 140 default
  vol_shift_pct: number; // 25 default
  maintenance_ratio: number; // MM/IM = 375/500 default
}

export const DEFAULT_SFPM: SfpmParams = {
  scan_range_bps: 375,
  initial_multiplier_pct: 140,
  vol_shift_pct: 25,
  maintenance_ratio: 375 / 500,
};

export interface OpenOrderInfo {
  order_id: number;
  symbol: Symbol;
  side: "bid" | "ask";
  price_ticks: number | null;
  open_lots: number;
  /** Reserved order margin. */
  margin_reserved_quote_minor: bigint;
  is_option: boolean;
}

export interface SimAccount {
  id: SubaccountId;
  cash_quote_minor: bigint;
  positions: Map<Symbol, Position>;
  open_orders: Map<number, OpenOrderInfo>;
  fees_paid_quote_minor: bigint;
  maker_rebates_quote_minor: bigint;
  funding_pnl_quote_minor: bigint;
  /** day-bucketed 30-day trailing volume (day → notional). */
  volume_buckets: Map<string, bigint>;
  collateral: Map<string, bigint>;
  vault_positions: Map<number, { shares: bigint; last_claim_quote_minor: bigint }>;
}

export function newAccount(id: SubaccountId): SimAccount {
  return {
    id,
    cash_quote_minor: 0n,
    positions: new Map(),
    open_orders: new Map(),
    fees_paid_quote_minor: 0n,
    maker_rebates_quote_minor: 0n,
    funding_pnl_quote_minor: 0n,
    volume_buckets: new Map(),
    collateral: new Map(),
    vault_positions: new Map(),
  };
}

export function positionOf(acc: SimAccount, symbol: Symbol): Position {
  let p = acc.positions.get(symbol);
  if (!p) {
    p = { symbol, signed_lots: 0, avg_entry_quote_minor: 0n, realized_pnl_quote_minor: 0n };
    acc.positions.set(symbol, p);
  }
  return p;
}

/** Scale: 10^baseDecimals — base minor units → whole base units. */
export function baseScale(baseDecimals: number): bigint {
  return 10n ** BigInt(baseDecimals);
}

/** Extend or reduce a position; VWAP entry, realized PnL on reduction. */
export function applyPositionDelta(
  acc: SimAccount,
  symbol: Symbol,
  deltaLots: number,
  priceMinorPerBase: bigint,
  lotBaseMinor: bigint,
  baseDecimals: number,
): void {
  const scale = baseScale(baseDecimals);
  const p = positionOf(acc, symbol);
  if (p.signed_lots === 0 || Math.sign(deltaLots) === Math.sign(p.signed_lots)) {
    // extension — VWAP
    const oldAbs = BigInt(Math.abs(p.signed_lots));
    const newAbs = oldAbs + BigInt(Math.abs(deltaLots));
    p.avg_entry_quote_minor =
      newAbs === 0n
        ? 0n
        : (p.avg_entry_quote_minor * oldAbs + priceMinorPerBase * BigInt(Math.abs(deltaLots))) / newAbs;
    p.signed_lots += deltaLots;
  } else {
    // reduction — realize PnL on the closed portion (exact single final division)
    const closed = Math.min(Math.abs(deltaLots), Math.abs(p.signed_lots));
    const sign = Math.sign(p.signed_lots);
    const pnl = (BigInt(closed) * lotBaseMinor * (priceMinorPerBase - p.avg_entry_quote_minor) * BigInt(sign)) / scale;
    p.realized_pnl_quote_minor += pnl;
    p.signed_lots += deltaLots;
    if (p.signed_lots === 0) {
      p.avg_entry_quote_minor = 0n;
    } else if (Math.sign(p.signed_lots) !== sign) {
      // flipped through zero — new entry at fill price
      p.avg_entry_quote_minor = priceMinorPerBase;
    }
  }
  if (p.signed_lots === 0) {
    acc.positions.delete(symbol);
  }
}

/** One position leg with its live mark (perp: spot per base; option: premium per base). */
export interface LegMark {
  symbol: Symbol;
  is_option: boolean;
  signed_lots: number;
  value_quote_minor_per_base: bigint;
  lot_size_base_minor: bigint;
  base_decimals: number;
  avg_entry_quote_minor: bigint;
  /** Option context for scenario repricing. */
  option?: OptionMarket;
}

/**
 * Scenario-grid portfolio margin. Returns initial and maintenance
 * requirements in quote minor units.
 */
export function sfpmMargin(
  legs: LegMark[],
  spotQuoteMinor: bigint,
  ivBpsBySymbol: Record<Symbol, number>,
  now: number,
  params: SfpmParams = DEFAULT_SFPM,
): { initial_quote_minor: bigint; maintenance_quote_minor: bigint } {
  if (legs.length === 0) return { initial_quote_minor: 0n, maintenance_quote_minor: 0n };

  const scan = params.scan_range_bps / 10_000;
  const volShift = 1 + params.vol_shift_pct / 100;
  const spotNum = Number(spotQuoteMinor);

  const valueAt = (spotScale: number, volScale: number): bigint => {
    let total = 0n;
    for (const leg of legs) {
      const lotsBase = BigInt(Math.abs(leg.signed_lots)) * leg.lot_size_base_minor;
      const scale = baseScale(leg.base_decimals);
      const sign = BigInt(Math.sign(leg.signed_lots));
      if (!leg.is_option) {
        total += (sign * lotsBase * BigInt(Math.round(spotNum * spotScale))) / scale;
      } else {
        const o = leg.option!;
        const strike = Number(o.strike_quote_minor);
        const tau = effectiveTteYears(o, now);
        const iv = ((ivBpsBySymbol[o.symbol] ?? 5500) / 10_000) * volScale;
        const premium = bsPrice(spotNum * spotScale / 100, strike / 100, tau, iv, o.kind_of) * 100;
        total += (sign * lotsBase * BigInt(Math.round(premium))) / scale;
      }
    }
    return total;
  };

  const base = valueAt(1, 1);
  let worstLoss = 0n;
  for (const spotMult of [1 + scan, 1 + scan / 2, 1 - scan / 2, 1 - scan, 1 + scan / 4, 1 - scan / 4]) {
    for (const volMult of [volShift, 1, 1 / volShift]) {
      const loss = base - valueAt(spotMult, volMult);
      if (loss > worstLoss) worstLoss = loss;
    }
  }

  // SOMC floor: naked short options pay the tail floor.
  let somc = 0n;
  for (const leg of legs) {
    if (leg.is_option && leg.signed_lots < 0 && leg.option) {
      somc += (BigInt(-leg.signed_lots) * leg.lot_size_base_minor * spotQuoteMinor * 5n) / (100n * baseScale(leg.base_decimals));
    }
  }

  const initial = (maxB(worstLoss, somc) * BigInt(params.initial_multiplier_pct)) / 100n;
  return {
    initial_quote_minor: initial,
    maintenance_quote_minor: (initial * BigInt(Math.round(params.maintenance_ratio * 1000))) / 1000n,
  };
}

function maxB(a: bigint, b: bigint): bigint {
  return a > b ? a : b;
}

/**
 * Simplified order-margin reservation (order sizing before rest):
 * perps: notional × IM ratio; option buys: premium; option shorts: SOMC.
 */
export function orderMarginFor(
  instrument: Instrument,
  qtyLots: number,
  priceTicks: number | null,
  spotQuoteMinor: bigint,
): bigint {
  const lotBase = instrument.lot_size_base_minor;
  const scale = baseScale(instrument.base_decimals);
  if (instrument.kind === "perp") {
    const price = priceTicks != null ? instrument.tick_size_quote_minor * BigInt(priceTicks) : spotQuoteMinor;
    const notional = (BigInt(qtyLots) * lotBase * price) / scale;
    return (notional * 5n) / 100n; // IM 500 bps
  }
  const o = instrument as OptionMarket;
  const premiumPerBase = priceTicks != null ? o.tick_size_quote_minor * BigInt(priceTicks) : 0n;
  if (priceTicks != null && priceTicks > 0) {
    // direction unknown at this layer — reserve max(buy premium, short SOMC)
    const premium = (BigInt(qtyLots) * lotBase * premiumPerBase) / scale;
    const somc = (BigInt(qtyLots) * lotBase * spotQuoteMinor * 5n) / (100n * scale);
    return premium > somc ? premium : somc;
  }
  return (BigInt(qtyLots) * lotBase * spotQuoteMinor * 5n) / (100n * scale);
}

/** Account equity: cash + unrealized position value + collateral haircut value. */
export function accountEquity(
  acc: SimAccount,
  legMarks: LegMark[],
  collateralValueMinor: bigint,
): bigint {
  let unrealized = 0n;
  for (const leg of legMarks) {
    const lotsBase = BigInt(Math.abs(leg.signed_lots)) * leg.lot_size_base_minor;
    const sign = BigInt(Math.sign(leg.signed_lots));
    unrealized += (sign * lotsBase * (leg.value_quote_minor_per_base - leg.avg_entry_quote_minor)) / baseScale(leg.base_decimals);
  }
  return acc.cash_quote_minor + unrealized + collateralValueMinor;
}
