//! Pre-trade risk gates.
//!
//! The engine calls [`check_order`] *before* any book or ledger mutation.
//! Every rejection reason is a typed [`Rejection`] so client-facing layers
//! can translate them 1:1 into error codes (Derive V3-style structured
//! rejects).

use std::collections::BTreeMap;

use poc_core::{Instrument, Order, OrderType, Side, Symbol};
use poc_margin::{MarginAccount, MarginSummary, MarkSet, PortfolioMarginEngine};

/// Per-venue risk limits (position caps, order caps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskLimits {
    /// Maximum absolute position (lots) per subaccount per instrument.
    pub max_position_lots: u64,
    /// Maximum simultaneously resting orders per subaccount.
    pub max_open_orders: usize,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_position_lots: 10_000,
            max_open_orders: 200,
        }
    }
}

/// Typed order rejection reasons (mirrors production venue error codes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// Instrument does not exist.
    UnknownInstrument,
    /// Account does not exist (accounts are deposit-gated).
    UnknownAccount,
    /// Quantity or price violates instrument constraints.
    InvalidOrder(String),
    /// Market is halted (oracle quorum lost) — fail-safe.
    MarketHalted,
    /// Limit price deviates from the mark beyond the instrument band.
    OutsidePriceBand {
        /// Limit price (quote minor per base).
        price_quote_minor: u128,
        /// Mark price at rejection.
        mark_quote_minor: u128,
    },
    /// Post-only order would cross the book.
    PostOnlyWouldCross,
    /// Reduce-only order would open or increase a position.
    ReduceOnlyWouldIncrease,
    /// Resulting position would exceed the per-instrument cap.
    PositionLimitExceeded {
        /// Position after the hypothetical full fill.
        projected_lots: i64,
        /// The configured cap.
        max_lots: u64,
    },
    /// Too many resting orders.
    TooManyOpenOrders {
        /// Current count.
        current: usize,
        /// The cap.
        max: usize,
    },
    /// Filling the order fully at its limit price would breach initial
    /// margin.
    InsufficientMargin {
        /// Additional margin the fill would require (quote minor).
        shortfall: u128,
        /// Equity the account would still hold after the fill.
        equity_after: i128,
    },
    /// No mark price is available for margining the instrument.
    MissingMark,
    /// Portfolio greeks cap exceeded (G-41).
    GreeksLimitExceeded {
        /// Which cap ("vega" | "gamma").
        what: &'static str,
        /// Resulting exposure.
        would_be: i128,
        /// The cap.
        cap: i128,
    },
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::UnknownInstrument => write!(f, "unknown instrument"),
            Rejection::UnknownAccount => write!(f, "unknown account"),
            Rejection::InvalidOrder(why) => write!(f, "invalid order: {why}"),
            Rejection::MarketHalted => write!(f, "market halted"),
            Rejection::OutsidePriceBand { .. } => write!(f, "limit outside price band"),
            Rejection::PostOnlyWouldCross => write!(f, "post-only order would cross"),
            Rejection::ReduceOnlyWouldIncrease => write!(f, "reduce-only would increase position"),
            Rejection::PositionLimitExceeded {
                projected_lots,
                max_lots,
            } => {
                write!(f, "position {projected_lots} lots exceeds cap {max_lots}")
            }
            Rejection::TooManyOpenOrders { current, max } => {
                write!(f, "open orders {current} exceeds cap {max}")
            }
            Rejection::InsufficientMargin { shortfall, .. } => {
                write!(f, "insufficient margin: shortfall {shortfall}")
            }
            Rejection::MissingMark => write!(f, "missing mark for margining"),
            Rejection::GreeksLimitExceeded {
                what,
                would_be,
                cap,
            } => write!(f, "portfolio {what} cap exceeded: {would_be} > {cap}"),
        }
    }
}

impl std::error::Error for Rejection {}

/// Everything the pre-trade gate needs to decide.
pub struct OrderRiskContext<'a> {
    /// The order under consideration.
    pub order: &'a Order,
    /// Instrument being traded.
    pub instrument: &'a Instrument,
    /// Full instrument registry (for portfolio scans).
    pub instruments: &'a BTreeMap<Symbol, Instrument>,
    /// The placing account.
    pub account: &'a MarginAccount,
    /// Current marks per underlying.
    pub marks: &'a BTreeMap<String, MarkSet>,
    /// Portfolio margin calculator.
    pub margin_engine: &'a PortfolioMarginEngine,
    /// Mark price of this instrument (quote minor per base), used for
    /// bands and hypothetical fills.
    pub mark_quote_minor: u128,
    /// Best bid/ask in ticks (for post-only crossing detection).
    pub best_bid_ticks: Option<u64>,
    /// Best ask in ticks.
    pub best_ask_ticks: Option<u64>,
    /// Venue limits.
    pub limits: RiskLimits,
    /// Whether trading is halted on this market.
    pub halted: bool,
    /// Worst-case fee for the full fill (signed, positive = paid).
    pub estimated_fee_quote_minor: i128,
    /// Haircut-adjusted collateral equity the account holds besides its
    /// positions (G-17): added to the hypothetical summary's equity so the
    /// pre-trade gate sees the same balance sheet the liquidation engine
    /// enforces.
    pub collateral_equity_quote_minor: i128,
}

/// Run every pre-trade gate. Returns `Ok(())` or the first rejection.
///
/// The margin gate simulates the *worst case*: the order fills completely
/// at its own limit price and the taker fee is charged immediately.
pub fn check_order(ctx: &OrderRiskContext<'_>) -> Result<MarginSummary, Rejection> {
    let order = ctx.order;

    if ctx.halted {
        return Err(Rejection::MarketHalted);
    }

    // --- Price band: fat-finger and manipulation guard ------------------
    if let Some(price_ticks) = order.price_ticks {
        if let Some(price) = ctx.instrument.price_quote_minor(price_ticks) {
            if ctx.mark_quote_minor > 0 {
                let diff = price.abs_diff(ctx.mark_quote_minor);
                let exceeds = diff.checked_mul(10_000).is_some_and(|d| {
                    d > ctx.mark_quote_minor * u128::from(ctx.instrument.price_band_bps())
                });
                if exceeds {
                    return Err(Rejection::OutsidePriceBand {
                        price_quote_minor: price,
                        mark_quote_minor: ctx.mark_quote_minor,
                    });
                }
            }
        }
    }

    // --- Post-only would cross -------------------------------------------
    if order.post_only {
        if let Some(price_ticks) = order.price_ticks {
            let crosses = match order.side {
                Side::Bid => ctx.best_ask_ticks.is_some_and(|a| price_ticks >= a),
                Side::Ask => ctx.best_bid_ticks.is_some_and(|b| price_ticks <= b),
            };
            if crosses {
                return Err(Rejection::PostOnlyWouldCross);
            }
        }
    }

    // --- Reduce-only semantics -------------------------------------------
    let current_lots = ctx.account.lots_of(&order.symbol);
    if order.reduce_only {
        let increases = match (current_lots.signum(), order.side) {
            (0, _) => true, // no position: any order opens one
            (1, Side::Bid) => true,
            (-1, Side::Ask) => true,
            _ => false,
        };
        if increases {
            return Err(Rejection::ReduceOnlyWouldIncrease);
        }
    }

    // --- Position cap -----------------------------------------------------
    let signed_qty = i64::try_from(order.open_qty()).unwrap_or(i64::MAX) * order.side.sign();
    let projected = current_lots.saturating_add(signed_qty);
    let projected_abs = projected.unsigned_abs();
    if projected_abs > ctx.limits.max_position_lots {
        return Err(Rejection::PositionLimitExceeded {
            projected_lots: projected,
            max_lots: ctx.limits.max_position_lots,
        });
    }

    // --- Open order cap ----------------------------------------------------
    if ctx.account.open_orders.len() >= ctx.limits.max_open_orders {
        return Err(Rejection::TooManyOpenOrders {
            current: ctx.account.open_orders.len(),
            max: ctx.limits.max_open_orders,
        });
    }

    // --- Margin gate: simulate worst-case full fill -----------------------
    if ctx.marks.get(ctx.instrument.base_symbol()).is_none() {
        return Err(Rejection::MissingMark);
    }
    let fill_price = order
        .price_ticks
        .and_then(|t| ctx.instrument.price_quote_minor(t))
        .unwrap_or(ctx.mark_quote_minor); // market order: assume mark

    let mut hypothetical = ctx.account.clone();
    hypothetical.apply_fill(
        ctx.instrument,
        &order.symbol,
        order.side,
        order.open_qty(),
        fill_price,
    );
    hypothetical.apply_fee(ctx.estimated_fee_quote_minor);

    let mut summary = ctx
        .margin_engine
        .margin_summary(&hypothetical, ctx.instruments, ctx.marks)
        .ok_or(Rejection::MissingMark)?;
    summary.equity_quote_minor = summary
        .equity_quote_minor
        .saturating_add(ctx.collateral_equity_quote_minor);

    let available = summary.available_quote_minor();
    if available < 0 {
        let shortfall = (-available).unsigned_abs();
        return Err(Rejection::InsufficientMargin {
            shortfall,
            equity_after: summary.equity_quote_minor,
        });
    }
    Ok(summary)
}

/// Incremental order-margin reservation: the additional initial margin the
/// account would consume if `order` filled completely at its limit price,
/// *not counting* margin already reserved by other resting orders.
///
/// The engine adds this to `order_margin_quote_minor` when the order rests
/// and subtracts it when it leaves the book.
#[must_use]
pub fn order_margin_increment(ctx: &OrderRiskContext<'_>) -> u128 {
    let order = ctx.order;
    let fill_price = order
        .price_ticks
        .and_then(|t| ctx.instrument.price_quote_minor(t))
        .unwrap_or(ctx.mark_quote_minor);

    let before = ctx
        .margin_engine
        .initial_margin(ctx.account, ctx.instruments, ctx.marks);

    let mut after = ctx.account.clone();
    after.apply_fill(
        ctx.instrument,
        &order.symbol,
        order.side,
        order.open_qty(),
        fill_price,
    );
    let after_margin = ctx
        .margin_engine
        .initial_margin(&after, ctx.instruments, ctx.marks);

    after_margin.saturating_sub(before)
}

/// Effective quantity a reduce-only order may fill given the current
/// position: caps at the current absolute size, so a reduce-only can never
/// flip a position through matching alone.
#[must_use]
pub fn reduce_only_cap(order: &Order, current_lots: i64) -> u64 {
    if !order.reduce_only || current_lots == 0 {
        return order.open_qty();
    }
    let aligned = current_lots.signum() == -order.side.sign();
    if aligned {
        order.open_qty().min(current_lots.unsigned_abs())
    } else {
        0
    }
}

/// Market-order worst-case price estimate: the worst tick the sweep could
/// reach, used for fee estimation and slippage guards. Returns `None` when
/// the book side is empty (a market order would rest — invalid).
#[must_use]
pub fn market_order_worst_price(
    order: &Order,
    best_bid_ticks: Option<u64>,
    best_ask_ticks: Option<u64>,
    max_slippage_ticks: u64,
) -> Option<u64> {
    let order_type = order.order_type;
    if !matches!(order_type, OrderType::Market) {
        return order.price_ticks;
    }
    match order.side {
        Side::Bid => best_ask_ticks.map(|a| a.saturating_add(max_slippage_ticks)),
        Side::Ask => best_bid_ticks.map(|b| b.saturating_sub(max_slippage_ticks)),
    }
}

/// Convert a signed i128 amount to u128 without panicking (saturating).
#[must_use]
pub fn saturating_abs_u128(x: i128) -> u128 {
    x.unsigned_abs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use poc_core::{OrderType, PerpMarket, SelfTradePrevention, TimeInForce};
    use poc_margin::Mark;

    fn perp_market() -> Instrument {
        Instrument::Perp(PerpMarket::default()) // band 1000bps = 10%
    }

    fn order(
        id: u64,
        sub: u64,
        side: Side,
        price_ticks: Option<u64>,
        qty: u64,
        reduce_only: bool,
        post_only: bool,
    ) -> Order {
        Order {
            id,
            subaccount: sub,
            symbol: "BTC-PERP".into(),
            side,
            order_type: if price_ticks.is_some() {
                OrderType::Limit
            } else {
                OrderType::Market
            },
            price_ticks,
            qty_lots: qty,
            filled_lots: 0,
            tif: TimeInForce::Gtc,
            post_only,
            reduce_only,
            stp: SelfTradePrevention::CancelNewest,
            display_lots: None,
            trailing_extreme_quote_minor: None,
            client_ts: 0,
            engine_ts: 0,
        }
    }

    struct Fixture {
        instruments: BTreeMap<Symbol, Instrument>,
        marks: BTreeMap<String, MarkSet>,
        margin_engine: PortfolioMarginEngine,
        limits: RiskLimits,
    }

    fn fixture(spot: u128) -> Fixture {
        let mut instruments = BTreeMap::new();
        let m = perp_market();
        instruments.insert("BTC-PERP".into(), m);
        let set = MarkSet::new("BTC", spot).with_mark(
            "BTC-PERP",
            Mark::Perp {
                mark_quote_minor_per_base: spot,
            },
        );
        let mut marks = BTreeMap::new();
        marks.insert("BTC".into(), set);
        Fixture {
            instruments,
            marks,
            margin_engine: PortfolioMarginEngine::new(),
            limits: RiskLimits::default(),
        }
    }

    fn ctx<'a>(
        f: &'a Fixture,
        order: &'a Order,
        account: &'a MarginAccount,
    ) -> OrderRiskContext<'a> {
        OrderRiskContext {
            order,
            instrument: &f.instruments["BTC-PERP"],
            instruments: &f.instruments,
            account,
            marks: &f.marks,
            margin_engine: &f.margin_engine,
            mark_quote_minor: 8_000_000, // $80,000.00
            best_bid_ticks: Some(79_999),
            best_ask_ticks: Some(80_001),
            limits: f.limits,
            halted: false,
            estimated_fee_quote_minor: 0,
            collateral_equity_quote_minor: 0,
        }
    }

    #[test]
    fn halt_rejects_everything() {
        let f = fixture(8_000_000);
        let account = MarginAccount::new(1, 10_000_000);
        let o = order(1, 1, Side::Bid, Some(80_000), 10, false, false);
        let mut c = ctx(&f, &o, &account);
        c.halted = true;
        assert_eq!(check_order(&c).err(), Some(Rejection::MarketHalted));
    }

    #[test]
    fn price_band_rejects_far_limits() {
        let f = fixture(8_000_000);
        let account = MarginAccount::new(1, 100_000_000);
        // $90,000 limit vs $80,000 mark = 12.5% > 10% band.
        let o = order(1, 1, Side::Bid, Some(90_000), 10, false, false);
        let c = ctx(&f, &o, &account);
        assert!(matches!(
            check_order(&c),
            Err(Rejection::OutsidePriceBand { .. })
        ));
        // $87,999 = 9.99% — inside.
        let o = order(1, 1, Side::Bid, Some(87_999), 10, false, false);
        let c = ctx(&f, &o, &account);
        assert!(check_order(&c).is_ok());
    }

    #[test]
    fn post_only_rejects_crossing() {
        let f = fixture(8_000_000);
        let account = MarginAccount::new(1, 100_000_000);
        // Buy at 80_001 >= best ask 80_001.
        let o = order(1, 1, Side::Bid, Some(80_001), 10, false, true);
        let c = ctx(&f, &o, &account);
        assert_eq!(check_order(&c).err(), Some(Rejection::PostOnlyWouldCross));
        // Buy at 80_000 strictly below the ask: fine.
        let o = order(1, 1, Side::Bid, Some(80_000), 10, false, true);
        let c = ctx(&f, &o, &account);
        assert!(check_order(&c).is_ok());
    }

    #[test]
    fn reduce_only_rules() {
        let f = fixture(8_000_000);
        let mut account = MarginAccount::new(1, 100_000_000);
        // No position: reduce-only always rejected.
        let o = order(1, 1, Side::Bid, Some(80_000), 10, true, false);
        let c = ctx(&f, &o, &account);
        assert_eq!(
            check_order(&c).err(),
            Some(Rejection::ReduceOnlyWouldIncrease)
        );

        // Long 50: selling is allowed (reduces), buying is not.
        let mut pos = poc_margin::Position::flat("BTC-PERP");
        pos.apply_fill(&perp_market(), Side::Bid, 50, 8_000_000);
        account.positions.insert("BTC-PERP".into(), pos);
        let o = order(1, 1, Side::Ask, Some(80_000), 10, true, false);
        let c = ctx(&f, &o, &account);
        assert!(check_order(&c).is_ok());
        let o = order(1, 1, Side::Bid, Some(80_000), 10, true, false);
        let c = ctx(&f, &o, &account);
        assert_eq!(
            check_order(&c).err(),
            Some(Rejection::ReduceOnlyWouldIncrease)
        );

        // Cap: selling more than the position caps at the position size.
        let o = order(1, 1, Side::Ask, Some(80_000), 100, true, false);
        assert_eq!(reduce_only_cap(&o, 50), 50);
        let o = order(1, 1, Side::Ask, Some(80_000), 30, true, false);
        assert_eq!(reduce_only_cap(&o, 50), 30);
    }

    #[test]
    fn position_limit_enforced() {
        let mut f = fixture(8_000_000);
        f.limits.max_position_lots = 100;
        let account = MarginAccount::new(1, 100_000_000);
        let o = order(1, 1, Side::Bid, Some(80_000), 101, false, false);
        let c = ctx(&f, &o, &account);
        assert!(matches!(
            check_order(&c),
            Err(Rejection::PositionLimitExceeded {
                projected_lots: 101,
                ..
            })
        ));
        let o = order(1, 1, Side::Bid, Some(80_000), 100, false, false);
        let c = ctx(&f, &o, &account);
        assert!(check_order(&c).is_ok());
    }

    #[test]
    fn margin_gate_rejects_undercapitalized_fill() {
        let f = fixture(8_000_000);
        // 1 lot = 0.001 BTC at $80k -> notional $80 = 8_000 minor.
        // 100 lots = $8,000 notional -> IM = 3.75% × 1.4 × $8,000 ≈ $420.
        let account = MarginAccount::new(1, 50_000);
        let o = order(1, 1, Side::Bid, Some(80_000), 100, false, false);
        let c = ctx(&f, &o, &account);
        assert!(check_order(&c).is_ok());

        // 10,000 lots = $800k notional -> IM ≈ $42k >> cash: reject.
        let o = order(1, 1, Side::Bid, Some(80_000), 10_000, false, false);
        let c = ctx(&f, &o, &account);
        match check_order(&c) {
            Err(Rejection::InsufficientMargin { shortfall, .. }) => {
                assert!(shortfall > 40_000 * 100, "shortfall {shortfall}");
            }
            other => panic!("expected margin rejection, got {other:?}"),
        }
    }

    #[test]
    fn margin_gate_accounts_for_existing_positions() {
        let f = fixture(8_000_000);
        // Already long 500 lots ($40k notional) with cash $2_200: current IM
        // ≈ $2_100, so a further 100 lots must be rejected.
        let mut account = MarginAccount::new(1, 2_200);
        let mut pos = poc_margin::Position::flat("BTC-PERP");
        pos.apply_fill(&perp_market(), Side::Bid, 500, 8_000_000);
        account.positions.insert("BTC-PERP".into(), pos);
        let o = order(1, 1, Side::Bid, Some(80_000), 100, false, false);
        let c = ctx(&f, &o, &account);
        assert!(matches!(
            check_order(&c),
            Err(Rejection::InsufficientMargin { .. })
        ));
    }

    #[test]
    fn missing_mark_fails_safe() {
        let f = fixture(8_000_000);
        let account = MarginAccount::new(1, 10_000);
        let o = order(1, 1, Side::Bid, Some(80_000), 10, false, false);
        let empty_marks: BTreeMap<String, MarkSet> = BTreeMap::new();
        let mut c = ctx(&f, &o, &account);
        c.marks = &empty_marks; // no marks at all
        assert_eq!(check_order(&c).err(), Some(Rejection::MissingMark));
    }

    #[test]
    fn order_margin_increment_positive_for_new_risk() {
        let f = fixture(8_000_000);
        let account = MarginAccount::new(1, 100_000_000);
        let o = order(1, 1, Side::Bid, Some(80_000), 100, false, false);
        let c = ctx(&f, &o, &account);
        let inc = order_margin_increment(&c);
        // 100 lots × $80 = $8000 notional, 3.75% × 1.4 scan ≈ $420.
        assert!(inc > 30_000 && inc < 50_000, "increment {inc}");
    }

    #[test]
    fn market_order_worst_price_bounds_slippage() {
        let buy = order(1, 1, Side::Bid, None, 10, false, false);
        assert_eq!(
            market_order_worst_price(&buy, Some(100), Some(105), 10),
            Some(115)
        );
        let sell = order(2, 1, Side::Ask, None, 10, false, false);
        assert_eq!(
            market_order_worst_price(&sell, Some(100), Some(105), 10),
            Some(90)
        );
        // Empty opposite side: no price.
        assert_eq!(market_order_worst_price(&buy, None, None, 10), None);
    }

    #[test]
    fn saturating_conversions() {
        assert_eq!(saturating_abs_u128(-5), 5);
        assert_eq!(saturating_abs_u128(poc_core::to_i128(1 << 100)), 1 << 100);
    }
}
