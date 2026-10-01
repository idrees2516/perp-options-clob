/**
 * Protocol subsystems — withdrawals, governance, vaults, proof-of-reserves, RFQ, MMP.
 */

import type {
  MoneyMinor,
  Permille,
  QtyLots,
  Side,
  SignedMoneyMinor,
  SubaccountId,
  Symbol,
  TimestampMs,
} from "./primitives";
import type { Trade } from "./event";

/* ─────────────────────────── Withdrawal pipeline (G-32) ─────────────────────────── */

export type WithdrawalState = "pending" | "approved" | "settled" | "cancelled";

export interface WithdrawalRequest {
  id: number;
  subaccount: SubaccountId;
  amount_quote_minor: MoneyMinor;
  destination: string;
  requested_at: TimestampMs;
  claimable_at: TimestampMs;
  state: WithdrawalState;
  needs_manual: boolean;
}

export interface WithdrawalPolicy {
  settlement_delay_ms: number;
  max_pending_per_account: number;
  per_withdrawal_cap_quote_minor: MoneyMinor;
  daily_quota_quote_minor: MoneyMinor;
  manual_threshold_quote_minor: MoneyMinor;
}

export const WITHDRAWAL_POLICY: WithdrawalPolicy = {
  settlement_delay_ms: 30 * 60 * 1000,
  max_pending_per_account: 5,
  per_withdrawal_cap_quote_minor: 10_000_000_00n * 10_000n / 10_000n, // $10M
  daily_quota_quote_minor: 50_000_000_00n * 10_000n / 10_000n, // $50M
  manual_threshold_quote_minor: 5_000_000_00n * 10_000n / 10_000n, // $5M
};

export type WithdrawalError =
  | "invalid_amount"
  | "insufficient_balance"
  | "too_many_pending"
  | "above_per_withdrawal_cap"
  | "above_daily_quota"
  | "not_pending";

/* ─────────────────────────── Governance (G-33) ─────────────────────────── */

export type ProposalState =
  | "pending_approval"
  | "queued"
  | "executed"
  | "cancelled"
  | "expired";

export interface Proposal {
  id: number;
  /** Parameter key, e.g. "fees.tier0.taker_bps". */
  key: string;
  value: bigint;
  description: string;
  proposer: string;
  state: ProposalState;
  approvals: Record<string, number>; // signer → weight snapshot
  queued_at: TimestampMs | null;
  executed_at: TimestampMs | null;
  created_at: TimestampMs;
}

export interface GovernanceConfig {
  approval_threshold_weight: number;
  timelock_ms: number;
  grace_ms: number;
  guardian: string;
}

export const GOVERNANCE_DEFAULTS: GovernanceConfig = {
  approval_threshold_weight: 3,
  timelock_ms: 24 * 60 * 60 * 1000,
  grace_ms: 72 * 60 * 60 * 1000,
  guardian: "guardian",
};

export type GovernanceAction =
  | { type: "propose"; key: string; value: bigint; description: string; proposer: string }
  | { type: "approve"; proposal_id: number; signer: string }
  | { type: "queue"; proposal_id: number }
  | { type: "execute"; proposal_id: number }
  | { type: "cancel"; proposal_id: number; by: string }
  | { type: "veto"; proposal_id: number; by: string }
  | { type: "sweep_expiries" };

export type GovernanceEvent =
  | { type: "proposed"; id: number }
  | { type: "approved"; id: number; signer: string; weight: number }
  | { type: "queued"; id: number; eta: TimestampMs }
  | { type: "executed"; id: number; key: string; value: bigint }
  | { type: "cancelled"; id: number }
  | { type: "expired"; id: number };

export type GovernanceError =
  | "unknown_signer"
  | "already_approved"
  | "wrong_state"
  | "too_early"
  | "expired"
  | "not_guardian";

/** Signers with weights (weighted multisig). */
export const GOVERNANCE_SIGNERS: Record<string, number> = {
  "alice.ops": 2,
  "bob.risk": 2,
  "carol.treasury": 3,
  "guardian": 0, // veto only
};

/* ─────────────────────────── LP vaults (G-16) ─────────────────────────── */

export interface VaultState {
  vault_id: number;
  revenue_share_bps: number;
  total_shares: bigint;
  nav_quote_minor: MoneyMinor;
  nav_per_share_quote_minor: MoneyMinor;
  /** Pending flows settle at the next epoch. */
  pending_subscriptions_quote_minor: MoneyMinor;
  pending_redemptions_shares: bigint;
  epoch: number;
  last_epoch_ts: TimestampMs;
  /** Lifetime insurance allocation received. */
  lifetime_credit_quote_minor: MoneyMinor;
}

export interface VaultPosition {
  vault_id: number;
  subaccount: SubaccountId;
  shares: bigint;
  value_quote_minor: MoneyMinor;
  last_claim_quote_minor: MoneyMinor;
}

/* ─────────────────────────── Proof of reserves (G-35) ─────────────────────────── */

export interface PorLiabilityRow {
  subaccount: SubaccountId;
  quote_cash_minor: SignedMoneyMinor;
  collateral: { code: string; balance_minor: bigint; value_quote_minor: bigint }[];
  vault_claims_quote_minor: MoneyMinor;
  /** Leaf hash (hex) of this row under the current report. */
  leaf_hash: string;
  /** Sibling path from leaf to root, each { hash, index_bit }. */
  inclusion_proof: { hash: string; right: boolean }[];
}

export interface PorReport {
  nonce: number;
  ts: TimestampMs;
  root: string;
  total_liabilities_quote_minor: MoneyMinor;
  entry_count: number;
  /** Attested reserves (pluggable attestation). */
  attested_reserve_quote_minor: MoneyMinor | null;
  coverage_permille: Permille | null;
}

/* ─────────────────────────── RFQ (G-11/14/38) ─────────────────────────── */

export interface RfqLeg {
  symbol: Symbol;
  side: Side;
  qty_lots: QtyLots;
}

export interface RfqQuoteView {
  quote_id: number;
  rfq_id: number;
  maker: SubaccountId;
  leg_prices_ticks: number[];
  total_cost_quote_minor: SignedMoneyMinor;
  ttl_ms: number;
  expires_at: TimestampMs;
}

export interface RfqView {
  rfq_id: number;
  taker: SubaccountId;
  legs: RfqLeg[];
  counterparties: SubaccountId[];
  min_total_cost_quote_minor: MoneyMinor | null;
  max_total_cost_quote_minor: MoneyMinor | null;
  created_at: TimestampMs;
  expires_at: TimestampMs;
  status: "open" | "quoted" | "executed" | "expired" | "cancelled";
  quotes: RfqQuoteView[];
  executed_quote_id: number | null;
  trades: Trade[] | null;
}

export interface BlockTradeView {
  block_id: number;
  taker: SubaccountId;
  maker: SubaccountId;
  legs: { symbol: Symbol; side: Side; qty_lots: QtyLots; price_ticks: number }[];
  total_notional_quote_minor: MoneyMinor;
  registered_ts: TimestampMs;
  broadcast_ts: TimestampMs;
  printed: boolean;
}

/* ─────────────────────────── MMP (market-maker protection) ─────────────────────────── */

export interface MmpConfig {
  subaccount: SubaccountId;
  base_symbol: string;
  interval_ms: number;
  frozen_time_ms: number;
  amount_limit_lots: QtyLots;
  delta_limit_lots: number;
}

export interface MmpRuntimeState {
  config: MmpConfig | null;
  frozen_until: TimestampMs | null;
  window_fill_lots: number;
  window_net_delta: number;
}

/* ─────────────────────────── Settlement layer (G-30) ─────────────────────────── */

export interface AccountCommitment {
  subaccount: SubaccountId;
  cash_quote_minor: SignedMoneyMinor;
  positions: { symbol: Symbol; signed_lots: number }[];
}

export interface SettlementBatchView {
  sequence: number;
  prev_root: string;
  new_root: string;
  mutations: number;
  custodial_residual: bigint;
  ts: TimestampMs;
  hash: string;
}
