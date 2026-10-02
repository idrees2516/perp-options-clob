/**
 * Economics — fee ladder, revenue router, MM tiers, rewards, funding history.
 * Exact parameters from poc-economics (mainnet defaults).
 */

import type { MoneyMinor, Permille, SignedMoneyMinor, SubaccountId, Symbol, TimestampMs } from "./primitives";

/** Volume fee tier (30-day trailing notional). */
export interface FeeTier {
  name: string;
  /** 30-day volume threshold in quote minor units. */
  volume_threshold_quote_minor: MoneyMinor;
  maker_bps: number;
  taker_bps: number;
}

/** `FeeSchedule::mainnet()` — takers 4.5bps→0.9bps… base 1/4 by original ladder: */
export const FEE_TIERS: readonly FeeTier[] = [
  { name: "Base", volume_threshold_quote_minor: 0n, maker_bps: 1, taker_bps: 4 },
  { name: "VIP-1", volume_threshold_quote_minor: 1_000_000_00n, maker_bps: 1, taker_bps: 3 },
  { name: "VIP-2", volume_threshold_quote_minor: 10_000_000_00n, maker_bps: 0, taker_bps: 2 },
  { name: "VIP-3", volume_threshold_quote_minor: 100_000_000_00n, maker_bps: 0, taker_bps: 1 },
  { name: "VIP-4", volume_threshold_quote_minor: 1_000_000_000_00n, maker_bps: -1, taker_bps: 1 },
] as const;

/** Option premium fee caps (F-2): min(rate × notional, cap × premium). */
export const OPTION_FEE_CAPS = {
  taker_cap_bps: 1250, // 12.5% of premium
  maker_cap_bps: 250, // 2.5% of premium
} as const;

/** Revenue router split (bps, sums to 10_000). */
export interface RevenueSplit {
  house_bps: number;
  insurance_bps: number;
  buyback_bps: number;
}

export const REVENUE_SPLIT: RevenueSplit = { house_bps: 6000, insurance_bps: 3000, buyback_bps: 1000 };

/** Live routing accumulator. */
export interface RevenueLedger {
  house_quote_minor: MoneyMinor;
  insurance_quote_minor: MoneyMinor;
  buyback_quote_minor: MoneyMinor;
  /** Overflow above coverage target goes to buyback. */
  insurance_overflow_quote_minor: MoneyMinor;
}

/** MM tier program (mainnet). */
export interface MmTierSpec {
  tier: string;
  min_uptime_permille: Permille;
  max_worst_side_spread_bps: number;
  min_smaller_side_lots: number;
  fee_discount_bps: number;
}

export const MM_TIERS: readonly MmTierSpec[] = [
  { tier: "MM-1", min_uptime_permille: 980, max_worst_side_spread_bps: 50, min_smaller_side_lots: 5, fee_discount_bps: 2000 },
  { tier: "MM-2", min_uptime_permille: 950, max_worst_side_spread_bps: 100, min_smaller_side_lots: 3, fee_discount_bps: 1200 },
  { tier: "MM-3", min_uptime_permille: 900, max_worst_side_spread_bps: 250, min_smaller_side_lots: 1, fee_discount_bps: 600 },
] as const;

export const MM_MAX_DISCOUNT_BPS = 5000; // 50% bound

/** One maker's rolling measurement window. */
export interface MmWindowState {
  subaccount: SubaccountId;
  enrolled: boolean;
  tier: string | null;
  fee_discount_bps: number;
  uptime_permille: Permille;
  samples: number;
  two_sided_samples: number;
  last_review_ts: TimestampMs;
}

/** Incentive scoreboard row (budgeted liquidity pool, two-sided tight quoting). */
export interface IncentiveScoreRow {
  subaccount: SubaccountId;
  score: number;
  reward_quote_minor: MoneyMinor;
  quoted_ticks: number;
}

export interface IncentiveParams {
  max_spread_bps: number;
  min_size_lots: number;
  two_sided_multiplier: number;
  budget_quote_minor_per_hour: MoneyMinor;
}

/** Funding interval record for the funding page. */
export interface FundingRecord {
  symbol: Symbol;
  ts: TimestampMs;
  rate_bps: number;
  premium_bps: number;
  interest_bps: number;
  /** markTWAP − indexTWAP in bps (clamped contribution). */
  twap_mark_quote_minor: MoneyMinor;
  twap_index_quote_minor: MoneyMinor;
}

/** Per-subaccount trailing 30d volume + fees. */
export interface FeeAccounting {
  subaccount: SubaccountId;
  trailing_30d_volume_quote_minor: MoneyMinor;
  tier_index: number;
  maker_fees_paid_quote_minor: MoneyMinor;
  taker_fees_paid_quote_minor: MoneyMinor;
  maker_rebates_quote_minor: MoneyMinor;
}

/** Engine stats (Rust `EngineStats`). */
export interface EngineStats {
  events: number;
  trades: number;
  lots_traded: number;
  notional_traded_quote_minor: MoneyMinor;
  revenue: RevenueLedger;
  insurance_balance: SignedMoneyMinor;
  funding_intervals: number;
  options_settled: number;
  exercises_settled: number;
  liquidations: number;
  adls: number;
}
