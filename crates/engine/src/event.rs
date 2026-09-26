//! Engine events — the journal every state mutation is recorded in.

use poc_core::{OrderId, Side, SubaccountId, Symbol, TimestampMs};
use poc_margin::MarginSummary;
use poc_risk::Rejection;

use crate::command::OrderRequest;

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
        /// Taker / maker.
        taker: SubaccountId,
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
        /// Counterparties.
        taker: SubaccountId,
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
