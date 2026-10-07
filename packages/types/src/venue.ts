/**
 * Venue view-models — the aggregated snapshot the UI consumes.
 * The sim engine / remote gateway both produce this shape.
 */

import type {
  AccountView,
  CollateralBalance,
  GreeksView,
  Health,
  MarginSummary,
  MoneyMinor,
  SignedMoneyMinor,
  SubaccountId,
  Symbol,
} from "./index";
import type { Instrument } from "./instrument";
import type { BookView, OracleState, TradePrint } from "./market";
import type { EngineStats, FundingRecord, IncentiveParams, IncentiveScoreRow, MmWindowState } from "./economics";
import type {
  BlockTradeView,
  MmpRuntimeState,
  PorLiabilityRow,
  PorReport,
  Proposal,
  RfqView,
  VaultPosition,
  VaultState,
  WithdrawalRequest,
} from "./protocol";
import type { JournalEntry } from "./event";

/** One row in the Markets table. */
export interface MarketRow {
  symbol: Symbol;
  kind: "perp" | "option";
  base_symbol: string;
  /** Display label, e.g. "BTC-PERP" or "27 Mar 80,000 C". */
  label: string;
  mark_quote_minor: MoneyMinor | null;
  best_bid_ticks: number | null;
  best_ask_ticks: number | null;
  spread_ticks: number | null;
  /** 24h volume in quote minor (rolling sim window). */
  volume_quote_minor: MoneyMinor;
  /** Session open price for change computation. */
  open_quote_minor: MoneyMinor | null;
  iv: number | null;
  funding_rate_bps: number | null;
  expired: boolean;
  halted: boolean;
  variant: "dated" | "everlasting" | null;
  exercise_style: "european" | "american" | null;
  strike_quote_minor: MoneyMinor | null;
  expiry_ts: number | null;
}

/** Position row with live marks. */
export interface PositionRow {
  symbol: Symbol;
  kind: "perp" | "option";
  signed_lots: number;
  avg_entry_quote_minor: MoneyMinor;
  mark_quote_minor_per_base: MoneyMinor | null;
  unrealized_pnl_quote_minor: SignedMoneyMinor;
  realized_pnl_quote_minor: SignedMoneyMinor;
  notional_quote_minor: MoneyMinor;
}

/** Open order row for the terminal. */
export interface OpenOrderRow {
  order_id: number;
  symbol: Symbol;
  side: "bid" | "ask";
  order_type_label: string;
  price_ticks: number | null;
  qty_lots: number;
  filled_lots: number;
  open_lots: number;
  tif_label: string;
  post_only: boolean;
  reduce_only: boolean;
  ts: number;
  oco_group: number | null;
}

/** Fill row (maker or taker side of a Trade). */
export interface FillRow extends TradePrint {
  subaccount: SubaccountId;
  role: "taker" | "maker";
  fee_quote_minor: SignedMoneyMinor;
  liquidity_label: "T" | "M";
}

/** Demo account descriptor. */
export interface AccountDescriptor {
  id: SubaccountId;
  name: string;
  role: string;
  initial_deposit_quote_minor: MoneyMinor;
  color: string;
}

/** Venue meta: sim clock, speed, phase. */
export interface VenueMeta {
  /** Sim wall clock (engine time). */
  now: number;
  /** Speed multiplier (0 = paused). */
  speed: number;
  running: boolean;
  seed: number;
  /** Session phase label from the scripted demo. */
  phase: string;
  started_at: number;
  ticks: number;
  connected: boolean;
  transport: "sim-worker" | "remote";
}

/** Insurance fund + risk state. */
export interface InsuranceState {
  balance_quote_minor: SignedMoneyMinor;
  inventory: { symbol: Symbol; signed_lots: number; mark_quote_minor: MoneyMinor }[];
  coverage_permille: number;
  liquidations: number;
  adls: number;
  total_absorbed_quote_minor: MoneyMinor;
}

/** Circuit breaker state per symbol. */
export interface BreakerState {
  symbol: Symbol;
  kind: "price-dislocation" | "cascade-velocity";
  tripped_at: number;
  cooldown_until: number;
}

/** TWAP parent row. */
export interface TwapRow {
  parent_id: number;
  symbol: Symbol;
  side: "bid" | "ask";
  total_lots: number;
  slices: number;
  slices_placed: number;
  lots_placed: number;
  next_slice_ts: number;
  state: "running" | "completed" | "canceled";
}

/** The full venue snapshot — bootstrap + manual refresh. */
export interface VenueSnapshot {
  meta: VenueMeta;
  instruments: Instrument[];
  markets: MarketRow[];
  accounts: AccountView[];
  accountDescriptors: AccountDescriptor[];
  positions: Record<SubaccountId, PositionRow[]>;
  greeks: Record<SubaccountId, GreeksView[]>;
  collateral: Record<SubaccountId, CollateralBalance[]>;
  openOrders: Record<SubaccountId, OpenOrderRow[]>;
  fills: FillRow[];
  stats: EngineStats;
  fundingHistory: FundingRecord[];
  oracle: Record<string, OracleState>;
  vaults: VaultState[];
  vaultPositions: Record<SubaccountId, VaultPosition[]>;
  rfqs: RfqView[];
  blocks: BlockTradeView[];
  withdrawals: WithdrawalRequest[];
  proposals: Proposal[];
  mmStates: MmWindowState[];
  incentiveScoreboard: IncentiveScoreRow[];
  incentiveParams: IncentiveParams;
  insurance: InsuranceState;
  breakers: BreakerState[];
  mmp: Record<SubaccountId, MmpRuntimeState>;
  twaps: TwapRow[];
  por: { report: PorReport | null; rows: PorLiabilityRow[] };
  marginSummaries: Record<SubaccountId, MarginSummary>;
  healths: Record<SubaccountId, Health>;
}
