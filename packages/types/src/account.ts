/**
 * Account, position, and margin view models — mirrors poc-margin + engine views.
 */

import type { MoneyMinor, SignedMoneyMinor, SubaccountId, Symbol } from "./primitives";

/** A position on a subaccount (poc-margin Position). */
export interface Position {
  symbol: Symbol;
  signed_lots: number;
  avg_entry_quote_minor: MoneyMinor;
  realized_pnl_quote_minor: SignedMoneyMinor;
}

/** Margin summary (poc-margin MarginSummary). */
export interface MarginSummary {
  equity_quote_minor: SignedMoneyMinor;
  maintenance_quote_minor: MoneyMinor;
  initial_quote_minor: MoneyMinor;
  order_margin_quote_minor: MoneyMinor;
}

export function marginFreeEquity(m: MarginSummary): SignedMoneyMinor {
  return m.equity_quote_minor - BigInt.asUintN(64, m.order_margin_quote_minor);
}

export function marginAvailable(m: MarginSummary): SignedMoneyMinor {
  return m.equity_quote_minor - BigInt.asUintN(64, m.initial_quote_minor);
}

/** Account health tri-state. */
export type Health = "healthy" | "restricted" | "liquidation";

/** Greeks (analytics — f64 on the Rust side too). */
export interface GreeksView {
  symbol: Symbol;
  delta: number;
  gamma: number;
  vega: number;
  theta: number;
}

/** Engine `account_view` projection. */
export interface AccountView {
  id: SubaccountId;
  summary: MarginSummary;
  cash_quote_minor: SignedMoneyMinor;
  positions: { symbol: Symbol; signed_lots: number; avg_entry_quote_minor: MoneyMinor }[];
  fees_paid_quote_minor: MoneyMinor;
  funding_received_quote_minor: SignedMoneyMinor;
  open_order_ids: number[];
  health: Health;
}

/** Collateral balance row (multi-collateral, G-17). */
export interface CollateralBalance {
  currency: string;
  balance_minor: bigint;
  /** Haircut value in quote minor (already applied). */
  value_quote_minor: MoneyMinor;
}

/** Collateral currency config. */
export interface CollateralCurrency {
  code: string;
  decimals: number;
  haircut_bps: number;
  price_source: { kind: "oracle"; base_symbol: string } | { kind: "fixed"; quote_minor_per_unit: bigint };
}
