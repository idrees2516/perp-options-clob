//! Scenario-based portfolio margin (SFPM).
//!
//! Maintenance margin is the **worst-case loss of the whole portfolio**
//! under a grid of standardized shocks, plus a short-option tail floor —
//! see the crate docs for the full rationale and competitor mapping.

use std::collections::BTreeMap;

use poc_core::{Instrument, OptionKind, Symbol};

use crate::account::{MarginAccount, MarginSummary};
use crate::blackscholes::{Flavour, OptionLegView};

/// Per-underlying margin parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnderlyingMarginParams {
    /// Spot scanning range in bps (default: the perp maintenance ratio —
    /// the venue's own liquidation-defining move).
    pub scan_range_bps: u64,
    /// Initial margin uplift over maintenance, in percent (140 = 1.4×,
    /// the Derive V3 SFPM uplift).
    pub initial_multiplier_pct: u64,
    /// Relative implied-vol shift applied in scenarios, percent
    /// (25 = ±25% of current IV).
    pub vol_shift_pct: u64,
    /// Risk-free rate used for discounting (analytics only).
    pub risk_free_rate: f64,
}

impl Default for UnderlyingMarginParams {
    fn default() -> Self {
        Self {
            scan_range_bps: 375, // 3.75% — BTC perp maintenance default
            initial_multiplier_pct: 140,
            vol_shift_pct: 25,
            risk_free_rate: 0.0,
        }
    }
}

/// Mark data for one instrument, supplied by the engine each margin pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mark {
    /// Perp mark: index-like price, quote minor per `1.0` base.
    Perp {
        /// Mark price.
        mark_quote_minor_per_base: u128,
    },
    /// Option mark: premium plus analytics state.
    Option {
        /// Mark premium, quote minor per `1.0` base.
        premium_quote_minor_per_base: u128,
        /// Implied volatility of the mark.
        iv: f64,
        /// Time to expiry in years.
        tau_years: f64,
    },
}

/// Marks for all instruments sharing one underlying, plus the spot.
#[derive(Debug, Clone, PartialEq)]
pub struct MarkSet {
    /// Underlying base symbol (e.g. `"BTC"`).
    pub base_symbol: String,
    /// Oracle spot, quote minor per `1.0` base.
    pub spot_quote_minor_per_base: u128,
    /// Marks by instrument symbol.
    pub marks: BTreeMap<Symbol, Mark>,
}

impl MarkSet {
    /// An empty mark set for `base_symbol` at `spot`.
    #[must_use]
    pub fn new(base_symbol: impl Into<String>, spot_quote_minor_per_base: u128) -> Self {
        Self {
            base_symbol: base_symbol.into(),
            spot_quote_minor_per_base,
            marks: BTreeMap::new(),
        }
    }

    /// Attach an instrument mark.
    pub fn with_mark(mut self, symbol: impl Into<Symbol>, mark: Mark) -> Self {
        self.marks.insert(symbol.into(), mark);
        self
    }
}

/// A flattened, analytics-ready leg used by the scenario scanner.
#[derive(Debug, Clone, Copy)]
enum Leg {
    Perp {
        /// Signed exposure in base units (lots × lot_size / 10^base_decimals).
        signed_base: f64,
    },
    Option {
        signed_base: f64,
        view: OptionLegView,
        /// Mark premium (quote minor per base).
        mark: f64,
        /// Net-short flat quantity in base units (0 for net-long legs).
        short_base: f64,
        /// Short-option minimum charge rate (bps of spot).
        somc_bps: u64,
    },
}

/// The SFPM calculator.
///
/// Stateless by design: the engine supplies accounts, instruments, and
/// marks; the calculator returns margin numbers. Two identical calls
/// return identical results (determinism), and no ledger mutation ever
/// flows back through it.
pub struct PortfolioMarginEngine {
    params: BTreeMap<String, UnderlyingMarginParams>,
}

impl Default for PortfolioMarginEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PortfolioMarginEngine {
    /// An engine with default parameters for every underlying.
    #[must_use]
    pub fn new() -> Self {
        Self {
            params: BTreeMap::new(),
        }
    }

    /// Override parameters for one underlying.
    pub fn set_params(&mut self, base_symbol: impl Into<String>, params: UnderlyingMarginParams) {
        self.params.insert(base_symbol.into(), params);
    }

    /// Parameters for an underlying (defaults when unset).
    #[must_use]
    pub fn params_for(&self, base_symbol: &str) -> UnderlyingMarginParams {
        self.params.get(base_symbol).copied().unwrap_or_default()
    }

    /// Full margin summary for one account.
    ///
    /// Returns `None` when a held position lacks a mark (the engine must
    /// halt that account rather than margin it on stale data — fail-safe).
    pub fn margin_summary(
        &self,
        account: &MarginAccount,
        instruments: &BTreeMap<Symbol, Instrument>,
        marks: &BTreeMap<String, MarkSet>,
    ) -> Option<MarginSummary> {
        self.margin_summary_ex(account, instruments, marks, &BTreeMap::new())
    }

    /// [`Self::margin_summary`] with spot-hedge-aware collateral (G-20):
    /// `spot_exposures` maps underlying → signed base units of
    /// oracle-priced collateral held by the account. Each exposure enters
    /// the scenario scan as a linear spot leg, so BTC held against a short
    /// BTC call nets the call's scenario gain instead of sitting outside
    /// the grid as an unshocked credit — the Derive V3
    /// hedge-with-collateral pattern.
    ///
    /// Equity still credits the *haircut* value (the engine's addition);
    /// the scanner shocks the *full* balance — the haircut stays
    /// conservative on both sides of the grid.
    pub fn margin_summary_ex(
        &self,
        account: &MarginAccount,
        instruments: &BTreeMap<Symbol, Instrument>,
        marks: &BTreeMap<String, MarkSet>,
        spot_exposures: &BTreeMap<String, f64>,
    ) -> Option<MarginSummary> {
        // Equity: cash + unrealized PnL of every position.
        let mut equity = account.cash_quote_minor;
        for (symbol, position) in &account.positions {
            let instrument = instruments.get(symbol)?;
            let base = marks.get(instrument.base_symbol())?;
            let mark = base.marks.get(symbol)?;
            let mark_price = match mark {
                Mark::Perp {
                    mark_quote_minor_per_base,
                } => *mark_quote_minor_per_base,
                Mark::Option {
                    premium_quote_minor_per_base,
                    ..
                } => *premium_quote_minor_per_base,
            };
            equity =
                equity.saturating_add(position.unrealized_pnl(instrument, mark_price).unwrap_or(0));
        }

        // Margin: scenario scan per underlying, summed (no cross-underlying
        // offsetting — SPAN treats commodities additively).
        let mut maintenance: u128 = 0;
        let mut initial: u128 = 0;
        for mark_set in marks.values() {
            let exposure = spot_exposures
                .get(&mark_set.base_symbol)
                .copied()
                .unwrap_or(0.0);
            if let Some(m) = self.scan_underlying_ex(account, instruments, mark_set, exposure) {
                maintenance = maintenance.saturating_add(m.maintenance_quote_minor);
                initial = initial.saturating_add(m.initial_quote_minor);
            }
        }

        Some(MarginSummary {
            equity_quote_minor: equity,
            maintenance_quote_minor: maintenance,
            initial_quote_minor: initial,
            order_margin_quote_minor: account.order_margin_quote_minor,
        })
    }

    /// Maintenance + initial margin for a single underlying's slice of the
    /// account.
    ///
    /// Returns `None` if a position on this underlying lacks mark data.
    #[must_use]
    pub fn scan_underlying(
        &self,
        account: &MarginAccount,
        instruments: &BTreeMap<Symbol, Instrument>,
        mark_set: &MarkSet,
    ) -> Option<UnderlyingMargin> {
        self.scan_underlying_ex(account, instruments, mark_set, 0.0)
    }

    /// [] with an extra signed spot exposure (G-20):
    /// oracle-priced collateral in this underlying, in base units. It
    /// becomes a linear perp-equivalent leg in every scenario.
    pub fn scan_underlying_ex(
        &self,
        account: &MarginAccount,
        instruments: &BTreeMap<Symbol, Instrument>,
        mark_set: &MarkSet,
        spot_exposure_base: f64,
    ) -> Option<UnderlyingMargin> {
        let params = self.params_for(&mark_set.base_symbol);
        let spot = mark_set.spot_quote_minor_per_base as f64;

        // Flatten positions into analytics legs.
        let mut legs: Vec<Leg> = Vec::new();
        if spot_exposure_base != 0.0 {
            legs.push(Leg::Perp {
                signed_base: spot_exposure_base,
            });
        }
        for (symbol, position) in &account.positions {
            if position.signed_lots == 0 {
                continue;
            }
            let instrument = instruments.get(symbol)?;
            if instrument.base_symbol() != mark_set.base_symbol {
                continue;
            }
            let mark = mark_set.marks.get(symbol)?;
            let base_decimals = instrument.base_decimals();
            let base_per_lot = instrument.lot_size() as f64 / 10_f64.powi(base_decimals as i32);
            let signed_base = position.signed_lots as f64 * base_per_lot;

            match (instrument, mark) {
                (Instrument::Perp(_), Mark::Perp { .. }) => {
                    legs.push(Leg::Perp { signed_base });
                }
                (
                    Instrument::Option(market),
                    Mark::Option {
                        premium_quote_minor_per_base,
                        iv,
                        tau_years,
                    },
                ) => {
                    legs.push(Leg::Option {
                        signed_base,
                        view: OptionLegView {
                            spot,
                            strike: market.strike_quote_minor as f64,
                            tau_years: *tau_years,
                            iv: *iv,
                            rate: params.risk_free_rate,
                            flavour: match market.kind {
                                OptionKind::Call => Flavour::Call,
                                OptionKind::Put => Flavour::Put,
                            },
                            // American legs reprice through the full
                            // early-exercise premium: under every grid
                            // shock the BAW value bounds the assignment
                            // payout (assignment settles at TWAP
                            // intrinsic, never above the American mark),
                            // so the scenario worst-case is a sound
                            // upper bound on assignment risk without a
                            // separate add-on charge.
                            american: market.is_american(),
                        },
                        mark: *premium_quote_minor_per_base as f64,
                        short_base: if position.signed_lots < 0 {
                            -signed_base
                        } else {
                            0.0
                        },
                        somc_bps: market.margin.short_option_min_bps,
                    });
                }
                _ => return None, // instrument/mark kind mismatch: fail-safe
            }
        }

        if legs.is_empty() {
            return Some(UnderlyingMargin::ZERO);
        }

        // Scenario grid: scaled spot shocks × vol shifts.
        let scan = params.scan_range_bps as f64 / 10_000.0;
        let vol_shift = params.vol_shift_pct as f64 / 100.0;
        let mut worst_loss: f64 = 0.0;
        for &spot_scale in &[-1.0_f64, -0.5, -0.25, 0.25, 0.5, 1.0] {
            for &vol_scale in &[-1.0_f64, 0.0, 1.0] {
                let spot_shock = scan * spot_scale;
                let vol_shock = vol_shift * vol_scale;
                let mut pnl: f64 = 0.0;
                for leg in &legs {
                    match *leg {
                        Leg::Perp { signed_base, .. } => {
                            // Linear leg: pnl = qty × (S' − S).
                            pnl += signed_base * (spot_shock * spot);
                        }
                        Leg::Option {
                            signed_base,
                            view,
                            mark,
                            ..
                        } => {
                            let repriced = view.reprice(spot_shock, vol_shock);
                            pnl += signed_base * (repriced - mark);
                        }
                    }
                }
                let loss = -pnl;
                if loss > worst_loss {
                    worst_loss = loss;
                }
            }
        }

        // Short-option minimum charge: net short option legs pay a floor
        // proportional to spot — wings move further than any scan range.
        let mut somc = 0.0_f64;
        for leg in &legs {
            if let Leg::Option {
                short_base,
                somc_bps,
                ..
            } = *leg
            {
                if short_base > 0.0 {
                    somc += short_base * spot * (somc_bps as f64 / 10_000.0);
                }
            }
        }

        let total = (worst_loss + somc).max(0.0);
        let maintenance = half_up_u128(total);
        let initial = apply_pct(maintenance, params.initial_multiplier_pct);
        Some(UnderlyingMargin {
            maintenance_quote_minor: maintenance,
            initial_quote_minor: initial,
        })
    }

    /// Initial margin for a *hypothetical* portfolio state, used by
    /// pre-trade checks: the engine clones the account, applies the
    /// candidate fill, and calls this.
    #[must_use]
    pub fn initial_margin(
        &self,
        account: &MarginAccount,
        instruments: &BTreeMap<Symbol, Instrument>,
        marks: &BTreeMap<String, MarkSet>,
    ) -> u128 {
        self.margin_summary(account, instruments, marks)
            .map_or(0, |s| s.initial_quote_minor)
    }
}

/// Per-underlying margin result of a scenario scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnderlyingMargin {
    /// Worst-case portfolio loss across the scenario grid, plus SOMC.
    pub maintenance_quote_minor: u128,
    /// Maintenance scaled by the initial multiplier.
    pub initial_quote_minor: u128,
}

impl UnderlyingMargin {
    /// Zero margin (no legs).
    pub const ZERO: UnderlyingMargin = UnderlyingMargin {
        maintenance_quote_minor: 0,
        initial_quote_minor: 0,
    };
}

/// `x * pct / 100` in integer money, half-up.
fn apply_pct(x: u128, pct: u64) -> u128 {
    if pct == 100 {
        return x;
    }
    x.saturating_mul(u128::from(pct)).saturating_add(50) / 100
}

/// f64 → u128 with half-up rounding, clamped at zero (risk figures only).
fn half_up_u128(x: f64) -> u128 {
    if x <= 0.0 {
        return 0;
    }
    let rounded = (x + 0.5).floor();
    if rounded >= u128::MAX as f64 {
        u128::MAX
    } else {
        rounded as u128
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::Position;
    use poc_core::{OptionMarket, PerpMarket, Side};

    fn perp_instrument() -> (Symbol, Instrument) {
        let m = PerpMarket::default(); // $1 ticks, 0.001 lots, 5dp, mmr 375bps
        ("BTC-PERP".to_string(), Instrument::Perp(m))
    }

    fn option_instrument() -> (Symbol, Instrument) {
        let m = OptionMarket::default(); // 80k call, 0.01 lots, somc 5%
        ("BTC-80000-C".to_string(), Instrument::Option(m))
    }

    fn marks_btc(spot: u128, iv: f64, tau: f64) -> BTreeMap<String, MarkSet> {
        let mut set = MarkSet::new("BTC", spot);
        set = set.with_mark(
            "BTC-PERP",
            Mark::Perp {
                mark_quote_minor_per_base: spot,
            },
        );
        set = set.with_mark(
            "BTC-80000-C",
            Mark::Option {
                premium_quote_minor_per_base: 2_000_000, // $20k premium placeholder
                iv,
                tau_years: tau,
            },
        );
        let mut map = BTreeMap::new();
        map.insert("BTC".to_string(), set);
        map
    }

    fn inst() -> Instrument {
        perp_instrument().1
    }

    fn instruments() -> BTreeMap<Symbol, Instrument> {
        let mut m = BTreeMap::new();
        let (p, pi) = perp_instrument();
        let (o, oi) = option_instrument();
        m.insert(p, pi);
        m.insert(o, oi);
        m
    }

    #[test]
    fn spot_collateral_nets_short_call_risk_inside_the_grid() {
        // G-20: a short call hedged with held BTC must scan cheaper than
        // the same naked short — the collateral's scenario gain nets the
        // option's delta loss leg by leg. The scan range is widened past
        // the SOMC so the scenario component dominates the floor and the
        // hedge is observable in maintenance.
        let mut engine = PortfolioMarginEngine::new();
        engine.set_params(
            "BTC",
            UnderlyingMarginParams {
                scan_range_bps: 1_000, // 10% — scenario dominates SOMC
                ..UnderlyingMarginParams::default()
            },
        );

        let mut naked = MarginAccount::new(1, 10_000_000);
        *naked.position_mut("BTC-80000-C") = Position {
            symbol: "BTC-80000-C".into(),
            signed_lots: -1, // short 1 lot (0.01 base)
            avg_entry_quote_minor: 2_000_000,
            realized_pnl_quote_minor: 0,
        };
        let hedged = naked.clone();
        let mut marks = marks_btc(8_000_000, 0.55, 0.25);
        // A realistic ATM premium (BS at 55% vol, 0.25y): the
        // placeholder in  is for equity math only.
        if let Some(set) = marks.get_mut("BTC") {
            set.marks.insert(
                "BTC-80000-C".into(),
                Mark::Option {
                    premium_quote_minor_per_base: 880_000,
                    iv: 0.55,
                    tau_years: 0.25,
                },
            );
        }

        let naked_summary = engine
            .margin_summary(&naked, &instruments(), &marks)
            .unwrap();

        // Delta hedge: 0.005 BTC held as collateral (long spot against
        // the short call's +0.5 delta).
        let mut exposures = BTreeMap::new();
        exposures.insert("BTC".to_string(), 0.005_f64);
        let hedged_summary = engine
            .margin_summary_ex(&hedged, &instruments(), &marks, &exposures)
            .unwrap();

        assert!(
            hedged_summary.maintenance_quote_minor < naked_summary.maintenance_quote_minor,
            "hedged scan {} must beat naked scan {}",
            hedged_summary.maintenance_quote_minor,
            naked_summary.maintenance_quote_minor
        );

        // The same exposure on a long perp INCREASES risk (same-side
        // stacking, no free lunch): sanity that the leg is not inverted.
        let mut long_perp = MarginAccount::new(2, 10_000_000);
        *long_perp.position_mut("BTC-PERP") = Position {
            symbol: "BTC-PERP".into(),
            signed_lots: 1,
            avg_entry_quote_minor: 8_000_000,
            realized_pnl_quote_minor: 0,
        };
        let base = engine
            .margin_summary(&long_perp, &instruments(), &marks)
            .unwrap();
        let stacked = engine
            .margin_summary_ex(&long_perp, &instruments(), &marks, &exposures)
            .unwrap();
        assert!(
            stacked.maintenance_quote_minor > base.maintenance_quote_minor,
            "same-side exposure must increase the requirement"
        );
    }

    #[test]
    fn flat_account_has_zero_margin_full_equity() {
        let account = MarginAccount::new(1, 1_000_000);
        let engine = PortfolioMarginEngine::new();
        let s = engine
            .margin_summary(&account, &instruments(), &marks_btc(8_000_000, 0.5, 0.25))
            .unwrap();
        assert_eq!(s.equity_quote_minor, 1_000_000);
        assert_eq!(s.maintenance_quote_minor, 0);
        assert_eq!(s.initial_quote_minor, 0);
        assert_eq!(s.available_quote_minor(), 1_000_000);
    }

    #[test]
    fn naked_perp_pays_scan_range() {
        // Long 1.0 BTC at spot $80,000 (mark = entry, uPnL = 0).
        let mut account = MarginAccount::new(1, 10_000_000);
        let mut pos = Position::flat("BTC-PERP");
        let _ = pos.apply_fill(&inst(), Side::Bid, 1000, 8_000_000); // 1000 lots × 0.001 BTC
        account.positions.insert("BTC-PERP".into(), pos);

        let engine = PortfolioMarginEngine::new();
        let s = engine
            .margin_summary(&account, &instruments(), &marks_btc(8_000_000, 0.5, 0.25))
            .unwrap();

        // notional = $80,000.00 = 8_000_000 minor. scan = 3.75%.
        let expected_maint = 8_000_000_u128 * 375 / 10_000; // 300_000
        assert_eq!(s.maintenance_quote_minor, expected_maint);
        // initial = 1.4 × maintenance.
        let expected_init = expected_maint * 140 / 100; // 420_000
        assert_eq!(s.initial_quote_minor, expected_init);
        assert_eq!(s.equity_quote_minor, 10_000_000);
    }

    #[test]
    fn short_perp_symmetric() {
        let mut account = MarginAccount::new(1, 10_000_000);
        let mut pos = Position::flat("BTC-PERP");
        let _ = pos.apply_fill(&inst(), Side::Ask, 1000, 8_000_000);
        account.positions.insert("BTC-PERP".into(), pos);

        let engine = PortfolioMarginEngine::new();
        let s = engine
            .margin_summary(&account, &instruments(), &marks_btc(8_000_000, 0.5, 0.25))
            .unwrap();
        assert_eq!(s.maintenance_quote_minor, 8_000_000 * 375 / 10_000);
    }

    #[test]
    fn hedged_book_pays_less_than_sum_of_legs() {
        // Short 1.0 BTC-equivalent of deep-ITM calls (delta ≈ 1) hedged
        // with long 1.0 BTC perp: scenario losses offset, leaving mostly
        // the SOMC floor on the short option.
        let mut instruments = instruments();
        // Deep-ITM call: strike $50k, spot $80k.
        if let Some(Instrument::Option(m)) = instruments.get_mut("BTC-80000-C") {
            m.strike_quote_minor = 5_000_000;
        }
        let mut marks = marks_btc(8_000_000, 0.5, 0.25);
        marks.get_mut("BTC").unwrap().marks.insert(
            "BTC-80000-C".into(),
            Mark::Option {
                premium_quote_minor_per_base: 3_000_000, // ≈ intrinsic $30k
                iv: 0.5,
                tau_years: 0.25,
            },
        );

        let mut account = MarginAccount::new(1, 10_000_000);
        let mut perp_pos = Position::flat("BTC-PERP");
        let _ = perp_pos.apply_fill(&inst(), Side::Bid, 1000, 8_000_000);
        account.positions.insert("BTC-PERP".into(), perp_pos);
        let mut opt_pos = Position::flat("BTC-80000-C");
        let _ = opt_pos.apply_fill(&inst(), Side::Ask, 100, 3_000_000); // short 100 lots = 1.0 BTC
        account.positions.insert("BTC-80000-C".into(), opt_pos);

        let engine = PortfolioMarginEngine::new();
        let s = engine
            .margin_summary(&account, &instruments, &marks)
            .unwrap();

        // Sum-of-legs floor would be perp scan (300_000) + SOMC on the
        // short call (5% × spot × 1.0 BTC = 400_000). The hedged scan is
        // far below that: the deep-ITM call offsets the perp almost
        // exactly, leaving mostly the SOMC + small residuals.
        assert!(
            s.maintenance_quote_minor < 300_000 + 400_000,
            "hedge must reduce margin, got {}",
            s.maintenance_quote_minor
        );
        assert!(
            s.maintenance_quote_minor >= 400_000,
            "SOMC floor binds on the short option: got {}",
            s.maintenance_quote_minor
        );
    }

    #[test]
    fn long_option_bounded_by_premium() {
        // Long 1.0 BTC of options: worst case is losing (most of) the
        // premium, never more.
        let mut account = MarginAccount::new(1, 10_000_000);
        let mut pos = Position::flat("BTC-80000-C");
        let _ = pos.apply_fill(&inst(), Side::Bid, 100, 2_000_000); // entry = $20k premium
        account.positions.insert("BTC-80000-C".into(), pos);

        let engine = PortfolioMarginEngine::new();
        let s = engine
            .margin_summary(&account, &instruments(), &marks_btc(8_000_000, 0.5, 0.25))
            .unwrap();

        // Mark premium $20k × 1.0 BTC; scenario can lose at most ~all of it
        // (premium is already sunk cost in uPnL terms — margin guards
        // further adverse movement beyond entry mark).
        assert!(
            s.maintenance_quote_minor <= 2_000_000,
            "long option margin bounded, got {}",
            s.maintenance_quote_minor
        );
    }

    #[test]
    fn missing_mark_fails_safe() {
        let mut account = MarginAccount::new(1, 1_000_000);
        let mut pos = Position::flat("BTC-PERP");
        let _ = pos.apply_fill(&inst(), Side::Bid, 10, 8_000_000);
        account.positions.insert("BTC-PERP".into(), pos);

        let engine = PortfolioMarginEngine::new();
        // Empty marks for BTC: cannot margin the position.
        assert!(engine
            .margin_summary(&account, &instruments(), &BTreeMap::new())
            .is_none());
    }

    #[test]
    fn equity_aggregates_unrealized_pnl() {
        let mut account = MarginAccount::new(1, 500_000);
        let mut pos = Position::flat("BTC-PERP");
        let _ = pos.apply_fill(&inst(), Side::Bid, 1000, 8_000_000); // long 1.0 BTC at $80k
        account.positions.insert("BTC-PERP".into(), pos);

        let engine = PortfolioMarginEngine::new();
        // Mark moves to $81k: uPnL = +100_000.
        let s = engine
            .margin_summary(&account, &instruments(), &marks_btc(8_100_000, 0.5, 0.25))
            .unwrap();
        assert_eq!(s.equity_quote_minor, 600_000);
        // Mark down to $78k: uPnL = −200_000.
        let s = engine
            .margin_summary(&account, &instruments(), &marks_btc(7_800_000, 0.5, 0.25))
            .unwrap();
        assert_eq!(s.equity_quote_minor, 300_000);
    }

    #[test]
    fn custom_scan_params() {
        let mut account = MarginAccount::new(1, 10_000_000);
        let mut pos = Position::flat("BTC-PERP");
        let _ = pos.apply_fill(&inst(), Side::Bid, 1000, 8_000_000);
        account.positions.insert("BTC-PERP".into(), pos);

        let mut engine = PortfolioMarginEngine::new();
        engine.set_params(
            "BTC",
            UnderlyingMarginParams {
                scan_range_bps: 1000, // 10%
                ..UnderlyingMarginParams::default()
            },
        );
        let s = engine
            .margin_summary(&account, &instruments(), &marks_btc(8_000_000, 0.5, 0.25))
            .unwrap();
        assert_eq!(s.maintenance_quote_minor, 8_000_000 * 1000 / 10_000);
    }

    #[test]
    fn helpers_round_half_up() {
        assert_eq!(apply_pct(300_000, 140), 420_000);
        assert_eq!(apply_pct(7, 140), 10); // 9.8 -> 10
        assert_eq!(apply_pct(100, 100), 100);
        assert_eq!(half_up_u128(-5.0), 0);
        assert_eq!(half_up_u128(0.4), 0);
        assert_eq!(half_up_u128(0.6), 1);
    }
}
