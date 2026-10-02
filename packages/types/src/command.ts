/**
 * Engine commands — all variants of `Command` (crates/engine/src/command.rs).
 * Commands are discriminated unions with a `type` tag for the wire.
 */

import type { MoneyMinor, QtyLots, Side, SubaccountId, Symbol, TimestampMs } from "./primitives";
import type { OrderRequest, RfqLegCommand } from "./order";

export type Command =
  | { type: "deposit"; subaccount: SubaccountId; amount_quote_minor: MoneyMinor }
  | { type: "withdraw"; subaccount: SubaccountId; amount_quote_minor: MoneyMinor }
  | { type: "place"; request: OrderRequest; now: TimestampMs }
  | { type: "cancel"; subaccount: SubaccountId; order_id: number; now: TimestampMs }
  | { type: "cancel_all"; subaccount: SubaccountId; symbol: Symbol | null; now: TimestampMs }
  | {
      type: "oracle_update";
      base_symbol: string;
      provider: string;
      ts: TimestampMs;
      price_quote_minor: MoneyMinor;
    }
  | { type: "tick"; now: TimestampMs }
  | {
      type: "rfq_create";
      taker: SubaccountId;
      legs: RfqLegCommand[];
      counterparties: SubaccountId[];
      min_total_cost_quote_minor: MoneyMinor | null;
      max_total_cost_quote_minor: MoneyMinor | null;
      ttl_ms: number;
      now: TimestampMs;
    }
  | {
      type: "rfq_quote";
      maker: SubaccountId;
      rfq_id: number;
      leg_prices_ticks: number[];
      ttl_ms: number;
      now: TimestampMs;
    }
  | { type: "rfq_execute"; taker: SubaccountId; rfq_id: number; quote_id: number; now: TimestampMs }
  | {
      type: "rfq_cancel";
      subaccount: SubaccountId;
      rfq_id: number | null;
      quote_id: number | null;
      now: TimestampMs;
    }
  | {
      type: "block_trade";
      taker: SubaccountId;
      maker: SubaccountId;
      legs: { symbol: Symbol; side: Side; qty_lots: QtyLots; price_ticks: number }[];
      now: TimestampMs;
    }
  | { type: "transfer"; from: SubaccountId; to: SubaccountId; amount_quote_minor: MoneyMinor; now: TimestampMs }
  | { type: "exercise"; subaccount: SubaccountId; symbol: Symbol; lots: QtyLots; now: TimestampMs }
  | {
      type: "set_mmp";
      subaccount: SubaccountId;
      base_symbol: string;
      interval_ms: number;
      frozen_time_ms: number;
      amount_limit_lots: QtyLots;
      delta_limit_lots: QtyLots;
      now: TimestampMs;
    }
  | { type: "set_cod"; subaccount: SubaccountId; enabled: boolean; now: TimestampMs }
  | { type: "session_dropped"; subaccount: SubaccountId; now: TimestampMs }
  | { type: "place_batch"; requests: OrderRequest[]; now: TimestampMs }
  | { type: "cancel_batch"; subaccount: SubaccountId; order_ids: number[]; now: TimestampMs }
  | {
      type: "amend";
      subaccount: SubaccountId;
      order_id: number;
      new_price_ticks: number | null;
      new_open_lots: QtyLots | null;
      now: TimestampMs;
    }
  | { type: "begin_auction"; symbol: Symbol; uncross_at: TimestampMs; now: TimestampMs }
  | {
      type: "deposit_collateral";
      subaccount: SubaccountId;
      currency: string;
      amount_minor: MoneyMinor;
      now: TimestampMs;
    }
  | {
      type: "withdraw_collateral";
      subaccount: SubaccountId;
      currency: string;
      amount_minor: MoneyMinor;
      now: TimestampMs;
    }
  | {
      type: "convert_collateral";
      subaccount: SubaccountId;
      from: string;
      to: string;
      from_amount_minor: MoneyMinor;
      now: TimestampMs;
    }
  | { type: "place_oco"; first: OrderRequest; second: OrderRequest; now: TimestampMs }
  | {
      type: "place_twap";
      subaccount: SubaccountId;
      symbol: Symbol;
      side: Side;
      total_lots: QtyLots;
      slices: number;
      slice_interval_ms: number;
      limit_ticks: number | null;
      now: TimestampMs;
    }
  | { type: "cancel_twap"; subaccount: SubaccountId; parent_id: number; now: TimestampMs }
  | { type: "vault_create"; revenue_share_bps: number; now: TimestampMs }
  | {
      type: "vault_subscribe";
      vault_id: number;
      subaccount: SubaccountId;
      amount_quote_minor: MoneyMinor;
      now: TimestampMs;
    }
  | {
      type: "vault_redeem";
      vault_id: number;
      subaccount: SubaccountId;
      shares: bigint;
      now: TimestampMs;
    }
  | { type: "mm_tier_enroll"; subaccount: SubaccountId; now: TimestampMs };

export type CommandType = Command["type"];

/** Every command type as a const list (console composer iterates this). */
export const COMMAND_TYPES = [
  "deposit",
  "withdraw",
  "place",
  "cancel",
  "cancel_all",
  "oracle_update",
  "tick",
  "rfq_create",
  "rfq_quote",
  "rfq_execute",
  "rfq_cancel",
  "block_trade",
  "transfer",
  "exercise",
  "set_mmp",
  "set_cod",
  "session_dropped",
  "place_batch",
  "cancel_batch",
  "amend",
  "begin_auction",
  "deposit_collateral",
  "withdraw_collateral",
  "convert_collateral",
  "place_oco",
  "place_twap",
  "cancel_twap",
  "vault_create",
  "vault_subscribe",
  "vault_redeem",
  "mm_tier_enroll",
] as const satisfies readonly CommandType[];

/** Governance actions live in their own crate — mirrored separately in protocol.ts. */
