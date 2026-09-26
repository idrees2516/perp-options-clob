//! Positions and cross-margin accounts.
//!
//! ## Fill application semantics
//!
//! One algorithm covers perps and premium-unpaid options alike
//! ([`Position::apply_fill`]): weighted-average entry on position
//! *extension*, realized PnL on *reduction*, price carried through on
//! flips. The identity `equity = cash + Σ uPnL` is maintained by
//! construction because entries never touch cash (variation-margin
//! convention; see crate docs).

use std::collections::BTreeMap;

use poc_core::{Instrument, OrderId, Side, SubaccountId, Symbol};

/// One instrument position held by a subaccount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    /// Instrument symbol.
    pub symbol: Symbol,
    /// Signed quantity in lots (`> 0` long, `< 0` short).
    pub signed_lots: i64,
    /// Volume-weighted average entry price, quote-minor per `1.0` base
    /// (premium for options, index-like price for perps).
    pub avg_entry_quote_minor: u128,
    /// Lifetime realized PnL, quote minor (signed).
    pub realized_pnl_quote_minor: i128,
}

impl Position {
    /// A new flat position for `symbol`.
    #[must_use]
    pub fn flat(symbol: impl Into<Symbol>) -> Self {
        Self {
            symbol: symbol.into(),
            signed_lots: 0,
            avg_entry_quote_minor: 0,
            realized_pnl_quote_minor: 0,
        }
    }

    /// Whether the position is flat.
    #[must_use]
    pub fn is_flat(&self) -> bool {
        self.signed_lots == 0
    }

    /// Apply a fill of `qty_lots` at `price_quote_minor` (per `1.0` base) to
    /// the position.
    ///
    /// Returns the realized PnL delta (signed, quote minor) — zero for
    /// extensions, `(price − entry) × reduced_lots` for reductions and
    /// flips, with the per-base → per-lot conversion performed by the
    /// instrument's lot size and base decimals.
    ///
    /// This is the *only* position mutation path; event replay calls it
    /// with identical arguments and must get identical results.
    #[must_use]
    pub fn apply_fill(
        &mut self,
        instrument: &Instrument,
        side: Side,
        qty_lots: u64,
        price_quote_minor: u128,
    ) -> i128 {
        if qty_lots == 0 {
            return 0;
        }
        let qty_i = i64::try_from(qty_lots).unwrap_or(i64::MAX);
        let signed_qty = qty_i * side.sign();
        let old_qty = self.signed_lots;
        let new_qty = old_qty + signed_qty;

        if old_qty == 0 {
            // Opening fresh: entry anchors at the fill price.
            self.avg_entry_quote_minor = price_quote_minor;
            self.signed_lots = new_qty;
            return 0;
        }

        let same_direction = old_qty.signum() == signed_qty.signum();
        if same_direction {
            // Extension: volume-weight the entry price (per base, by lots).
            let old_abs = u128::from(old_qty.unsigned_abs());
            let add_abs = u128::from(qty_lots);
            let total = old_abs + add_abs;
            if total > 0 {
                let weighted = old_abs
                    .saturating_mul(self.avg_entry_quote_minor)
                    .saturating_add(add_abs.saturating_mul(price_quote_minor));
                if let Some(avg) = weighted.checked_div(total) {
                    self.avg_entry_quote_minor = avg;
                }
            }
            self.signed_lots = new_qty;
            return 0;
        }

        // Reduction (possibly with a flip). Realized PnL is computed in
        // per-lot space so the per-base → per-lot conversion happens once
        // per price, not per lot.
        let fill_per_lot = quote_per_lot(instrument, price_quote_minor);
        let entry_per_lot = quote_per_lot(instrument, self.avg_entry_quote_minor);
        let reduced = old_qty.unsigned_abs().min(qty_lots); // lots closed
        let realized = if old_qty > 0 {
            (fill_per_lot - entry_per_lot) * i128::from(reduced)
        } else {
            (entry_per_lot - fill_per_lot) * i128::from(reduced)
        };
        self.realized_pnl_quote_minor = self.realized_pnl_quote_minor.saturating_add(realized);
        self.signed_lots = new_qty;
        if new_qty == 0 {
            self.avg_entry_quote_minor = 0;
        } else if new_qty.signum() != old_qty.signum() {
            // Flipped: the new-side portion enters at the fill price.
            self.avg_entry_quote_minor = price_quote_minor;
        }
        realized
    }

    /// Signed unrealized PnL at `mark_quote_minor` (per `1.0` base) for
    /// this position, in quote minor units.
    #[must_use]
    pub fn unrealized_pnl(&self, instrument: &Instrument, mark_quote_minor: u128) -> Option<i128> {
        if self.signed_lots == 0 {
            return Some(0);
        }
        instrument.linear_pnl_quote_minor(
            self.avg_entry_quote_minor,
            mark_quote_minor,
            self.signed_lots,
        )
    }

    /// Mark value of the position (signed): `mark × signed_qty`.
    #[must_use]
    pub fn mark_value(&self, instrument: &Instrument, mark_quote_minor: u128) -> Option<i128> {
        if self.signed_lots == 0 {
            return Some(0);
        }
        let notional = instrument.position_notional_minor(mark_quote_minor, self.signed_lots)?;
        let signed = if self.signed_lots < 0 {
            -to_i128(notional)
        } else {
            to_i128(notional)
        };
        Some(signed)
    }
}

/// Saturating `u128 → i128` conversion for prices and money.
fn to_i128(x: u128) -> i128 {
    i128::try_from(x).unwrap_or(i128::MAX)
}

/// Convert a per-base price (quote minor) to a per-lot amount, half-up.
fn quote_per_lot(instrument: &Instrument, price_per_base: u128) -> i128 {
    let base_unit = match 10_u128.checked_pow(instrument.base_decimals()) {
        Some(b) => b,
        None => return to_i128(price_per_base),
    };
    match poc_core::mul_div(
        price_per_base,
        instrument.lot_size(),
        base_unit,
        poc_core::Rounding::NearestHalfUp,
    ) {
        Some(v) => to_i128(v),
        None => 0,
    }
}

/// A cross-margin subaccount: cash plus every position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarginAccount {
    /// Owning subaccount id.
    pub id: SubaccountId,
    /// Cash balance, quote minor. Signed: negative only transiently during
    /// bankruptcy processing (the liquidation engine claws it back to 0).
    pub cash_quote_minor: i128,
    /// Positions by instrument symbol (flat positions are pruned).
    pub positions: BTreeMap<Symbol, Position>,
    /// Order margin reserved for open orders (initial margin the open
    /// order book would consume if fully filled at limit prices).
    pub order_margin_quote_minor: u128,
    /// Orders currently resting on the book (for reduce-only & reservation
    /// recomputation).
    pub open_orders: BTreeMap<OrderId, OpenOrderInfo>,
    /// Lifetime fees paid (gross, positive number, reporting only).
    pub fees_paid_quote_minor: u128,
    /// Lifetime funding paid (signed; positive = net paid, negative = net
    /// received).
    pub funding_pnl_quote_minor: i128,
}

/// Bookkeeping for one resting order of this account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOrderInfo {
    /// Engine-assigned order id.
    pub order_id: OrderId,
    /// Instrument symbol.
    pub symbol: Symbol,
    /// Side.
    pub side: Side,
    /// Limit price in ticks.
    pub price_ticks: u64,
    /// Open (unfilled) quantity in lots.
    pub open_lots: u64,
}

impl MarginAccount {
    /// A new account with `id` and a starting cash deposit.
    #[must_use]
    pub fn new(id: SubaccountId, initial_cash_quote_minor: i128) -> Self {
        Self {
            id,
            cash_quote_minor: initial_cash_quote_minor,
            positions: BTreeMap::new(),
            order_margin_quote_minor: 0,
            open_orders: BTreeMap::new(),
            fees_paid_quote_minor: 0,
            funding_pnl_quote_minor: 0,
        }
    }

    /// Mutable access to a position (creating a flat one if absent).
    pub fn position_mut(&mut self, symbol: &str) -> &mut Position {
        self.positions
            .entry(symbol.to_owned())
            .or_insert_with(|| Position::flat(symbol))
    }

    /// Read-only position lookup.
    #[must_use]
    pub fn position(&self, symbol: &str) -> Option<&Position> {
        self.positions.get(symbol)
    }

    /// Signed lots held on an instrument (0 when absent).
    #[must_use]
    pub fn lots_of(&self, symbol: &str) -> i64 {
        self.position(symbol).map_or(0, |p| p.signed_lots)
    }

    /// Apply a fill to the account: position mutation plus realized-PnL
    /// settlement into cash. Premium-unpaid convention: the entry anchor is
    /// updated inside the position; only *realized* PnL moves cash.
    ///
    /// Returns the realized PnL delta for the event log.
    pub fn apply_fill(
        &mut self,
        instrument: &Instrument,
        symbol: &str,
        side: Side,
        qty_lots: u64,
        price_quote_minor: u128,
    ) -> i128 {
        let realized =
            self.position_mut(symbol)
                .apply_fill(instrument, side, qty_lots, price_quote_minor);
        if realized != 0 {
            self.cash_quote_minor = self.cash_quote_minor.saturating_add(realized);
        }
        if let Some(p) = self.positions.get(symbol) {
            if p.is_flat() {
                self.positions.remove(symbol);
            }
        }
        realized
    }

    /// Record a fee (signed: positive = paid by the account).
    pub fn apply_fee(&mut self, fee_quote_minor: i128) {
        if fee_quote_minor == 0 {
            return;
        }
        if fee_quote_minor > 0 {
            self.fees_paid_quote_minor = self
                .fees_paid_quote_minor
                .saturating_add(fee_quote_minor.unsigned_abs());
        }
        self.cash_quote_minor = self.cash_quote_minor.saturating_sub(fee_quote_minor);
    }

    /// Record a funding credit (positive = received).
    pub fn apply_funding(&mut self, credit_quote_minor: i128) {
        self.cash_quote_minor = self.cash_quote_minor.saturating_add(credit_quote_minor);
        self.funding_pnl_quote_minor = self
            .funding_pnl_quote_minor
            .saturating_sub(credit_quote_minor);
    }

    /// Record a reward credit (liquidity incentives).
    pub fn apply_reward(&mut self, credit_quote_minor: u128) {
        self.cash_quote_minor = self
            .cash_quote_minor
            .saturating_add(i128::try_from(credit_quote_minor).unwrap_or(i128::MAX));
    }

    /// Register a resting order for margin reservation and reduce-only
    /// accounting.
    pub fn track_order(&mut self, info: OpenOrderInfo) {
        self.open_orders.insert(info.order_id, info);
    }

    /// Drop a resting order from tracking.
    pub fn untrack_order(&mut self, order_id: OrderId) {
        self.open_orders.remove(&order_id);
    }

    /// Cash available after order-margin reservation.
    #[must_use]
    pub fn free_cash_quote_minor(&self) -> i128 {
        self.cash_quote_minor - i128::try_from(self.order_margin_quote_minor).unwrap_or(i128::MAX)
    }
}

/// Margin summary for one account — the risk engine's view of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarginSummary {
    /// `cash + Σ unrealized PnL` across all positions.
    pub equity_quote_minor: i128,
    /// Portfolio maintenance margin (liquidation trigger).
    pub maintenance_quote_minor: u128,
    /// Portfolio initial margin (order acceptance threshold).
    pub initial_quote_minor: u128,
    /// Margin reserved by open orders.
    pub order_margin_quote_minor: u128,
}

impl MarginSummary {
    /// Equity net of order margin.
    #[must_use]
    pub fn free_equity(&self) -> i128 {
        self.equity_quote_minor - i128::try_from(self.order_margin_quote_minor).unwrap_or(i128::MAX)
    }

    /// Funds available for new positions:
    /// `equity − initial − order_margin`.
    #[must_use]
    pub fn available_quote_minor(&self) -> i128 {
        self.free_equity() - i128::try_from(self.initial_quote_minor).unwrap_or(i128::MAX)
    }
}

/// Health classification of a margin account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Health {
    /// Equity ≥ initial: full trading rights.
    Healthy,
    /// initial > equity ≥ maintenance: no new risk, orders restricted to
    /// risk-reducing only (industry-standard "margin call" state).
    Restricted,
    /// equity < maintenance: liquidatable by the risk engine.
    Liquidation,
}

impl Health {
    /// Classify a summary.
    #[must_use]
    pub fn classify(summary: &MarginSummary) -> Health {
        let maintenance = i128::try_from(summary.maintenance_quote_minor).unwrap_or(i128::MAX);
        let initial = i128::try_from(summary.initial_quote_minor).unwrap_or(i128::MAX);
        if summary.equity_quote_minor < maintenance {
            Health::Liquidation
        } else if summary.equity_quote_minor < initial {
            Health::Restricted
        } else {
            Health::Healthy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perp() -> Instrument {
        Instrument::Perp(poc_core::PerpMarket::default()) // $1 ticks, 0.001 BTC lots, 5dp
    }

    /// A math-friendly instrument: 1 lot = 1.0 base, so per-lot amounts
    /// equal per-base prices and position arithmetic is transparent.
    fn flat_qty() -> Instrument {
        let m = poc_core::PerpMarket {
            base_decimals: 0,
            lot_size_base_minor: 1,
            tick_size_quote_minor: 1,
            ..poc_core::PerpMarket::default()
        };
        Instrument::Perp(m)
    }

    #[test]
    fn fresh_position_anchors_entry() {
        let mut p = Position::flat("BTC-PERP");
        assert_eq!(p.apply_fill(&perp(), Side::Bid, 10, 6_000_000), 0);
        assert_eq!(p.signed_lots, 10);
        assert_eq!(p.avg_entry_quote_minor, 6_000_000);
    }

    #[test]
    fn extension_volume_weights_entry() {
        let mut p = Position::flat("X");
        let _ = p.apply_fill(&flat_qty(), Side::Bid, 10, 100);
        let _ = p.apply_fill(&flat_qty(), Side::Bid, 30, 200);
        assert_eq!(p.signed_lots, 40);
        assert_eq!(p.avg_entry_quote_minor, (10 * 100 + 30 * 200) / 40); // 175
    }

    #[test]
    fn reduction_realizes_pnl() {
        let mut p = Position::flat("X");
        let _ = p.apply_fill(&flat_qty(), Side::Bid, 10, 100);
        // Close 4 at 150: +4 * 50 = +200.
        assert_eq!(p.apply_fill(&flat_qty(), Side::Ask, 4, 150), 200);
        assert_eq!(p.signed_lots, 6);
        assert_eq!(p.avg_entry_quote_minor, 100, "entry unchanged on reduction");
        assert_eq!(p.realized_pnl_quote_minor, 200);
    }

    #[test]
    fn flip_carries_new_entry() {
        let mut p = Position::flat("X");
        let _ = p.apply_fill(&flat_qty(), Side::Bid, 10, 100);
        // Sell 15 at 160: close 10 (+600), open short 5 at 160.
        assert_eq!(p.apply_fill(&flat_qty(), Side::Ask, 15, 160), 600);
        assert_eq!(p.signed_lots, -5);
        assert_eq!(p.avg_entry_quote_minor, 160);
        // Close the short at 140: +5 * 20 = +100.
        assert_eq!(p.apply_fill(&flat_qty(), Side::Bid, 5, 140), 100);
        assert!(p.is_flat());
        assert_eq!(p.realized_pnl_quote_minor, 700);
        assert_eq!(p.avg_entry_quote_minor, 0);
    }

    #[test]
    fn short_side_pnl_signs() {
        let mut p = Position::flat("X");
        let _ = p.apply_fill(&flat_qty(), Side::Ask, 10, 200); // short at 200
                                                               // Buy back at 180: +10 * 20 = +200.
        assert_eq!(p.apply_fill(&flat_qty(), Side::Bid, 10, 180), 200);
        // Long at 100, sell at 80: -20 per lot.
        let _ = p.apply_fill(&flat_qty(), Side::Bid, 10, 100);
        assert_eq!(p.apply_fill(&flat_qty(), Side::Ask, 10, 80), -200);
    }

    #[test]
    fn unrealized_pnl_uses_linear_formula() {
        let inst = perp();
        let mut p = Position::flat("BTC-PERP");
        let _ = p.apply_fill(&perp(), Side::Bid, 1000, 6_000_000); // long 1.0 BTC at $60k
                                                                   // Mark $65k: +$5000 = +500_000 minor.
        assert_eq!(p.unrealized_pnl(&inst, 6_500_000), Some(500_000));
        // Short at $60k marked $65k: -$5000.
        let mut s = Position::flat("BTC-PERP");
        let _ = s.apply_fill(&perp(), Side::Ask, 1000, 6_000_000);
        assert_eq!(s.unrealized_pnl(&inst, 6_500_000), Some(-500_000));
    }

    #[test]
    fn account_apply_fill_settles_cash_and_prunes() {
        let mut a = MarginAccount::new(1, 1_000_000);
        let _ = a.apply_fill(&perp(), "BTC-PERP", Side::Bid, 100, 6_000_000);
        assert!(a.position("BTC-PERP").is_some());
        let realized = a.apply_fill(&perp(), "BTC-PERP", Side::Ask, 100, 6_100_000);
        // 100 lots × 0.001 BTC, $60k → $61k = $1.00 gain per lot = 100 minor
        // × 100 lots = $100.00 = 10_000 minor.
        assert_eq!(realized, 10_000);
        assert_eq!(a.cash_quote_minor, 1_010_000);
        assert!(a.position("BTC-PERP").is_none(), "flat positions pruned");
        assert_eq!(a.lots_of("BTC-PERP"), 0);
    }

    #[test]
    fn fees_and_funding_ledger() {
        let mut a = MarginAccount::new(1, 1_000);
        a.apply_fee(25);
        assert_eq!(a.cash_quote_minor, 975);
        assert_eq!(a.fees_paid_quote_minor, 25);
        a.apply_fee(-5); // rebate credits cash
        assert_eq!(a.cash_quote_minor, 980);
        assert_eq!(
            a.fees_paid_quote_minor, 25,
            "rebates don't reduce fees_paid"
        );
        a.apply_funding(40);
        assert_eq!(a.cash_quote_minor, 1020);
        assert_eq!(a.funding_pnl_quote_minor, -40, "positive credit = received");
        a.apply_reward(10);
        assert_eq!(a.cash_quote_minor, 1030);
    }

    #[test]
    fn free_cash_respects_order_margin() {
        let mut a = MarginAccount::new(1, 1_000);
        a.order_margin_quote_minor = 300;
        assert_eq!(a.free_cash_quote_minor(), 700);
        let s = MarginSummary {
            equity_quote_minor: 1_000,
            maintenance_quote_minor: 200,
            initial_quote_minor: 400,
            order_margin_quote_minor: 300,
        };
        assert_eq!(s.available_quote_minor(), 300);
        assert_eq!(s.free_equity(), 700);
    }

    #[test]
    fn health_classification() {
        let base = MarginSummary {
            equity_quote_minor: 1_000,
            maintenance_quote_minor: 300,
            initial_quote_minor: 500,
            order_margin_quote_minor: 0,
        };
        assert_eq!(Health::classify(&base), Health::Healthy);
        let restricted = MarginSummary {
            equity_quote_minor: 400,
            ..base
        };
        assert_eq!(Health::classify(&restricted), Health::Restricted);
        let liquidation = MarginSummary {
            equity_quote_minor: 299,
            ..base
        };
        assert_eq!(Health::classify(&liquidation), Health::Liquidation);
    }
}
