//! Order semantics and shared identifiers.

/// Milliseconds since the Unix epoch.
pub type TimestampMs = u64;

/// Subaccount id. One user may hold many subaccounts; margin is pooled at the
/// subaccount level (Derive V3 model).
pub type SubaccountId = u64;

/// Monotonic engine tick, advanced by every processed event. Used for
/// ordering diagnostics; wall-clock time remains `TimestampMs`.
pub type TickstampMs = u64;

/// Opaque instrument symbol, e.g. `"BTC-PERP"` or `"BTC-20260327-80000-C"`.
pub type Symbol = String;

/// Globally unique, monotonically increasing order id assigned by the engine.
pub type OrderId = u64;

/// Order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    /// Buy / long.
    Bid,
    /// Sell / short.
    Ask,
}

impl Side {
    /// Opposite side.
    #[must_use]
    pub fn opposite(self) -> Side {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }

    /// `+1` for bids, `-1` for asks — convenient for signed PnL math.
    #[must_use]
    pub fn sign(self) -> i64 {
        match self {
            Side::Bid => 1,
            Side::Ask => -1,
        }
    }
}

/// Order type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderType {
    /// Rests on the book until filled, canceled, or expired.
    Limit,
    /// Sweeps the book; any unfilled remainder is canceled (IOC by definition).
    Market,
    /// Becomes a market order once mark price crosses `trigger_price`.
    StopMarket {
        /// Mark price that arms execution.
        trigger_price: u64,
    },
    /// Becomes a limit order once mark price crosses `trigger_price`.
    StopLimit {
        /// Mark price that arms execution.
        trigger_price: u64,
        /// Limit price used once triggered.
        limit_price: u64,
    },
    /// Trailing stop-market (G-07): the trigger follows the running mark
    /// extreme by `offset_ticks`. A sell trails the *high* (triggers when
    /// the mark falls `offset` below the high); a buy trails the *low`.
    /// Activation is a market order.
    TrailingStopMarket {
        /// Trigger distance from the running extreme, in ticks.
        offset_ticks: u64,
    },
    /// Trailing stop-limit (G-07): as [`OrderType::TrailingStopMarket`]
    /// but activation rests a limit order at `limit_ticks`.
    TrailingStopLimit {
        /// Trigger distance from the running extreme, in ticks.
        offset_ticks: u64,
        /// Limit price used once triggered.
        limit_ticks: u64,
    },
}

impl OrderType {
    /// Resting limit price in ticks, if the order type can rest.
    #[must_use]
    pub fn resting_price(&self) -> Option<u64> {
        match self {
            OrderType::Limit => None,
            OrderType::Market
            | OrderType::StopMarket { .. }
            | OrderType::StopLimit { .. }
            | OrderType::TrailingStopMarket { .. }
            | OrderType::TrailingStopLimit { .. } => None,
        }
    }

    /// Whether this order parks off-book until a trigger crosses
    /// (stop and trailing-stop families).
    #[must_use]
    pub fn is_parked(&self) -> bool {
        matches!(
            self,
            OrderType::StopMarket { .. }
                | OrderType::StopLimit { .. }
                | OrderType::TrailingStopMarket { .. }
                | OrderType::TrailingStopLimit { .. }
        )
    }
}

/// Time-in-force policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeInForce {
    /// Good-til-cancelled.
    Gtc,
    /// Immediate-or-cancel: cancel any remainder after the first sweep.
    Ioc,
    /// Fill-or-kill: all-or-nothing against the current book.
    Fok,
    /// Good-til-date (ms epoch). Expired remnants are swept by the engine.
    Gtd(TimestampMs),
}

/// Self-trade prevention policy (Paradigm/Derive style).
///
/// When an incoming order would cross against a resting order from the *same
/// subaccount*, the pair is resolved before a fill is generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SelfTradePrevention {
    /// Cancel the *newest* order, keep the resting one. Default — it preserves
    /// queue priority of resting liquidity and matches Derive V3 behaviour.
    #[default]
    CancelNewest,
    /// Cancel the *resting* order and continue matching.
    CancelOldest,
    /// Cancel both orders.
    CancelBoth,
    /// Decrement both by the overlapping quantity; cancel whichever reaches
    /// zero. No fill is generated (used by firms that self-quote both sides).
    DecrementAndCancel,
}

/// A validated, engine-accepted order request.
///
/// Prices are integer ticks on the instrument's grid; quantities are integer
/// lots. Both are guaranteed positive by the constructor path in the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    /// Engine-assigned id.
    pub id: OrderId,
    /// Owning subaccount.
    pub subaccount: SubaccountId,
    /// Instrument symbol.
    pub symbol: Symbol,
    /// Side of the book.
    pub side: Side,
    /// Type (limit / market / stop variants).
    pub order_type: OrderType,
    /// Limit price in ticks. `None` for pure market orders.
    pub price_ticks: Option<u64>,
    /// Total quantity in lots.
    pub qty_lots: u64,
    /// Quantity already filled in lots.
    pub filled_lots: u64,
    /// Time-in-force.
    pub tif: TimeInForce,
    /// Reject the order if it would take (cross) liquidity.
    pub post_only: bool,
    /// Order may only reduce an existing position, never open/increase one.
    pub reduce_only: bool,
    /// Self-trade prevention policy.
    pub stp: SelfTradePrevention,
    /// Iceberg display slice (G-06): when set, only this many lots are
    /// visible on the book at a time; each consumed slice re-queues the
    /// remainder at the back of its price level (Deribit shape). `None` =
    /// fully displayed order.
    pub display_lots: Option<u64>,
    /// Trailing-stop running extreme (G-07): the best mark seen since
    /// placement — the highest for buy-side trailers, the lowest for
    /// sell-side. Updated only by journaled `TrailingUpdated` events so
    /// replay is exact. `None` for non-trailing orders.
    pub trailing_extreme_quote_minor: Option<u128>,
    /// Client-assigned epoch-ms for report matching.
    pub client_ts: TimestampMs,
    /// Engine-assigned epoch-ms.
    pub engine_ts: TimestampMs,
}

impl Order {
    /// Quantity still open (unfilled) in lots.
    #[must_use]
    pub fn open_qty(&self) -> u64 {
        self.qty_lots.saturating_sub(self.filled_lots)
    }

    /// Whether the order can still rest or match.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open_qty() > 0
    }

    /// Whether this order may rest on the book (limit semantics).
    #[must_use]
    pub fn can_rest(&self) -> bool {
        matches!(self.order_type, OrderType::Limit) && self.price_ticks.is_some()
    }
}

/// Lifecycle state of an order as reported to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderState {
    /// Accepted, not yet processed.
    Pending,
    /// Resting on the book (fully or partially unfilled).
    Open,
    /// Fully filled.
    Filled,
    /// Canceled by user, risk engine, or self-trade prevention.
    Canceled,
    /// Rejected at pre-trade (validation, margin, price bands).
    Rejected,
    /// Expired (GTD sweep or market close).
    Expired,
}
