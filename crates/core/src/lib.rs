//! # poc-core
//!
//! Shared domain primitives for the perpetuals + options CLOB engine:
//! identifiers, order semantics, instrument definitions, and the exact
//! integer money arithmetic every other crate is built on.
//!
//! ## Numeric conventions (critical)
//!
//! * **Money is exact.** All quote-currency amounts are unsigned 128-bit
//!   integers in *minor units* (e.g. cents, `1_00 == $1.00`). Every money
//!   path goes through [`num`] with explicit rounding, so ledger invariants
//!   (conservation of funds) can be asserted in tests.
//! * **Prices** are per `1.0` base unit, expressed in quote minor units,
//!   stored on the book as integer *ticks* (`price_minor = ticks * tick_size`).
//! * **Quantities** are integer *lots* (`base_minor = lots * lot_size`).
//! * **f64** is allowed *only* for analytics (implied volatility, greeks,
//!   stress-test repricing). It never touches a ledger.
//!
//! ## Notional formula
//!
//! For a fill of `q` lots at `p` ticks on an instrument with base
//! precision `B` (i.e. `10^B` base minor units per whole base unit):
//!
//! ```text
//! notional_quote_minor = (p * tick_size) * (q * lot_size) / 10^B
//! ```
//!
//! All products are computed in `u128` with checked arithmetic and a single
//! documented rounding decision at the division.

pub mod errors;
pub mod instrument;
pub mod num;
pub mod types;

pub use errors::{CoreError, MathError};
pub use instrument::{
    EverlastingParams, FundingParams, Instrument, OptionKind, OptionMarginParams, OptionMarket,
    OptionVariant, PerpMarket,
};
pub use num::{apply_bps, mul_div, mul_div_ceil, mul_div_floor, to_i128, Rounding};
pub use types::{
    Order, OrderId, OrderState, OrderType, SelfTradePrevention, Side, SubaccountId, Symbol,
    TickstampMs, TimeInForce, TimestampMs,
};
