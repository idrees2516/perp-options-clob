//! Core shared types: ids, sides, time-in-force, order flags, orders, trades.
//!
//! All identifiers are `u32` (slab indices). All prices are unsigned integer
//! ticks (1 tick = 0.01). All quantities are contracts (`u32`). All cash
//! amounts are signed `i128` ticks — exact integer arithmetic end to end, no
//! floats anywhere in the engine.

/// Order identifier (slab index + generation, packed into a `u64`):
/// low 32 bits = book slot, high 32 bits = per-book insertion counter.
/// Slot recycling is guarded by the generation, so a stale cancel can never
/// hit a recycled slot.
pub type OrderId = u64;
/// Account identifier (global, assigned by the platform).
pub type AcctId = u32;
/// Symbol identifier (per-shard slab index).
pub type SymbolId = u32;
/// Underlying identifier.
pub type UnderlyingId = u16;
/// Price in ticks (1 tick = 0.01).
pub type Price = u64;
/// Quantity in contracts.
pub type Qty = u32;
/// Cash amount in signed ticks.
pub type Cash = i128;
/// Logical timestamp in nanoseconds since the epoch.
pub type Nanos = u64;
/// Monotonic sequence number for time priority and journal ordering.
pub type Seq = u64;

/// Sentinel for "no id".
pub const NO_ID: u64 = u64::MAX;

/// Pack (slot, uid) into a public order id.
#[inline(always)]
pub fn pack_id(slot: u32, uid: u32) -> OrderId {
    (uid as u64) << 32 | slot as u64
}

/// Unpack the slot part of a public order id.
#[inline(always)]
pub fn id_slot(id: OrderId) -> u32 {
    id as u32
}

/// Unpack the generation part of a public order id.
#[inline(always)]
pub fn id_uid(id: OrderId) -> u32 {
    (id >> 32) as u32
}

/// Order side. Bit value doubles as index into side arrays.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Side {
    /// Buyer.
    Buy = 0,
    /// Seller.
    Sell = 1,
}

impl Side {
    /// Opposite side.
    #[inline(always)]
    pub fn opp(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
    /// Array index (0/1).
    #[inline(always)]
    pub fn idx(self) -> usize {
        self as usize
    }
}

/// Time in force.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Tif {
    /// Good till cancelled (session scope for 0DTE: cancelled at expiry).
    Gtc = 0,
    /// Immediate or cancel: cross what is available, cancel the rest.
    Ioc = 1,
    /// Fill or kill: pre-scan depth, all-or-nothing.
    Fok = 2,
}

/// Order kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    /// Resting limit order.
    Limit = 0,
    /// Market order with explicit protection (worst acceptable) price.
    Market = 1,
}

/// Order behaviour flags (bit set on a `u8`).
pub mod flags {
    /// Liquidity-making only: reject if it would cross.
    pub const POST_ONLY: u8 = 1 << 0;
    /// Position reducing only: reject if it would increase exposure.
    pub const REDUCE_ONLY: u8 = 1 << 1;
    /// Self-trade prevention marker (DecrementAndCancel policy is always on;
    /// this flag is informational for gateways).
    pub const STP_DCA: u8 = 1 << 2;
    /// Eligible for auction participation (MOC/reopening).
    pub const AUCTION: u8 = 1 << 3;
}

/// A new order request (journalable input).
#[derive(Clone, Copy, Debug)]
pub struct NewOrder {
    /// Symbol the order targets.
    pub symbol: SymbolId,
    /// Owning account.
    pub account: AcctId,
    /// Buy or sell.
    pub side: Side,
    /// Limit price, or worst-acceptable protection price for market orders.
    pub price: Price,
    /// Quantity in contracts.
    pub qty: Qty,
    /// Time in force.
    pub tif: Tif,
    /// Order kind.
    pub kind: Kind,
    /// Behaviour flags (see [`crate::types::flags`]).
    pub flags: u8,
    /// Linked OCO order id (cancel-this-cancel-that), or [`NO_ID`].
    pub oco: OrderId,
}

impl NewOrder {
    /// Does this order carry the post-only flag?
    #[inline(always)]
    pub fn is_post_only(&self) -> bool {
        self.flags & flags::POST_ONLY != 0
    }
    /// Does this order carry the reduce-only flag?
    #[inline(always)]
    pub fn is_reduce_only(&self) -> bool {
        self.flags & flags::REDUCE_ONLY != 0
    }
}

/// An executed trade between taker and maker.
#[derive(Clone, Copy, Debug)]
pub struct Fill {
    /// Symbol traded.
    pub symbol: SymbolId,
    /// Maker (resting) order id.
    pub maker: OrderId,
    /// Taker (incoming) order id.
    pub taker: OrderId,
    /// Maker account.
    pub maker_acct: AcctId,
    /// Taker account.
    pub taker_acct: AcctId,
    /// Execution price.
    pub price: Price,
    /// Execution quantity.
    pub qty: Qty,
    /// Aggressor side.
    pub aggressor: Side,
}

/// Reject reason codes (stable u8 values, part of the wire contract).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Reject {
    /// Unknown symbol.
    UnknownSymbol = 0,
    /// Unknown account.
    UnknownAccount = 1,
    /// Order price outside the permitted band.
    PriceBand = 2,
    /// Post-only order would cross.
    PostOnlyCross = 3,
    /// Reduce-only order would increase exposure.
    ReduceOnlyWouldIncrease = 4,
    /// Insufficient cash for premium and/or margin.
    InsufficientFunds = 5,
    /// Position limit exceeded.
    PositionLimit = 6,
    /// Order rate limit exceeded.
    RateLimit = 7,
    /// Zero quantity or otherwise malformed.
    Malformed = 8,
    /// Symbol not in continuous trading (halted / auction / expired).
    NotContinuous = 9,
    /// FOK pre-scan found insufficient depth.
    FokDepth = 10,
    /// Duplicate order id.
    DuplicateOrder = 11,
    /// Cancel target unknown or already gone.
    UnknownOrder = 12,
    /// Order on an underlying that is halted.
    Halted = 13,
    /// Delta notional limit exceeded.
    DeltaLimit = 14,
    /// Per-account open-order count cap exceeded (book-flood defense).
    TooManyOrders = 15,
    /// Order quantity above the configured per-order cap.
    QtyTooLarge = 16,
    /// Cancel/replace targeted an order owned by a different account.
    NotOwner = 17,
}

impl Reject {
    /// Stable machine-readable reason string (REST/WS wire).
    pub fn name(self) -> &'static str {
        match self {
            Reject::UnknownSymbol => "UnknownSymbol",
            Reject::UnknownAccount => "UnknownAccount",
            Reject::PriceBand => "PriceBand",
            Reject::PostOnlyCross => "PostOnlyCross",
            Reject::ReduceOnlyWouldIncrease => "ReduceOnlyWouldIncrease",
            Reject::InsufficientFunds => "InsufficientFunds",
            Reject::PositionLimit => "PositionLimit",
            Reject::RateLimit => "RateLimit",
            Reject::Malformed => "Malformed",
            Reject::NotContinuous => "NotContinuous",
            Reject::FokDepth => "FokDepth",
            Reject::DuplicateOrder => "DuplicateOrder",
            Reject::UnknownOrder => "UnknownOrder",
            Reject::Halted => "Halted",
            Reject::DeltaLimit => "DeltaLimit",
            Reject::TooManyOrders => "TooManyOrders",
            Reject::QtyTooLarge => "QtyTooLarge",
            Reject::NotOwner => "NotOwner",
        }
    }
}
