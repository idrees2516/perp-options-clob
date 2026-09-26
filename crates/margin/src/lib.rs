//! # poc-margin
//!
//! Cross-margin portfolio engine: **accounts, positions, and scenario-based
//! portfolio margin** in the Derive V3 / CME SPAN tradition.
//!
//! ## The margin philosophy
//!
//! Isolated per-position margin (BitMEX legacy, Binance futures default)
//! wastes capital: a short call hedged by a long perp holds roughly
//! offsetting risk but pays margin twice. Portfolio margin charges the
//! **worst-case loss of the whole book** under a standardized stress grid,
//! so hedges net out and only true residual risk is capitalized.
//!
//! ## Standardized Futures and Portfolio Margin (SFPM)
//!
//! The scenario grid is anchored to the underlying's **scanning range**,
//! which we set equal to the perp maintenance ratio (the venue's own
//! definition of how far the underlying can move before liquidation):
//!
//! * spot shifts: `{±1.0, ±0.5, ±0.25, 0}` × scan range;
//! * implied-vol shifts: `{±1.0, 0}` × the vol-shift parameter;
//! * every leg (perps *and* options) is repriced under each scenario and
//!   the **portfolio** PnL is taken, so long calls offset short perps;
//! * maintenance = worst-case portfolio loss across the grid;
//! * initial = maintenance × an uplift multiplier (Derive uses 1.4×);
//! * **short-option minimum charge (SOMC)** floors the margin of net short
//!   option legs — wings can move further than any scan range, so short
//!   gamma pays for its tail explicitly.
//!
//! ## Premium-unpaid (variation margin) convention
//!
//! Options exchange **no cash at trade time** (Derive V3 model): entries
//! anchor unrealized PnL and cash moves only through funding, settlement,
//! fees, and liquidation. This keeps one unified accounting identity:
//!
//! ```text
//! equity  = cash + Σ (mark − entry) × signed_qty      (all instruments)
//! health  = equity vs initial / maintenance
//! ```

pub mod account;
pub mod blackscholes;
pub mod portfolio;

pub use account::{Health, MarginAccount, MarginSummary, OpenOrderInfo, Position};
pub use blackscholes::{Flavour, ImpliedVolError, OptionAnalytics, OptionLegView};
pub use portfolio::{
    Mark, MarkSet, PortfolioMarginEngine, UnderlyingMargin, UnderlyingMarginParams,
};
