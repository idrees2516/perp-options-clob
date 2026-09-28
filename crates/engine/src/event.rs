//! Engine events — the journal every state mutation is recorded in.

use poc_core::{OrderId, Side, SubaccountId, Symbol, TimestampMs};
use poc_margin::MarginSummary;
use poc_risk::Rejection;

use crate::command::OrderRequest;

/// An in-place order amendment that keeps queue priority (G-09).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderAmended {
    /// The amended order id.
    pub order_id: OrderId,
    /// Owner.
    pub subaccount: SubaccountId,
    /// Instrument.
    pub symbol: Symbol,
    /// New total open quantity in lots (≤ the old open quantity).
    pub new_open_lots: u64,
    /// Engine wall-clock.
    pub ts: TimestampMs,
}

/// A TWAP parent order (G-10): the deterministic slicer state.
///
/// Slicing rule: `total_lots` splits into `slices` children; the first
/// `total_lots % slices` children carry one extra lot so the placed sum
/// is exactly `total_lots` with integer lots. Children are marketable
/// limit orders at `limit_ticks` (or pure markets when unbounded) with
/// IOC semantics — the parent never rests on the book itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwapParent {
    /// Engine-assigned parent id.
    pub parent_id: u64,
    /// Owning subaccount.
    pub subaccount: SubaccountId,
    /// Instrument symbol.
    pub symbol: Symbol,
    /// Side of every slice.
    pub side: Side,
    /// Total quantity in lots.
    pub total_lots: u64,
    /// Number of child slices.
    pub slices: u64,
    /// Wall-clock spacing between slices, ms.
    pub slice_interval_ms: TimestampMs,
    /// Worst acceptable price in ticks per slice (`None` = market).
    pub limit_ticks: Option<u64>,
    /// Timestamp of the next child placement.
    pub next_slice_ts: TimestampMs,
    /// Children already emitted.
    pub slices_placed: u64,
    /// Lots already placed into children.
    pub lots_placed: u64,
    /// Placement wall-clock of the opening.
    pub opened_ts: TimestampMs,
}

impl TwapParent {
    /// Lot count of child `index` (0-based).
    #[must_use]
    pub fn slice_lots(&self, index: u64) -> u64 {
        let base = self.total_lots / self.slices.max(1);
        let rem = self.total_lots % self.slices.max(1);
        if index < rem {
            base + 1
        } else {
            base
        }
    }

    /// Whether every child has been emitted.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.slices_placed >= self.slices
    }
}

/// An LP underwriter vault epoch settlement (G-16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultEpoch {
    /// The vault.
    pub vault_id: u64,
    /// Epoch index.
    pub epoch: u64,
    /// NAV per share before this epoch's flows (quote minor).
    pub nav_per_share_quote_minor: u128,
    /// Subscription shares issued this epoch.
    pub subscribed_shares: u128,
    /// Subscription quote minor received.
    pub subscribed_quote_minor: u128,
    /// Redemption shares burned this epoch.
    pub redeemed_shares: u128,
    /// Redemption quote minor paid out.
    pub redeemed_quote_minor: u128,
    /// Share of insurance revenue credited this epoch (quote minor).
    pub insurance_credit_quote_minor: u128,
    /// NAV per share after the epoch (quote minor).
    pub nav_after_quote_minor: u128,
    /// Per-subscriber signed quote flows (subscriptions negative,
    /// redemptions positive).
    pub flows: Vec<(SubaccountId, i128)>,
    /// Engine wall-clock.
    pub ts: TimestampMs,
}

/// A collateral movement (G-17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollateralMoved {
    /// The account.
    pub subaccount: SubaccountId,
    /// Collateral currency code (`"USD"` = quote cash).
    pub currency: String,
    /// Signed amount in the currency's minor units (positive = credited).
    pub amount_minor: i128,
    /// Engine wall-clock.
    pub ts: TimestampMs,
}

/// A currency conversion settled at oracle prices (G-17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollateralConverted {
    /// The converting account.
    pub subaccount: SubaccountId,
    /// Source currency code.
    pub from: String,
    /// Destination currency code.
    pub to: String,
    /// Amount spent, source minor units.
    pub from_amount_minor: u128,
    /// Amount received, destination minor units.
    pub to_amount_minor: u128,
    /// Conversion rate applied: quote minor per `1.0` of source.
    pub rate_quote_minor_per_unit: u128,
    /// Engine wall-clock.
    pub ts: TimestampMs,
}

/// A trade (fill) event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trade {
    /// Monotonic event sequence number.
    pub seq: u64,
    /// Instrument.
    pub symbol: Symbol,
    /// Taker order id.
    pub taker_order_id: OrderId,
    /// Maker order id.
    pub maker_order_id: OrderId,
    /// Taker's subaccount.
    pub taker_subaccount: SubaccountId,
    /// Maker's subaccount.
    pub maker_subaccount: SubaccountId,
    /// Side of the maker (resting order).
    pub maker_side: Side,
    /// Execution price in ticks.
    pub price_ticks: u64,
    /// Execution quantity in lots.
    pub qty_lots: u64,
    /// Notional in quote minor (at execution price).
    pub notional_quote_minor: u128,
    /// Fee paid by the taker (signed, positive = paid).
    pub taker_fee_quote_minor: i128,
    /// Fee paid by/credited to the maker (signed).
    pub maker_fee_quote_minor: i128,
    /// Settlement timestamp.
    pub ts: TimestampMs,
}

/// A rejected order request with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRejected {
    /// The rejected request.
    pub request: OrderRequest,
    /// Why.
    pub reason: Rejection,
    /// Engine-assigned id consumed by the attempt.
    pub order_id: OrderId,
}

/// A funding interval settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingSettled {
    /// Perp instrument.
    pub symbol: Symbol,
    /// Applied rate, signed (positive = longs paid).
    pub rate_bps: i64,
    /// Settlement timestamp.
    pub ts: TimestampMs,
}

/// One account's funding cash flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingPaid {
    /// The account.
    pub subaccount: SubaccountId,
    /// Perp instrument.
    pub symbol: Symbol,
    /// Credit (positive = received), quote minor.
    pub credit_quote_minor: i128,
}

/// One account's option expiry settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionSettled {
    /// The account.
    pub subaccount: SubaccountId,
    /// Option instrument.
    pub symbol: Symbol,
    /// Signed lots at settlement.
    pub signed_lots: i64,
    /// TWAP settlement price (quote minor per base).
    pub settlement_quote_minor: u128,
    /// Payout: intrinsic × signed lots (positive = received).
    pub payout_quote_minor: i128,
}

/// One liquidation closure execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidationExecuted {
    /// Liquidated account.
    pub subaccount: SubaccountId,
    /// Instrument closed.
    pub symbol: Symbol,
    /// Lots closed.
    pub lots: u64,
    /// Execution price (penalized mark for the insurance portion).
    pub price_quote_minor: u128,
    /// True when the insurance fund was the counterparty.
    pub to_insurance: bool,
    /// Penalty component routed to the insurance fund.
    pub penalty_quote_minor: u128,
    /// Shortfall absorbed by the fund (bankruptcy).
    pub absorbed_quote_minor: u128,
    /// Whether the liquidated position was long (closing side = Ask).
    pub closing_side_is_ask: bool,
}

/// Auto-deleveraging execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdlExecuted {
    /// The bankrupt (or insurance-exhausted) side.
    pub liquidated_subaccount: SubaccountId,
    /// The counterparty force-closed to offset it.
    pub counterparty_subaccount: SubaccountId,
    /// Instrument.
    pub symbol: Symbol,
    /// Lots closed on both sides.
    pub lots: u64,
    /// Execution price (mark at ADL time).
    pub price_quote_minor: u128,
    /// Whether the liquidated position was long (closing side = Ask).
    pub closing_side_is_ask: bool,
}

/// A liquidity-reward payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewardPaid {
    /// Rewarded account.
    pub subaccount: SubaccountId,
    /// Amount in quote minor.
    pub amount_quote_minor: u128,
}

/// One liquidity-scoring observation fed to the incentive engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiquidityObservation {
    /// The quoting account.
    pub subaccount: SubaccountId,
    /// Resting size within the scoring band, lots.
    pub size_lots: u64,
    /// Distance of the quote from mid, bps.
    pub spread_bps: u64,
    /// Whether the account quotes both sides simultaneously.
    pub two_sided: bool,
    /// The quote's side (G-15: tier obligations are evaluated per
    /// side — the worst side decides).
    pub side: Side,
}

/// The full engine journal. Every variant is applied via
/// [`crate::engine::Engine::apply_event`] — and nothing else mutates state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Cash credited.
    Deposit {
        /// Target account.
        subaccount: SubaccountId,
        /// Amount, quote minor.
        amount_quote_minor: u128,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// Cash debited.
    Withdrawal {
        /// Source account.
        subaccount: SubaccountId,
        /// Amount, quote minor.
        amount_quote_minor: u128,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// Withdrawal denied (no state change; journal for clients).
    WithdrawRejected {
        /// Source account.
        subaccount: SubaccountId,
        /// Requested amount.
        requested: u128,
        /// Why.
        reason: &'static str,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// Provider observation pushed to an oracle (recorded before any
    /// mark effects, which are detected by the following `Tick`).
    ProviderObserved {
        /// Underlying.
        base_symbol: String,
        /// Provider name.
        provider: String,
        /// Observation time.
        ts: TimestampMs,
        /// Observed price, quote minor per base.
        price_quote_minor: u128,
    },
    /// An instrument was listed (books and oracles created).
    MarketListed {
        /// The registered instrument.
        instrument: poc_core::Instrument,
        /// Surface anchor IV override for auto-listed markets (G-34):
        /// bps; `None` falls back to `option_ivs` / the 55% default.
        anchor_iv_bps: Option<u64>,
    },
    /// Engine clock advanced by a tick.
    ClockAdvanced {
        /// New engine time.
        now: TimestampMs,
    },
    /// An order rests on the book (possibly with partial fills already).
    OrderResting {
        /// The resting order (with fill state).
        order: poc_core::Order,
        /// Order margin reserved while this order rests.
        margin_reserved_quote_minor: u128,
    },
    /// An order was fully filled or removed by matching.
    OrderClosed {
        /// Order id.
        order_id: OrderId,
        /// Owner.
        subaccount: SubaccountId,
        /// Instrument.
        symbol: Symbol,
        /// Final fill state of the order.
        order: poc_core::Order,
        /// Why it left the book.
        reason: OrderCloseReason,
    },
    /// A pre-trade rejection.
    OrderRejection(Box<OrderRejected>),
    /// A trade.
    TradeExecuted(Box<Trade>),
    /// Self-trade prevention canceled resting makers.
    StpCancels {
        /// Canceled maker order ids.
        maker_ids: Vec<OrderId>,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// Funding interval settled for a perp.
    Funding(Box<FundingSettled>),
    /// One account's funding flow.
    FundingFlow(Box<FundingPaid>),
    /// One account's option settlement.
    OptionExpiry(Box<OptionSettled>),
    /// An option instrument settled and delisted.
    OptionDelisted {
        /// The instrument symbol.
        symbol: Symbol,
    },
    /// Liquidity scores accumulated for the current reward interval.
    LiquidityScored {
        /// The observations.
        observations: Vec<LiquidityObservation>,
    },
    /// Reward payments for a completed interval.
    Reward(Box<RewardPaid>),
    /// The reward interval closed (scores reset, pool carried).
    RewardsSettled,
    /// A liquidation closure.
    Liquidation(Box<LiquidationExecuted>),
    /// An ADL execution.
    Adl(Box<AdlExecuted>),
    /// An underlying halted (oracle quorum lost).
    MarketHalted {
        /// Underlying.
        base_symbol: String,
        /// Time of the halt.
        ts: TimestampMs,
    },
    /// An underlying resumed.
    MarketResumed {
        /// Underlying.
        base_symbol: String,
        /// Time of the resume.
        ts: TimestampMs,
    },
    /// An RFQ was published (unsigned intent; id assigned at apply).
    RfqCreated {
        /// Requesting taker.
        taker: SubaccountId,
        /// The priced package legs.
        legs: Vec<poc_rfq::RfqLeg>,
        /// Private direction (empty = open to all makers).
        counterparties: Vec<SubaccountId>,
        /// Cost bounds.
        min_total_cost_quote_minor: Option<u128>,
        /// Upper cost bound.
        max_total_cost_quote_minor: Option<u128>,
        /// Quoting window.
        ttl_ms: TimestampMs,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// A maker quote landed on an RFQ (id assigned at apply).
    RfqQuoted {
        /// Target RFQ.
        rfq_id: u64,
        /// Quoting maker.
        maker: SubaccountId,
        /// Per-leg prices, ticks.
        leg_prices_ticks: Vec<u64>,
        /// Quote window.
        ttl_ms: TimestampMs,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// An RFQ action was rejected at the plan gate.
    RfqRejected {
        /// The requesting account.
        subaccount: SubaccountId,
        /// Why.
        reason: &'static str,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// An RFQ execution settled as venue trades (fees + margin applied).
    RfqSettled {
        /// The RFQ.
        rfq_id: u64,
        /// The executed quote.
        quote_id: u64,
        /// The executing taker.
        taker: SubaccountId,
        /// The quoting maker.
        maker: SubaccountId,
        /// Per-leg trades (synthetic order ids; prices in ticks).
        trades: Vec<Trade>,
        /// Taker fees per leg (maker pays zero — Paradigm economics).
        taker_fees_quote_minor: Vec<i128>,
    },
    /// An RFQ or quote was cancelled / expired.
    RfqClosed {
        /// The RFQ id (0 = quote-only action).
        rfq_id: u64,
        /// The quote id when a single quote was acted on.
        quote_id: Option<u64>,
        /// Why (`cancelled` | `expired` | `filled`).
        reason: &'static str,
    },
    /// Cancel-on-disconnect setting changed.
    CodChanged {
        /// Subaccount.
        subaccount: SubaccountId,
        /// The new setting.
        enabled: bool,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// A block trade registered (private until broadcast; id at apply).
    BlockRegistered {
        /// The taker-side counterparty.
        taker: SubaccountId,
        /// The maker-side counterparty.
        maker: SubaccountId,
        /// Legs (symbol, taker side, qty, price ticks).
        legs: Vec<(Symbol, Side, u64, u64)>,
        /// Package notional, quote minor.
        total_notional_quote_minor: u128,
        /// Taker fees per leg (the block fee rail; maker pays zero).
        taker_fees_quote_minor: Vec<i128>,
        /// Public print time.
        broadcast_ts: TimestampMs,
    },
    /// A block printed to the public tape after its delay.
    BlockPrinted {
        /// The block id.
        block_id: u64,
    },
    /// An internal transfer settled.
    TransferExecuted {
        /// Source.
        from: SubaccountId,
        /// Destination.
        to: SubaccountId,
        /// Amount, quote minor.
        amount_quote_minor: u128,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// An internal transfer was denied.
    TransferRejected {
        /// Source.
        from: SubaccountId,
        /// Destination.
        to: SubaccountId,
        /// Requested amount.
        requested: u128,
        /// Why.
        reason: &'static str,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// MMP configuration recorded.
    MmpConfigured {
        /// Subaccount.
        subaccount: SubaccountId,
        /// Underlying.
        base_symbol: String,
        /// Rolling window (ms).
        interval_ms: TimestampMs,
        /// Freeze duration (ms; 0 = manual reset only).
        frozen_time_ms: TimestampMs,
        /// Amount limit (lots).
        amount_limit_lots: u64,
        /// Delta limit (lots).
        delta_limit_lots: u64,
    },
    /// MMP tripped: resting orders cancelled, trading frozen for the window.
    MmpTripped {
        /// Subaccount.
        subaccount: SubaccountId,
        /// Underlying.
        base_symbol: String,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// Cancel-on-disconnect executed for a dropped session.
    SessionDisconnected {
        /// Subaccount.
        subaccount: SubaccountId,
        /// Resting orders pulled.
        canceled_orders: Vec<poc_core::OrderId>,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// One volatility-surface observation (book touch -> blend input).
    SurfaceObserved(Box<SurfaceObservation>),
    /// The governance sweep ran (move clamps + staleness fallback).
    SurfaceSwept {
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// A circuit breaker tripped (price dislocation / cascade velocity).
    BreakerTripped {
        /// Which breaker (`price-dislocation` | `cascade-velocity`).
        kind: &'static str,
        /// Instrument (price breaker) or empty (velocity breaker).
        symbol: Symbol,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// A circuit breaker released after its cooldown.
    BreakerReleased {
        /// Which breaker.
        kind: &'static str,
        /// Instrument.
        symbol: Symbol,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// An in-place amendment kept the order's queue priority (G-09).
    OrderAmended(Box<OrderAmended>),
    /// A trailing stop's running extreme moved (G-07). Journaled so the
    /// parked order's trigger state replays exactly.
    TrailingUpdated {
        /// The parked order.
        order_id: OrderId,
        /// Owner.
        subaccount: SubaccountId,
        /// Instrument.
        symbol: Symbol,
        /// New running extreme (quote minor per base).
        extreme_quote_minor: u128,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// An auction opened on one instrument (G-12): orders accumulate
    /// without matching until `uncross_at`.
    AuctionOpened {
        /// Instrument.
        symbol: Symbol,
        /// Uncross deadline.
        uncross_at: TimestampMs,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// An auction uncrossed at a uniform clearing price (G-12). The
    /// embedded trades are also journaled individually as
    /// [`Event::TradeExecuted`]; this event additionally reduces the
    /// resting "takers" and restores continuous matching.
    AuctionUncrossed {
        /// Instrument.
        symbol: Symbol,
        /// Uniform clearing price in ticks (`None` when the book never
        /// crossed — the auction simply ends).
        clearing_price_ticks: Option<u64>,
        /// Total matched quantity, lots.
        matched_lots: u64,
        /// (taker order id, lots) reductions to apply to resting takers.
        taker_reductions: Vec<(OrderId, u64)>,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// Collateral credited or debited (G-17).
    CollateralMoved(Box<CollateralMoved>),
    /// A collateral request was denied (G-17).
    CollateralRejected {
        /// The account.
        subaccount: SubaccountId,
        /// Currency code.
        currency: String,
        /// Requested amount, minor units.
        requested_minor: u128,
        /// Why.
        reason: &'static str,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// A currency conversion settled at oracle prices (G-17).
    CollateralConversion(Box<CollateralConverted>),
    /// An everlasting option's strike rebased (G-03): every position was
    /// closed at the old market's mark and reopened at the new strike's
    /// mark — the Everstrike strike-reset that keeps contracts near the
    /// money. Resting orders on the old market are cancelled and the old
    /// market delists.
    PositionMigrated {
        /// The migrating account.
        subaccount: SubaccountId,
        /// Old instrument.
        from_symbol: Symbol,
        /// New instrument.
        to_symbol: Symbol,
        /// Signed lots carried over.
        signed_lots: i64,
        /// Mark the old position was closed at (quote minor per base).
        close_price_quote_minor: u128,
        /// Mark the new position was opened at (quote minor per base).
        open_price_quote_minor: u128,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// An OCO pair was linked (G-08): the engine assigned a group id to
    /// two orders; the first terminal sibling cancels the other.
    OcoLinked {
        /// The group.
        group: u64,
        /// First sibling's order id.
        first: OrderId,
        /// Second sibling's order id.
        second: OrderId,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// A TWAP parent opened (G-10): the slicer state that emits child
    /// placements every `slice_interval_ms`.
    TwapOpened(Box<TwapParent>),
    /// A TWAP parent emitted one child placement (G-10). The embedded
    /// request is journaled in full so replay reproduces the child
    /// without re-deriving the slice arithmetic.
    TwapSliced {
        /// The parent.
        parent_id: u64,
        /// The child placement request.
        request: crate::command::OrderRequest,
        /// Slice index (0-based) of this child.
        slice_index: u64,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// A TWAP parent finished (all slices placed) or was canceled (G-10).
    TwapClosed {
        /// The parent.
        parent_id: u64,
        /// Owning subaccount.
        subaccount: SubaccountId,
        /// Why the parent closed: `completed` or `canceled`.
        reason: &'static str,
        /// Total lots placed by the parent.
        placed_lots: u64,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// The 30-day volatility index was published (G-05, DVOL-shaped):
    /// 100 x sqrt(annualized 30-day fair variance off the live surface).
    VolIndexPublished {
        /// Underlying.
        base_symbol: String,
        /// Index level, scaled by 1000 (three decimals).
        index_permille: u64,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// Interest accrued on utilized non-quote collateral (G-18): the
    /// in-kind charge for borrowing margin capacity in a foreign
    /// currency, credited to the house.
    CollateralInterestAccrued {
        /// The account.
        subaccount: SubaccountId,
        /// Currency code.
        currency: String,
        /// Minor units debited from the account's balance.
        amount_minor: u128,
        /// Quote value of the charge (at the accrual price).
        quote_value_minor: u128,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// The insurance fund marked its inventory to the current marks
    /// (G-23): position PnL since the last mark lands in the fund balance.
    InsuranceMarked {
        /// Instrument.
        symbol: Symbol,
        /// Signed lots carried.
        signed_lots: i64,
        /// Mark used (quote minor per base).
        mark_quote_minor: u128,
        /// PnL applied to the fund (positive = gain).
        pnl_quote_minor: i128,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// The insurance fund rebalanced inventory back into the book
    /// (G-23): the drips are journaled as ordinary trades; this event
    /// updates fund inventory and completes the audit trail.
    InsuranceRebalanced {
        /// Instrument.
        symbol: Symbol,
        /// Lots sold back into the book.
        lots: u64,
        /// Proceeds (quote minor) added to the fund.
        proceeds_quote_minor: u128,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// An LP underwriter vault epoch settled (G-16): subscriptions and
    /// redemptions processed at the epoch NAV, and the vault's share of
    /// insurance revenue was credited.
    VaultEpochSettled(Box<VaultEpoch>),
    /// A vault was opened (G-16).
    VaultOpened {
        /// The vault.
        vault_id: u64,
        /// Share of the insurance revenue allocation routed here, bps.
        revenue_share_bps: u64,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// A subscription or redemption was queued for the next vault epoch
    /// (G-16). The cash movement itself happens only at the boundary.
    VaultQueued {
        /// The vault.
        vault_id: u64,
        /// The account.
        subaccount: SubaccountId,
        /// `true` = subscription, `false` = redemption.
        is_subscribe: bool,
        /// Quote minor (subscribe) or shares (redeem).
        amount: u128,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// A subaccount enrolled in the market-maker tier program (G-15).
    /// Enrollment persists across review windows; withdrawal is an
    /// explicit venue action.
    MmEnrolled {
        /// The enrolled maker.
        subaccount: SubaccountId,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// One monthly market-maker tier review outcome (G-15): the tier the
    /// trailing window earned, the discount it activates, and the
    /// measured uptime that justified it.
    MmTierAdjusted {
        /// The reviewed maker.
        subaccount: SubaccountId,
        /// The tier name earned (`None` = demoted to standard fees).
        tier: Option<&'static str>,
        /// Fee discount active after this review, bps.
        fee_discount_bps: u64,
        /// Measured overall two-sided presence uptime, permille.
        uptime_permille: u64,
        /// Sampled ticks in the window.
        ticks: u64,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
    /// Quote-balance interest accrued on *utilized* quote margin at a
    /// UTC day boundary (G-18 completion): the charge for margin
    /// capacity borrowed in the quote currency, routed through the
    /// revenue router like every other fee income.
    QuoteInterestAccrued {
        /// The charged account.
        subaccount: SubaccountId,
        /// Interest charged, quote minor.
        amount_quote_minor: u128,
        /// Engine wall-clock.
        ts: TimestampMs,
    },
}

/// Why an order left the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderCloseReason {
    /// Fully filled.
    Filled,
    /// Canceled by user or risk engine.
    Canceled,
    /// Expired (GTD sweep).
    Expired,
    /// Cancelled remainder of an IOC order.
    IocRemainder,
    /// Cancelled because its OCO sibling filled completely or triggered
    /// (G-08).
    OcoSibling,
}

/// Read-only view of one account for API surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountView {
    /// Subaccount id.
    pub id: SubaccountId,
    /// Current margin summary.
    pub summary: MarginSummary,
    /// Cash balance.
    pub cash_quote_minor: i128,
    /// Positions (symbol, signed lots, avg entry).
    pub positions: Vec<(Symbol, i64, u128)>,
    /// Lifetime fees paid.
    pub fees_paid_quote_minor: u128,
    /// Lifetime net funding received (positive = received).
    pub funding_received_quote_minor: i128,
    /// Resting order ids.
    pub open_order_ids: Vec<OrderId>,
}

/// One governed-surface book observation (G-04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceObservation {
    /// Option market.
    pub symbol: Symbol,
    /// Oracle spot, quote minor per base.
    pub spot_quote_minor: u128,
    /// Strike, quote minor per base.
    pub strike_quote_minor: u128,
    /// Call side.
    pub is_call: bool,
    /// Effective time to expiry, ms.
    pub tte_ms: u128,
    /// Best bid premium, quote minor per base.
    pub bid_quote_minor: u128,
    /// Best ask premium, quote minor per base.
    pub ask_quote_minor: u128,
    /// Resting size at best bid, lots.
    pub bid_lots: u64,
    /// Resting size at best ask, lots.
    pub ask_lots: u64,
    /// Engine wall-clock.
    pub ts: TimestampMs,
}

/// Top-of-book + halt state of one instrument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookView {
    /// Instrument symbol.
    pub symbol: Symbol,
    /// Best bid in ticks.
    pub best_bid_ticks: Option<u64>,
    /// Best ask in ticks.
    pub best_ask_ticks: Option<u64>,
    /// Number of resting orders.
    pub open_orders: usize,
    /// Whether the underlying is halted.
    pub halted: bool,
    /// Order type of the instrument (for API discrimination).
    pub kind: InstrumentKindView,
}

/// Instrument family for views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrumentKindView {
    /// Linear perpetual.
    Perp,
    /// European option.
    Option,
}

/// Aggregate market state for snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketStateView {
    /// Per-instrument book views.
    pub books: Vec<BookView>,
    /// Oracle spot per underlying (quote minor per base).
    pub spots: Vec<(String, Option<u128>)>,
    /// Insurance fund balance.
    pub insurance_balance: i128,
}
