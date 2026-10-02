/**
 * Domain primitives mirroring `poc-core` (crates/core/src/types.rs, instrument.rs, num.rs).
 *
 * Money convention (CRITICAL): every monetary amount is a `bigint` of quote
 * minor units — u128/i128 on the Rust side. `1_00n` == $1.00 at 2 quote
 * decimals. `f64` never touches a ledger; numbers are reserved for
 * analytics (IV, greeks) exactly as in the Rust engine.
 */

/** Milliseconds since the Unix epoch (Rust `TimestampMs`, u64). */
export type TimestampMs = number;

/** Monotonic engine tick counter (Rust `TickstampMs`, u64). */
export type TickstampMs = number;

/** Margin is pooled per subaccount (u64). */
export type SubaccountId = number;

/** Engine-assigned monotonic order id (u64). */
export type OrderId = number;

/** e.g. "BTC-PERP", "BTC-20260327-80000-C" */
export type Symbol = string;

/** Base asset symbol, e.g. "BTC". */
export type BaseSymbol = string;

/** Quote minor units, u128/i128 on the wire — bigint here. */
export type MoneyMinor = bigint;

/** Signed money (PnL, funding flows, fees can be rebates). */
export type SignedMoneyMinor = bigint;

/** Price in integer ticks; quote_minor = ticks × tick_size_quote_minor. */
export type PriceTicks = number;

/** Quantity in integer lots; base_minor = lots × lot_size_base_minor. */
export type QtyLots = number;

/** Reserved system subaccounts (crates/settlement). */
export const RESERVED_SUBACCOUNTS = {
  HOUSE: Number.MAX_SAFE_INTEGER - 4,
  INSURANCE: Number.MAX_SAFE_INTEGER - 3,
  REWARDS: Number.MAX_SAFE_INTEGER - 2,
  BUYBACK: Number.MAX_SAFE_INTEGER - 1,
} as const;

/** Highest user-assignable subaccount id. */
export const MAX_USER_SUBACCOUNT = Number.MAX_SAFE_INTEGER - 8;

/** Order side. */
export type Side = "bid" | "ask";

export const SIDE: Record<"bid" | "ask", "bid" | "ask"> = { bid: "bid", ask: "ask" };

export function opposite(side: Side): Side {
  return side === "bid" ? "ask" : "bid";
}

/** Signed quantity convention: > 0 long, < 0 short. */
export type SignedLots = number;

/** Time-in-force policies. */
export type TimeInForce =
  | { kind: "gtc" }
  | { kind: "ioc" }
  | { kind: "fok" }
  | { kind: "gtd"; until: TimestampMs };

/** Self-trade prevention policies (poc-core STP). */
export type SelfTradePrevention =
  | "cancel_newest"
  | "cancel_oldest"
  | "cancel_both"
  | "decrement_and_cancel";

/** Order types — the full engine surface. */
export type OrderType =
  | { kind: "limit" }
  | { kind: "market" }
  | { kind: "stop_market"; trigger_price: PriceTicks }
  | { kind: "stop_limit"; trigger_price: PriceTicks; limit_price: PriceTicks }
  | { kind: "trailing_stop_market"; offset_ticks: PriceTicks }
  | { kind: "trailing_stop_limit"; offset_ticks: PriceTicks; limit_ticks: PriceTicks };

export type OrderTypeKind = OrderType["kind"];

/** Order lifecycle states (poc-core OrderState). */
export type OrderState =
  | "pending"
  | "open"
  | "filled"
  | "canceled"
  | "rejected"
  | "expired";

/** Why a resting order left the book (OrderCloseReason). */
export type OrderCloseReason =
  | "filled"
  | "canceled"
  | "expired"
  | "ioc_remainder"
  | "oco_sibling";

/** Option kind (calls / puts). */
export type OptionKind = "call" | "put";

/** Dated expiry vs the everlasting (Paradigm roll) variant. */
export type OptionVariant = "dated" | "everlasting";

/** Exercise style (v0.6). */
export type ExerciseStyle = "european" | "american";

/** Oracle price source for a collateral currency. */
export type PriceSource =
  | { kind: "oracle"; base_symbol: BaseSymbol }
  | { kind: "fixed"; quote_minor_per_unit: MoneyMinor };

/** Funding sign convention: rate_bps > 0 ⇒ longs pay shorts. */
export type FundingRateBps = number;

/** Per-mille (×1000) — used for uptime and coverage ratios. */
export type Permille = number;
