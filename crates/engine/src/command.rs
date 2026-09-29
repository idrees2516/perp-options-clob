//! Engine commands — the only inputs the sequencer accepts.

use poc_core::{
    OrderType, SelfTradePrevention, Side, SubaccountId, Symbol, TimeInForce, TimestampMs,
};

/// A validated order request from a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRequest {
    /// Owning subaccount.
    pub subaccount: SubaccountId,
    /// Instrument symbol.
    pub symbol: Symbol,
    /// Side of the book.
    pub side: Side,
    /// Limit / market / stop variants.
    pub order_type: OrderType,
    /// Limit price in ticks (`None` for pure market orders).
    pub price_ticks: Option<u64>,
    /// Total quantity in lots.
    pub qty_lots: u64,
    /// Time-in-force.
    pub tif: TimeInForce,
    /// Reject if the order would take liquidity.
    pub post_only: bool,
    /// Only reduce an existing position.
    pub reduce_only: bool,
    /// Self-trade prevention policy.
    pub stp: SelfTradePrevention,
    /// Iceberg display slice (G-06): visible lots per slice.
    pub display_lots: Option<u64>,
    /// OCO group (G-08): engine-assigned when the request rides a
    /// place-OCO pair; `None` on ordinary requests.
    pub oco_group: Option<u64>,
    /// Client timestamp (reporting).
    pub client_ts: TimestampMs,
}

impl OrderRequest {
    /// A plain GTC limit order request.
    #[must_use]
    pub fn limit(
        subaccount: SubaccountId,
        symbol: impl Into<Symbol>,
        side: Side,
        price_ticks: u64,
        qty_lots: u64,
    ) -> Self {
        Self {
            subaccount,
            symbol: symbol.into(),
            side,
            order_type: OrderType::Limit,
            price_ticks: Some(price_ticks),
            qty_lots,
            tif: TimeInForce::Gtc,
            post_only: false,
            reduce_only: false,
            stp: SelfTradePrevention::CancelNewest,
            display_lots: None,
            oco_group: None,
            client_ts: 0,
        }
    }

    /// An immediate-or-cancel market order request.
    #[must_use]
    pub fn market(
        subaccount: SubaccountId,
        symbol: impl Into<Symbol>,
        side: Side,
        qty_lots: u64,
    ) -> Self {
        Self {
            subaccount,
            symbol: symbol.into(),
            side,
            order_type: OrderType::Market,
            price_ticks: None,
            qty_lots,
            tif: TimeInForce::Ioc,
            post_only: false,
            reduce_only: false,
            stp: SelfTradePrevention::CancelNewest,
            display_lots: None,
            oco_group: None,
            client_ts: 0,
        }
    }
}

/// Everything an operator or the outside world can ask the engine to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Credit a subaccount's cash balance (genesis, deposits).
    Deposit {
        /// Target subaccount.
        subaccount: SubaccountId,
        /// Amount in quote minor units.
        amount_quote_minor: u128,
    },
    /// Debit a subaccount (withdrawals, must keep equity solvent).
    Withdraw {
        /// Source subaccount.
        subaccount: SubaccountId,
        /// Amount in quote minor units.
        amount_quote_minor: u128,
    },
    /// Place an order.
    Place {
        /// The request.
        request: OrderRequest,
        /// Engine wall-clock for this command.
        now: TimestampMs,
    },
    /// Cancel one resting order.
    Cancel {
        /// Owning subaccount.
        subaccount: SubaccountId,
        /// Engine-assigned order id.
        order_id: u64,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Cancel every resting order of a subaccount (optionally one symbol).
    CancelAll {
        /// Owning subaccount.
        subaccount: SubaccountId,
        /// Restrict to one instrument when set.
        symbol: Option<Symbol>,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Push a provider observation into an underlying's oracle.
    OracleUpdate {
        /// Underlying base symbol, e.g. `"BTC"`.
        base_symbol: String,
        /// Provider name.
        provider: String,
        /// Observation timestamp (ms).
        ts: TimestampMs,
        /// Observed price, quote minor per `1.0` base.
        price_quote_minor: u128,
    },
    /// Advance time: funding boundaries, expiries, halts, rewards,
    /// liquidation sweeps. One deterministic heartbeat.
    Tick {
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Create an RFQ (an unsigned intent — cannot move funds).
    RfqCreate {
        /// Requesting taker.
        taker: SubaccountId,
        /// Package legs (instrument, taker side, qty lots).
        legs: Vec<RfqLegCommand>,
        /// Empty = open to all makers; otherwise private direction.
        counterparties: Vec<SubaccountId>,
        /// Bounds on the taker's total cost.
        min_total_cost_quote_minor: Option<u128>,
        /// Upper bound on the taker's total cost.
        max_total_cost_quote_minor: Option<u128>,
        /// Quoting window.
        ttl_ms: TimestampMs,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Maker submits (or atomically replaces) a quote on an RFQ.
    RfqQuote {
        /// Quoting maker.
        maker: SubaccountId,
        /// Target RFQ.
        rfq_id: u64,
        /// Price per leg in ticks, aligned with the RFQ's leg order.
        leg_prices_ticks: Vec<u64>,
        /// Quote window.
        ttl_ms: TimestampMs,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Taker atomically executes one quote of their RFQ.
    RfqExecute {
        /// Executing taker.
        taker: SubaccountId,
        /// The RFQ.
        rfq_id: u64,
        /// The accepted quote.
        quote_id: u64,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Cancel an RFQ (taker) or a quote (maker).
    RfqCancel {
        /// Owning subaccount.
        subaccount: SubaccountId,
        /// Cancel this RFQ when set.
        rfq_id: Option<u64>,
        /// Cancel this quote when set.
        quote_id: Option<u64>,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Register a privately negotiated, venue-cleared block trade
    /// (both accounts' consent in the venue trust model). Printed to the
    /// public tape after the broadcast delay.
    BlockTrade {
        /// First counterparty (taker side of each leg).
        taker: SubaccountId,
        /// Second counterparty (maker side of each leg).
        maker: SubaccountId,
        /// Legs: (symbol, taker side, qty lots, price ticks).
        legs: Vec<(Symbol, Side, u64, u64)>,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Internal transfer between two subaccounts (margin-neutral for the venue).
    Transfer {
        /// Source subaccount.
        from: SubaccountId,
        /// Destination subaccount.
        to: SubaccountId,
        /// Amount, quote minor.
        amount_quote_minor: u128,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Tender a long American option position for early exercise.
    ///
    /// The request parks until `requested_at + settlement_twap_ms`, then
    /// settles on the TWAP of that window: the long closes at TWAP
    /// intrinsic (minus the exercise fee) and matching short positions
    /// are assigned pro-rata. European markets reject the command.
    Exercise {
        /// The tendering (long) subaccount.
        subaccount: SubaccountId,
        /// American option market.
        symbol: Symbol,
        /// Lots to exercise.
        lots: u64,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Configure market-maker protection for (subaccount, currency).
    SetMmp {
        /// Protected subaccount.
        subaccount: SubaccountId,
        /// Underlying whose instruments the config governs.
        base_symbol: String,
        /// Rolling window length (ms).
        interval_ms: TimestampMs,
        /// Freeze duration (0 = until manual reset).
        frozen_time_ms: TimestampMs,
        /// Cumulative |fill size| in the window that trips the freeze (lots).
        amount_limit_lots: u64,
        /// Cumulative |net delta| in the window that trips the freeze (lots).
        delta_limit_lots: u64,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Enable/disable cancel-on-disconnect for a subaccount.
    SetCod {
        /// The subaccount.
        subaccount: SubaccountId,
        /// The setting.
        enabled: bool,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// A session dropped: cancel-on-disconnect pulls the subaccount's
    /// resting orders and quotes when enabled.
    SessionDropped {
        /// The disconnected subaccount.
        subaccount: SubaccountId,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Place a batch of orders atomically (G-09). Every request is
    /// validated against the pre-batch snapshot; one failure rejects the
    /// whole batch. Intra-batch price-time interaction does not occur —
    /// the batch is a transport and margin-atomicity primitive, not a
    /// matching primitive.
    PlaceBatch {
        /// The requests, in submission order.
        requests: Vec<OrderRequest>,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Cancel a set of orders in one atomic command (G-09).
    CancelBatch {
        /// Owning subaccount.
        subaccount: SubaccountId,
        /// Order ids to cancel.
        order_ids: Vec<u64>,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Amend a resting order in place or by re-placement (G-09).
    ///
    /// Queue rules (the Derive/Paradex ladder, resolved for our pure-match
    /// engine): a price change or a size increase loses queue priority via
    /// cancel-and-replace (new order id, back of the level); a pure size
    /// reduction at the same price keeps priority and is applied in place.
    Amend {
        /// Owning subaccount.
        subaccount: SubaccountId,
        /// The order to amend.
        order_id: u64,
        /// New limit price in ticks (`None` = unchanged).
        new_price_ticks: Option<u64>,
        /// New *total open* quantity in lots (`None` = unchanged).
        new_open_lots: Option<u64>,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Open a pre-open auction on one instrument (G-12). Orders rest
    /// without matching until the uncross time, then print at one uniform
    /// clearing price and continuous trading resumes.
    BeginAuction {
        /// Instrument symbol.
        symbol: Symbol,
        /// When the sweep uncrosses the book.
        uncross_at: TimestampMs,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Credit a subaccount's *non-quote* collateral balance (G-17).
    DepositCollateral {
        /// Target subaccount.
        subaccount: SubaccountId,
        /// Collateral currency code (must be configured).
        currency: String,
        /// Amount in the currency's minor units.
        amount_minor: u128,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Debit a subaccount's non-quote collateral (margin-gated, G-17).
    WithdrawCollateral {
        /// Source subaccount.
        subaccount: SubaccountId,
        /// Collateral currency code.
        currency: String,
        /// Amount in the currency's minor units.
        amount_minor: u128,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Convert between collateral currencies (and to/from quote cash) at
    /// oracle prices, zero fee, margin-gated (G-17).
    ConvertCollateral {
        /// Converting subaccount.
        subaccount: SubaccountId,
        /// Source currency code ("USD" = quote cash).
        from: String,
        /// Destination currency code ("USD" = quote cash).
        to: String,
        /// Amount in the source currency's minor units.
        from_amount_minor: u128,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Place a one-cancels-other pair (G-08): two orders — typically a
    /// take-profit and a stop-loss bracketing a position. The pair is
    /// validated and margined atomically; the first sibling to fill
    /// completely, trigger, or expire cancels the other with
    /// [`OrderCloseReason::OcoSibling`](crate::event::OrderCloseReason::OcoSibling).
    PlaceOco {
        /// First leg of the bracket.
        first: OrderRequest,
        /// Second leg of the bracket.
        second: OrderRequest,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Start a TWAP execution (G-10): a parent order sliced into equal
    /// child placements spaced by `slice_interval_ms`. Children are
    /// marketable limit orders bounded by `limit_ticks` (when supplied);
    /// the parent completes when every slice has been placed.
    PlaceTwap {
        /// Owning subaccount.
        subaccount: SubaccountId,
        /// Instrument symbol.
        symbol: Symbol,
        /// Side of every slice.
        side: Side,
        /// Total quantity in lots (>= `slices`).
        total_lots: u64,
        /// Number of child slices.
        slices: u64,
        /// Wall-clock spacing between slices, ms.
        slice_interval_ms: TimestampMs,
        /// Worst acceptable price in ticks per slice (`None` = market
        /// children, only bounded by the book).
        limit_ticks: Option<u64>,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Cancel a TWAP parent (already-placed children are not recalled).
    CancelTwap {
        /// Owning subaccount.
        subaccount: SubaccountId,
        /// Parent id.
        parent_id: u64,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Open a new LP underwriter vault (G-16, operator action).
    VaultCreate {
        /// Share of the insurance revenue allocation routed to this
        /// vault, bps.
        revenue_share_bps: u64,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Queue a subscription into a vault for the next epoch boundary
    /// (G-16). Margin-gated: the amount must be spendable cash.
    VaultSubscribe {
        /// The vault.
        vault_id: u64,
        /// Subscribing subaccount.
        subaccount: SubaccountId,
        /// Quote minor to subscribe.
        amount_quote_minor: u128,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Queue a redemption from a vault for the next epoch boundary
    /// (G-16, first-come-first-served within the epoch).
    VaultRedeem {
        /// The vault.
        vault_id: u64,
        /// Redeeming subaccount.
        subaccount: SubaccountId,
        /// Shares to redeem.
        shares: u128,
        /// Engine wall-clock.
        now: TimestampMs,
    },
    /// Enroll a subaccount in the market-maker tier program (G-15).
    /// Enrollment is free but *measured*: obligations are evaluated at
    /// every review window from the randomized liquidity samples the
    /// sweep already takes, and the tier discount follows the measured
    /// performance — never the enrollment.
    MmTierEnroll {
        /// The enrolling maker.
        subaccount: SubaccountId,
        /// Engine wall-clock.
        now: TimestampMs,
    },
}

/// One RFQ package leg as issued by a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfqLegCommand {
    /// Instrument symbol.
    pub symbol: Symbol,
    /// Taker's side on this leg.
    pub side: Side,
    /// Quantity in lots.
    pub qty_lots: u64,
}
