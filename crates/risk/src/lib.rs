//! # poc-risk
//!
//! The risk layer: **pre-trade gates** and the **liquidation pipeline**.
//!
//! ## Pre-trade: reject before a single state change happens
//!
//! Every order passes [`pretrade`] before matching: price-band sanity,
//! position limits, reduce-only semantics, and — the core question —
//! *would the account still satisfy initial margin if this order filled in
//! full at its own limit price?* The check runs against a **hypothetical**
//! clone of the account (fill + worst-case fee applied), the same
//! "simulate then decide" pattern dYdX v4 and Derive use, so live and
//! replayed decisions cannot diverge.
//!
//! ## Liquidation: partial first, then insurance, then ADL
//!
//! When equity falls below maintenance, [`liquidation`] produces a plan in
//! the industry-standard cascade:
//!
//! 1. **Partial liquidation** — close the largest risk-contributing legs
//!    first until equity is restored (Deribit/Derive: minimize the
//!    disruption to the account; most liquidations end here).
//! 2. **Liquidation penalty** — closed legs cross at a penalized mark; the
//!    penalty flows to the insurance fund, the only party that pays for
//!    being the buyer of last resort.
//! 3. **Insurance fund** — if an account ends equity-negative after all
//!    closures, the fund absorbs the shortfall.
//! 4. **Auto-deleveraging (ADL)** — if the fund itself is exhausted, the
//!    highest-profit counterparties on the opposite side are force-closed
//!    against the bankrupt position at the bankruptcy price. This is the
//!    last resort every venue from BitMEX onward ships and hopes to never
//!    use.
//!
//! All money is integer quote-minor units; the risk layer never mutates a
//! ledger itself — it *plans*, the engine executes and journals.

pub mod greeks_limits;
pub mod liquidation;
pub mod pretrade;

pub use greeks_limits::{GreeksLimits, GreeksRejection};
pub use liquidation::{
    AdlCandidate, AdlRanking, InsuranceFund, LiquidationAction, LiquidationCandidate,
    LiquidationParams, LiquidationPlan, LiquidationPlanner, LiquidationQueue,
};
pub use pretrade::{
    check_order, market_order_worst_price, order_margin_increment, reduce_only_cap,
    OrderRiskContext, Rejection, RiskLimits,
};
