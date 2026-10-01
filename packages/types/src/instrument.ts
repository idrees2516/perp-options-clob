/**
 * Instrument model — mirrors crates/core/src/instrument.rs.
 */

import type {
  BaseSymbol,
  ExerciseStyle,
  MoneyMinor,
  OptionKind,
  OptionVariant,
  PriceTicks,
  QtyLots,
  Symbol,
  TimestampMs,
} from "./primitives";

/** BitMEX-style premium + interest funding (poc-core FundingParams). */
export interface FundingParams {
  /** Funding interval, e.g. 8h in ms. */
  interval_ms: number;
  /** Interest component, bps per interval (default 1). */
  interest_rate_bps_per_interval: number;
  /** Premium clamp, bps (default 5). */
  premium_clamp_bps: number;
  /** Hard rate cap, bps (default 75). */
  rate_cap_bps: number;
}

export const DEFAULT_FUNDING_PARAMS: FundingParams = {
  interval_ms: 8 * 60 * 60 * 1000,
  interest_rate_bps_per_interval: 1,
  premium_clamp_bps: 5,
  rate_cap_bps: 75,
};

/** American exercise parameters (v0.6). */
export interface AmericanParams {
  /** TWAP window for tender settlement (default 30 min). */
  settlement_twap_ms: number;
  /** Exercise fee in bps of intrinsic (default 5). */
  exercise_fee_bps: number;
}

export const DEFAULT_AMERICAN_PARAMS: AmericanParams = {
  settlement_twap_ms: 30 * 60 * 1000,
  exercise_fee_bps: 5,
};

/** Everlasting (Paradigm roll) parameters. */
export interface EverlastingParams {
  /** Roll interval (default 1h). */
  interval_ms: number;
  /** Effective maturity = interval × multiple (default 24 → ~1 day). */
  maturity_multiple: number;
}

export const DEFAULT_EVERLASTING_PARAMS: EverlastingParams = {
  interval_ms: 60 * 60 * 1000,
  maturity_multiple: 24,
};

/** Option margin parameters. */
export interface OptionMarginParams {
  /** Short-option minimum charge, bps of underlying notional (SOMC floor, default 500). */
  short_option_min_bps: number;
  /** Liquidation fee for option legs (default 125 bps). */
  liquidation_fee_bps: number;
}

export const DEFAULT_OPTION_MARGIN_PARAMS: OptionMarginParams = {
  short_option_min_bps: 500,
  liquidation_fee_bps: 125,
};

/** Linear perpetual market. */
export interface PerpMarket {
  kind: "perp";
  symbol: Symbol;
  base_symbol: BaseSymbol;
  quote_decimals: number;
  base_decimals: number;
  /** Tick size in quote minor units (BTC-PERP default: 100 = $1.00). */
  tick_size_quote_minor: MoneyMinor;
  /** Lot size in base minor units (BTC-PERP default: 100 = 0.001 BTC @ 5dp). */
  lot_size_base_minor: MoneyMinor;
  initial_margin_ratio_bps: number;
  maintenance_margin_ratio_bps: number;
  price_band_bps: number;
  max_order_lots: QtyLots;
  funding: FundingParams;
}

/** Cash-settled option market (European or American; dated or everlasting). */
export interface OptionMarket {
  kind: "option";
  symbol: Symbol;
  base_symbol: BaseSymbol;
  kind_of: OptionKind; // "call" | "put" — named kind_of to avoid kind clash
  strike_quote_minor: MoneyMinor;
  expiry_ts_ms: TimestampMs; // dated only; everlasting carries roll interval instead
  variant: OptionVariant; // "dated" | "everlasting"
  exercise_style: ExerciseStyle; // "european" | "american"
  american: AmericanParams;
  everlasting: EverlastingParams;
  quote_decimals: number;
  base_decimals: number;
  tick_size_quote_minor: MoneyMinor; // option default: 50 = $0.50
  lot_size_base_minor: MoneyMinor; // option default: 1_000 = 0.01 BTC @ 5dp
  price_band_bps: number;
  max_order_lots: QtyLots;
  margin: OptionMarginParams;
}

export type Instrument = PerpMarket | OptionMarket;

/** Instrument kind discriminator for views. */
export type InstrumentKind = "perp" | "option";

/** Auto-listing template (G-34). */
export interface OptionTemplate {
  quote_decimals: number;
  base_decimals: number;
  tick_size_quote_minor: MoneyMinor;
  lot_size_base_minor: MoneyMinor;
  price_band_bps: number;
  max_order_lots: QtyLots;
  margin: OptionMarginParams;
  everlasting: EverlastingParams;
  exercise_style: ExerciseStyle;
  american: AmericanParams;
  /** Anchor IV, bps of 100% (55% default → 5500). */
  anchor_iv_bps: number;
}

export interface UnderlyingListing {
  base_symbol: BaseSymbol;
  strike_spacing_quote_minor: MoneyMinor;
  strike_steps: number;
  /** Tenors for dated expiries, ms from listing. */
  tenors_ms: number[];
  /** Everlasting strikes kept live around spot. */
  everlasting_strikes: number;
  listing_band_bps: number;
  delist_band_bps: number;
  rebase_band_bps: number;
  auction_ms: number;
  template: OptionTemplate;
}

/** Uniform accessors — mirrors Rust `Instrument` trait surface. */
export function instrumentSymbol(i: Instrument): Symbol {
  return i.symbol;
}
export function instrumentBase(i: Instrument): BaseSymbol {
  return i.base_symbol;
}
export function instrumentTickSize(i: Instrument): MoneyMinor {
  return i.tick_size_quote_minor;
}
export function instrumentLotSize(i: Instrument): MoneyMinor {
  return i.lot_size_base_minor;
}
export function instrumentMaxOrderLots(i: Instrument): QtyLots {
  return i.max_order_lots;
}
export function isOption(i: Instrument): i is OptionMarket {
  return i.kind === "option";
}

/** Option symbol convention: BTC-20260327-80000-C */
export function optionSymbol(
  base: BaseSymbol,
  expiry: TimestampMs,
  strikeQuoteMinor: MoneyMinor,
  kind: OptionKind,
): Symbol {
  const d = new Date(expiry);
  const y = d.getUTCFullYear();
  const m = String(d.getUTCMonth() + 1).padStart(2, "0");
  const day = String(d.getUTCDate()).padStart(2, "0");
  const strike = Number(strikeQuoteMinor) / 100; // 2dp quote
  const strikeStr = strike % 1 === 0 ? strike.toFixed(0) : strike.toFixed(0);
  return `${base}-${y}${m}${day}-${strikeStr}-${kind === "call" ? "C" : "P"}`;
}

/** Parse an option symbol back into parts (display helpers). */
export function parseOptionSymbol(symbol: Symbol): {
  base: BaseSymbol;
  expiry: string;
  strike: number;
  kind: OptionKind;
} | null {
  const m = /^(.+)-(\d{8})-(\d+)-([CP])$/.exec(symbol);
  if (!m) return null;
  return {
    base: m[1],
    expiry: m[2],
    strike: Number(m[3]),
    kind: m[4] === "C" ? "call" : "put",
  };
}

/** Effective time-to-expiry years for pricing/margin (everlasting = interval × multiple). */
export function effectiveTteYears(o: OptionMarket, now: TimestampMs): number {
  if (o.variant === "everlasting") {
    return (o.everlasting.interval_ms * o.everlasting.maturity_multiple) / (365 * 24 * 3600 * 1000);
  }
  const ms = Math.max(0, o.expiry_ts_ms - now);
  return ms / (365 * 24 * 3600 * 1000);
}

/** Mark for an instrument at a given spot (Rust `Mark` enum, analytics fields are f64). */
export type Mark =
  | { kind: "perp"; symbol: Symbol; mark_quote_minor_per_base: MoneyMinor }
  | {
      kind: "option";
      symbol: Symbol;
      premium_quote_minor_per_base: MoneyMinor;
      iv: number; // e.g. 0.55
      tau_years: number;
    };

export interface MarkSet {
  base_symbol: BaseSymbol;
  spot_quote_minor_per_base: MoneyMinor;
  marks: Record<Symbol, Mark>;
}
