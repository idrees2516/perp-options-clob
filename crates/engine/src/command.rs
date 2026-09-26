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
