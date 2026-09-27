//! # poc-economics
//!
//! The economic model of the exchange: **fees, funding, revenue routing, and
//! liquidity incentives**.
//!
//! Every rule in this crate encodes an explicit economic incentive, mirroring
//! the patterns that dominate production venues:
//!
//! * **Volume-tiered fees** with maker *rebates* at the top tier — the
//!   Hyperliquid / dYdX v4 ladder that pays for the order flow that anchors
//!   the book.
//! * **Funding** for perpetuals via the BitMEX *premium + interest* model:
//!   perp price is tethered to index by making longs pay shorts (or vice
//!   versa) whenever the mark drifts. An *everlasting option* mode implements
//!   the Paradigm "Everlasting Options" roll — longs pay the option's
//!   mark-to-market value each interval so the position behaves like a
//!   continuously renewed option without expiry management.
//! * **Revenue routing** — fee income is split between the house, the
//!   insurance fund (backstopping liquidations), and a buyback pool, with
//!   exact integer conservation.
//! * **Liquidity rewards** — a fixed per-interval reward pool distributed
//!   pro-rata to market makers who quote two-sided, tight markets, the
//!   dYdX v4 / Blur-style incentive that bootstraps depth before fee
//!   revenue can.
//!
//! ## Numeric policy
//!
//! All amounts are `u128` quote-minor units routed through [`poc_core::num`].
//! Fees owed round **up**; rebates and rewards round **down**; reward dust is
//! carried forward so that no unit of account is ever created or destroyed.

pub mod fees;
pub mod funding;
pub mod incentives;
pub mod referral;
pub mod revenue;

pub use fees::{FeeCalculator, FeeQuote, FeeSchedule, FeeTier, OptionFeeCaps};
pub use funding::{EverlastingRoll, FundingCalculator, FundingQuote};
pub use incentives::{IncentiveParams, LiquidityIncentives, RewardPool, RewardSettlement};
pub use revenue::{Allocation, CoveragePolicy, RevenueRouter, RevenueSplit};
