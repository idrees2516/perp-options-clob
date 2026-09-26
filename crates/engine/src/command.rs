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
}
