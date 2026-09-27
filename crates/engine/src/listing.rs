//! Strike auto-listing (G-34) and the everlasting strike-rebase ladder
//! (G-03).
//!
//! ## Auto-listing (Deribit / Paradex pattern, resolved for this engine)
//!
//! A [`ListingPolicy`] names, per underlying, a strike grid (spacing +
//! step count), a dated tenor ladder, and an everlasting strike ladder.
//! Every sweep:
//!
//! 1. the dated grid **rolls forward daily**: each tenor's expected expiry
//!    is the next UTC-midnight boundary at-or-beyond `now + tenor`; a
//!    missing expiry is listed with the full strike grid;
//! 2. strikes whose distance from spot exceeds the listing band are
//!    *added* at every live expiry (so the grid follows the market);
//! 3. far strikes with **zero open interest** are delisted (the delist
//!    band is wider than the listing band — hysteresis, no flapping);
//! 4. newly listed markets open in a short pre-open **auction** (G-12) so
//!    the first print is a fair uniform price instead of a latency race.
//!
//! ## Everlasting rebase (Everstrike pattern)
//!
//! Everlasting options keep their strikes near the money: when spot
//! drifts beyond `rebase_band_bps` from a listed everlasting strike, the
//! sweep cash-closes every position at the old market's mark and reopens
//! it at the new (near-the-money) strike's model mark — a *position
//! migration* that exactly conserves value and realized PnL — then cancels
//! resting orders on the old market and delists it. The concentrated
//! liquidity that an everlasting contract needs never fragments across
//! far strikes.
//!
//! Both stages are pure planners: they emit `MarketListed`,
//! `AuctionOpened`, `PositionMigrated`, `OptionDelisted`, and
//! `OrderClosed` events; every mutation happens in `apply_event`, so
//! replay is bit-exact.

use poc_core::{
    mul_div, Instrument, OptionKind, OptionMarket, OptionVariant, SubaccountId, TimestampMs,
};

use crate::engine::Engine;
use crate::event::{Event, OrderCloseReason};

/// UTC day boundary, ms.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Per-underlying auto-listing rules (G-34).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnderlyingListing {
    /// Underlying whose options are managed (e.g. `BTC`).
    pub base_symbol: String,
    /// Strike spacing, quote minor per base (e.g. 500_000 = $5,000).
    pub strike_spacing_quote_minor: u128,
    /// Grid steps each side of spot for a fresh listing.
    pub strike_steps: usize,
    /// Dated tenor ladder, ms (e.g. 7d, 30d); rolls forward daily.
    pub tenors_ms: Vec<u64>,
    /// Everlasting strikes kept around spot.
    pub everlasting_strikes: usize,
    /// List new strikes when the nearest is this far from spot, bps.
    pub listing_band_bps: u64,
    /// Delist zero-OI strikes beyond this band, bps (wider — hysteresis).
    pub delist_band_bps: u64,
    /// Everlasting rebase trigger, bps (G-03; 0 disables).
    pub rebase_band_bps: u64,
    /// Pre-open auction length for freshly listed markets (G-12; 0 = none).
    pub auction_ms: u64,
    /// Numeric template for generated option markets.
    pub template: OptionTemplate,
}

/// Numeric shape of auto-generated option markets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionTemplate {
    /// Quote minor units per major.
    pub quote_decimals: u32,
    /// Base minor units per major.
    pub base_decimals: u32,
    /// Premium tick, quote minor per base.
    pub tick_size_quote_minor: u128,
    /// Contract lot, base minor units.
    pub lot_size_base_minor: u128,
    /// Order placement band, bps.
    pub price_band_bps: u64,
    /// Largest single order, lots.
    pub max_order_lots: u64,
    /// Margin parameters.
    pub margin: poc_core::OptionMarginParams,
    /// Everlasting roll parameters.
    pub everlasting: poc_core::EverlastingParams,
    /// Anchor IV for the governed surface, bps (55% → 5_500).
    pub anchor_iv_bps: u64,
}

/// The default BTC-shaped template (mirrors the workspace's BTC option
/// conventions: $0.50 ticks, 0.01 BTC lots, hourly roll × 24).
#[must_use]
pub fn default_btc_template() -> OptionTemplate {
    OptionTemplate {
        quote_decimals: 2,
        base_decimals: 5,
        tick_size_quote_minor: 50,
        lot_size_base_minor: 1_000,
        price_band_bps: 2_000,
        max_order_lots: 10_000,
        margin: poc_core::OptionMarginParams::default(),
        everlasting: poc_core::EverlastingParams::default(),
        anchor_iv_bps: 5_500,
    }
}

impl UnderlyingListing {
    /// A BTC-shaped default policy: $5,000 spacing, 4+4 strikes, 7d/30d
    /// dated ladder, 2 everlasting strikes, 3% list band, 5% delist band,
    /// 10% rebase band, 5-minute opening auctions.
    #[must_use]
    pub fn btc_default() -> Self {
        Self {
            base_symbol: "BTC".into(),
            strike_spacing_quote_minor: 500_000,
            strike_steps: 4,
            tenors_ms: vec![7 * DAY_MS, 30 * DAY_MS],
            everlasting_strikes: 2,
            listing_band_bps: 300,
            delist_band_bps: 500,
            rebase_band_bps: 1_000,
            auction_ms: 5 * 60 * 1000,
            template: default_btc_template(),
        }
    }
}

/// Nearest grid strike to `spot` (spacing-aligned).
fn nearest_strike(spot: u128, spacing: u128) -> Option<u128> {
    if spacing == 0 {
        return None;
    }
    Some((spot + spacing / 2) / spacing * spacing)
}

/// Grid strikes around spot: `steps` each side, all positive, ascending.
fn grid_strikes(spot: u128, spacing: u128, steps: usize) -> Vec<u128> {
    let mut out = Vec::new();
    let Some(center) = nearest_strike(spot, spacing) else {
        return out;
    };
    let width = u128::try_from(steps).unwrap_or(u128::MAX);
    let mut k = width;
    while k > 0 {
        let low = center.saturating_sub(k.saturating_mul(spacing));
        if low > 0 {
            out.push(low);
        }
        k -= 1;
    }
    out.push(center);
    for i in 1..=width {
        out.push(center.saturating_add(i.saturating_mul(spacing)));
    }
    out
}

/// Next UTC-midnight boundary at-or-beyond `t`.
fn ceil_to_day(t: u64) -> u64 {
    t.saturating_add(DAY_MS - 1) / DAY_MS * DAY_MS
}

/// Dated option symbol: `BASE-YYYYMMDD-STRIKE-C|P` (strike in quote major).
#[must_use]
pub fn dated_symbol(base: &str, expiry: TimestampMs, strike: u128, quote_decimals: u32) -> String {
    let days = expiry / DAY_MS;
    // Civil-from-days (Howard Hinnant's algorithm, valid for the epoch range).
    let z = (days as i64) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let major = strike / 10_u128.saturating_pow(quote_decimals);
    format!("{base}-{year:04}{m:02}{d:02}-{major}")
}

/// Everlasting option symbol: `BASE-EVER-STRIKE` (strike in quote major).
#[must_use]
pub fn everlasting_symbol(base: &str, strike: u128, quote_decimals: u32) -> String {
    let major = strike / 10_u128.saturating_pow(quote_decimals);
    format!("{base}-EVER-{major}")
}

fn build_market(
    listing: &UnderlyingListing,
    kind: OptionKind,
    strike: u128,
    variant: OptionVariant,
    expiry: TimestampMs,
) -> OptionMarket {
    let t = &listing.template;
    let stem = match variant {
        OptionVariant::Dated => {
            dated_symbol(&listing.base_symbol, expiry, strike, t.quote_decimals)
        }
        OptionVariant::Everlasting => {
            everlasting_symbol(&listing.base_symbol, strike, t.quote_decimals)
        }
    };
    OptionMarket {
        symbol: format!("{stem}-{}", kind.letter()),
        base_symbol: listing.base_symbol.clone(),
        kind,
        strike_quote_minor: strike,
        expiry_ts_ms: expiry,
        variant,
        everlasting: t.everlasting,
        quote_decimals: t.quote_decimals,
        base_decimals: t.base_decimals,
        tick_size_quote_minor: t.tick_size_quote_minor,
        lot_size_base_minor: t.lot_size_base_minor,
        price_band_bps: t.price_band_bps,
        max_order_lots: t.max_order_lots,
        margin: t.margin,
    }
}

/// Open interest (absolute lots) on one symbol across all accounts.
fn open_interest(engine: &Engine, symbol: &str) -> u64 {
    engine
        .accounts
        .values()
        .map(|a| a.lots_of(symbol).unsigned_abs())
        .sum()
}

/// The sweep stage: auto-listing + everlasting rebase (G-34 + G-03).
pub(crate) fn plan_auto_listing(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    let mut events = Vec::new();
    for listing in &engine.config.listing.underlyings {
        let Some(spot) = engine
            .oracles
            .get(&listing.base_symbol)
            .and_then(|o| o.mark(now))
        else {
            continue;
        };
        let anchor = listing.template.anchor_iv_bps;

        // ---- Dated ladder: roll-forward + grid maintenance ----------------
        for tenor in &listing.tenors_ms {
            let expiry = ceil_to_day(now.saturating_add(*tenor));
            let grid = grid_strikes(
                spot,
                listing.strike_spacing_quote_minor,
                listing.strike_steps,
            );
            // Fresh expiry (first listing) seeds the FULL grid; an expiry
            // that already trades only adds strikes that left the band.
            let seeded = engine
                .instruments
                .values()
                .any(|i| matches!(i, Instrument::Option(m) if m.base_symbol == listing.base_symbol && m.expiry_ts_ms == expiry && m.variant == OptionVariant::Dated));
            for &strike in &grid {
                for kind in [OptionKind::Call, OptionKind::Put] {
                    let market = build_market(listing, kind, strike, OptionVariant::Dated, expiry);
                    let symbol = market.symbol.clone();
                    if engine.instruments.contains_key(&symbol) {
                        continue;
                    }
                    if seeded && !within_band(strike, spot, listing.listing_band_bps) {
                        continue;
                    }
                    events.extend(listing_events(market, now, listing.auction_ms, anchor));
                }
            }
        }

        // ---- Everlasting ladder -------------------------------------------
        let ever = grid_strikes(
            spot,
            listing.strike_spacing_quote_minor,
            listing.everlasting_strikes.saturating_sub(1),
        );
        for &strike in &ever {
            for kind in [OptionKind::Call, OptionKind::Put] {
                let market = build_market(listing, kind, strike, OptionVariant::Everlasting, 0);
                let symbol = market.symbol.clone();
                if engine.instruments.contains_key(&symbol) {
                    continue;
                }
                if !within_band(strike, spot, listing.listing_band_bps) {
                    continue;
                }
                events.extend(listing_events(market, now, listing.auction_ms, anchor));
            }
        }

        // ---- Delist far, empty strikes (hysteresis band) -------------------
        let dead: Vec<String> = engine
            .instruments
            .values()
            .filter_map(|i| match i {
                Instrument::Option(m) if m.base_symbol == listing.base_symbol => {
                    Some(m.symbol.clone())
                }
                _ => None,
            })
            .filter(|symbol| {
                let Some(Instrument::Option(m)) = engine.instruments.get(symbol) else {
                    return false;
                };
                let far = !within_band(m.strike_quote_minor, spot, listing.delist_band_bps);
                far && open_interest(engine, symbol) == 0
            })
            .collect();
        for symbol in dead {
            events.push(Event::OptionDelisted { symbol });
        }

        // ---- Everlasting rebase (G-03) --------------------------------------
        if listing.rebase_band_bps > 0 {
            events.extend(plan_rebase(engine, now, listing, spot, &ever));
        }
    }
    events
}

/// Whether `strike` is within `band_bps` of spot (relative to the strike).
fn within_band(strike: u128, spot: u128, band_bps: u64) -> bool {
    if strike == 0 {
        return false;
    }
    let diff = strike.abs_diff(spot);
    mul_div(diff, 10_000, strike, poc_core::Rounding::Floor)
        .is_some_and(|bps| bps <= u128::from(band_bps))
}

/// The events that list one market: registration, surface anchor, and an
/// optional pre-open auction (G-12).
fn listing_events(
    market: OptionMarket,
    now: TimestampMs,
    auction_ms: u64,
    anchor_iv_bps: u64,
) -> Vec<Event> {
    let mut events = vec![Event::MarketListed {
        instrument: Instrument::Option(market.clone()),
        anchor_iv_bps: Some(anchor_iv_bps),
    }];
    if auction_ms > 0 {
        events.push(Event::AuctionOpened {
            symbol: market.symbol,
            uncross_at: now.saturating_add(auction_ms),
            ts: now,
        });
    }
    events
}

/// The everlasting strike rebase (G-03): migrate positions from
/// out-of-band strikes to the nearest in-band grid strike.
fn plan_rebase(
    engine: &Engine,
    now: TimestampMs,
    listing: &UnderlyingListing,
    spot: u128,
    ever_grid: &[u128],
) -> Vec<Event> {
    let mut events = Vec::new();
    if ever_grid.is_empty() {
        return events;
    }

    // Out-of-band everlasting markets on this underlying.
    let stale: Vec<OptionMarket> = engine
        .instruments
        .values()
        .filter_map(|i| match i {
            Instrument::Option(m)
                if m.base_symbol == listing.base_symbol
                    && m.variant == OptionVariant::Everlasting
                    && !within_band(m.strike_quote_minor, spot, listing.rebase_band_bps) =>
            {
                Some(m.clone())
            }
            _ => None,
        })
        .collect();
    if stale.is_empty() {
        return events;
    }

    let Some(marks) = engine.build_marks(now) else {
        return events;
    };
    let Some(set) = marks.get(&listing.base_symbol) else {
        return events;
    };
    let mark_of = |symbol: &str| -> Option<u128> {
        match set.marks.get(symbol)? {
            poc_margin::Mark::Option {
                premium_quote_minor_per_base,
                ..
            } => Some(*premium_quote_minor_per_base),
            poc_margin::Mark::Perp { .. } => None,
        }
    };

    for old in stale {
        // Target: nearest grid strike to spot, same kind, fresh market if
        // needed (listed first so its book exists when positions migrate).
        let target_strike = ever_grid
            .iter()
            .copied()
            .min_by_key(|&k| k.abs_diff(spot))
            .unwrap_or(old.strike_quote_minor);
        let new_market = build_market(
            listing,
            old.kind,
            target_strike,
            OptionVariant::Everlasting,
            0,
        );
        if !engine.instruments.contains_key(&new_market.symbol) {
            events.extend(listing_events(
                new_market.clone(),
                now,
                listing.auction_ms,
                listing.template.anchor_iv_bps,
            ));
        }
        let close_price = mark_of(&old.symbol).unwrap_or(0);
        let open_price =
            new_strike_mark(engine, listing, &new_market, set.spot_quote_minor_per_base)
                .unwrap_or(0);

        // Migrate every holder: close at the old mark, open at the new
        // mark. Accounts in ascending id order (deterministic).
        let mut holders: Vec<(SubaccountId, i64)> = engine
            .accounts
            .iter()
            .map(|(&sub, acct)| (sub, acct.lots_of(&old.symbol)))
            .filter(|&(_, lots)| lots != 0)
            .collect();
        holders.sort_unstable_by_key(|(sub, _)| *sub);
        for (sub, lots) in holders {
            events.push(Event::PositionMigrated {
                subaccount: sub,
                from_symbol: old.symbol.clone(),
                to_symbol: new_market.symbol.clone(),
                signed_lots: lots,
                close_price_quote_minor: close_price,
                open_price_quote_minor: open_price,
                ts: now,
            });
        }

        // Pull resting orders on the old market, then delist it.
        if let Some(book) = engine.books.get(&old.symbol) {
            for resting in book.resting_orders() {
                events.push(Event::OrderClosed {
                    order_id: resting.order.id,
                    subaccount: resting.order.subaccount,
                    symbol: old.symbol.clone(),
                    order: resting.order.clone(),
                    reason: OrderCloseReason::Canceled,
                });
            }
        }
        events.push(Event::OptionDelisted {
            symbol: old.symbol.clone(),
        });
    }
    events
}

/// Model mark for a freshly generated everlasting market (the listing's
/// anchor IV through the same Black engine the marks use).
fn new_strike_mark(
    engine: &Engine,
    listing: &UnderlyingListing,
    market: &OptionMarket,
    spot_quote_minor: u128,
) -> Option<u128> {
    let tau_years = (u128::from(market.everlasting.interval_ms)
        .saturating_mul(u128::from(market.everlasting.maturity_multiple))
        as f64)
        / (365.0 * 24.0 * 3_600_000.0);
    let flavour = match market.kind {
        OptionKind::Call => poc_margin::Flavour::Call,
        OptionKind::Put => poc_margin::Flavour::Put,
    };
    let premium = poc_margin::OptionAnalytics::price(
        flavour,
        spot_quote_minor as f64,
        market.strike_quote_minor as f64,
        tau_years,
        (listing.template.anchor_iv_bps as f64) / 10_000.0,
        engine.config.risk_free_rate,
    );
    if premium <= 0.0 {
        return None;
    }
    Some((premium + 0.5) as u64 as u128)
}

// ----------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_centers_on_spot() {
        // Spot 1,004,000 at 500,000 spacing: nearest = 1,000,000.
        let grid = grid_strikes(1_004_000, 500_000, 2);
        assert_eq!(grid, vec![500_000, 1_000_000, 1_500_000, 2_000_000]);
    }

    #[test]
    fn grid_low_clamps_at_spacing() {
        let grid = grid_strikes(1_004_000, 500_000, 8);
        assert!(grid.iter().all(|&s| s > 0));
        assert_eq!(*grid.first().unwrap(), 500_000);
        assert_eq!(grid.len(), 10); // 1 below + center + 8 above
    }

    #[test]
    fn grid_below_one_step() {
        let grid = grid_strikes(400_000, 500_000, 2);
        // Nearest = 500,000; below: only 0 (dropped) then 500_000 up.
        assert_eq!(grid, vec![500_000, 1_000_000, 1_500_000]);
    }

    #[test]
    fn day_ceil_math() {
        assert_eq!(ceil_to_day(0), 0);
        assert_eq!(ceil_to_day(1), DAY_MS);
        assert_eq!(ceil_to_day(DAY_MS), DAY_MS);
        assert_eq!(ceil_to_day(DAY_MS + 1), 2 * DAY_MS);
    }

    #[test]
    fn symbol_dates_encode() {
        // 2026-09-27 is day 20,723 since epoch; $80,000 at 2dp is
        // 8_000_000 quote minor.
        let day = 20_723_u64 * DAY_MS;
        assert_eq!(dated_symbol("BTC", day, 8_000_000, 2), "BTC-20260927-80000");
        assert_eq!(everlasting_symbol("BTC", 8_000_000, 2), "BTC-EVER-80000");
    }

    #[test]
    fn band_checks() {
        assert!(within_band(100_000, 102_500, 300)); // 2.5% off, 3% band
        assert!(!within_band(100_000, 104_000, 300)); // 4% off
        assert!(!within_band(0, 100, 10_000));
    }

    #[test]
    fn market_symbols_carry_kind() {
        let listing = UnderlyingListing::btc_default();
        let m = build_market(
            &listing,
            OptionKind::Put,
            8_000_000,
            OptionVariant::Dated,
            20_723 * DAY_MS,
        );
        assert_eq!(m.symbol, "BTC-20260927-80000-P");
        let e = build_market(
            &listing,
            OptionKind::Call,
            8_000_000,
            OptionVariant::Everlasting,
            0,
        );
        assert_eq!(e.symbol, "BTC-EVER-80000-C");
        assert_eq!(e.variant, OptionVariant::Everlasting);
    }
}
