//! # poc-volsurface
//!
//! A live, governed volatility surface for option mark IV (gap **G-04**).
//!
//! Mark IVs are the exchange's risk truth, so they may never be a raw
//! function of a possibly-thin, possibly-manipulated order book. This crate
//! implements the three-stage design adopted in `docs/DESIGN_SOURCES.md`
//! §4 (the Paradex mark-price pattern):
//!
//! 1. **Anchor** — every market registers with a configured anchor IV
//!    (bps). The mark starts there, and the anchor is the fail-safe the
//!    mark decays back to whenever the book goes quiet.
//! 2. **Blend** — each usable book observation (two-sided, resting at or
//!    above the touch minimum) is inverted through poc-margin's Black-76
//!    implied-volatility solver, checked against the anchor sanity band,
//!    and EWMA-blended into the mark with integer arithmetic whose update
//!    magnitude is floored, so the mark approaches the book monotonically
//!    and can never overshoot it.
//! 3. **Govern** — a per-interval sweep rate-limits how fast the mark may
//!    chase the last accepted book IV (at most `max_move_bps_per_sweep`
//!    bps of the *current* mark per sweep) and decays a stale mark back
//!    toward the anchor one clamped step at a time.
//!
//! ## Determinism
//!
//! The surface is a pure function of (config, registrations, observation
//! sequence, sweep times): no clocks, no randomness, and symbol iteration
//! follows the sorted order of a [`BTreeMap`]. Two surfaces fed the same
//! inputs hold identical state — asserted by test.
//!
//! ## Numeric conventions
//!
//! All parameters and all state are integers (bps, permille, ms, lots).
//! f64 exists only inside the Black-76 inversion boundary: premiums enter
//! as quote-minor integers, are cast to the solver's analytics units
//! exactly the way poc-margin's portfolio scanner builds its leg views,
//! and the solved implied-volatility fraction leaves as a bps integer.

use std::collections::BTreeMap;
use std::fmt;

use poc_core::num::mul_div_floor;
use poc_core::{Symbol, TimestampMs};
use poc_margin::{AmericanAnalytics, Flavour, OptionAnalytics};

/// Milliseconds per year for the ms→years conversion (365-day convention,
/// mirroring poc-engine exactly so surface IVs and engine marks share one
/// time base).
const MS_PER_YEAR: f64 = 365.0 * 24.0 * 3_600_000.0;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Configuration errors raised by [`SurfaceConfig::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceError {
    /// `sanity_band_bps` is zero: a zero-width band rejects every book
    /// observation, freezing the surface at its anchor.
    ZeroSanityBand,
    /// `ewma_alpha_permille` exceeds 1000: a blend weight above 1.0 would
    /// overshoot the book IV.
    EwmaAlphaAboveOne,
    /// `max_move_bps_per_sweep` is zero: governance would freeze the mark.
    ZeroMaxMove,
    /// `max_move_bps_per_sweep` exceeds 10 000 (100% of the mark): a sweep
    /// could jump straight to its target, defeating the rate limit.
    MaxMoveAboveOne,
    /// `staleness_ms` is zero: every sweep after the observation instant
    /// would treat the book as stale.
    ZeroStaleness,
    /// `min_touch_lots` is zero: empty price levels would move the mark.
    ZeroMinTouch,
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SurfaceError::ZeroSanityBand => write!(f, "sanity_band_bps must be positive"),
            SurfaceError::EwmaAlphaAboveOne => write!(f, "ewma_alpha_permille must be <= 1000"),
            SurfaceError::ZeroMaxMove => write!(f, "max_move_bps_per_sweep must be positive"),
            SurfaceError::MaxMoveAboveOne => write!(f, "max_move_bps_per_sweep must be <= 10000"),
            SurfaceError::ZeroStaleness => write!(f, "staleness_ms must be positive"),
            SurfaceError::ZeroMinTouch => write!(f, "min_touch_lots must be positive"),
        }
    }
}

impl std::error::Error for SurfaceError {}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Per-market surface governance parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceConfig {
    /// Allowed deviation of book-implied IV from the anchor IV, in bps of
    /// the anchor (e.g. 2500 = book IV must be within 25% of anchor).
    pub sanity_band_bps: u64,
    /// EWMA smoothing weight (0..=1000 permille) for blending book IV
    /// into the mark IV each observation. e.g. 300 = 30% weight to the
    /// newest observation.
    pub ewma_alpha_permille: u64,
    /// Maximum mark-IV move per governance sweep, in bps of the current
    /// mark (e.g. 500 = 5% of vol per interval).
    pub max_move_bps_per_sweep: u64,
    /// Age of the last usable book observation after which the mark IV
    /// decays back toward the anchor (ms).
    pub staleness_ms: u64,
    /// Minimum resting size (lots) on BOTH sides for the book observation
    /// to count (thin books do not move the mark).
    pub min_touch_lots: u64,
}

impl Default for SurfaceConfig {
    fn default() -> Self {
        Self {
            sanity_band_bps: 2500,
            ewma_alpha_permille: 300,
            max_move_bps_per_sweep: 500,
            staleness_ms: 60_000,
            min_touch_lots: 1,
        }
    }
}

impl SurfaceConfig {
    /// Validate the governance parameters.
    ///
    /// Rejected as misconfiguration: a zero sanity band (nothing can ever
    /// pass), a blend weight above 1.0 (would overshoot the book), a zero
    /// or above-100% move clamp (frozen or unlimited governance), zero
    /// staleness (instantly stale books), and a zero touch minimum (empty
    /// levels could move the mark).
    pub fn validate(&self) -> Result<(), SurfaceError> {
        if self.sanity_band_bps == 0 {
            return Err(SurfaceError::ZeroSanityBand);
        }
        if self.ewma_alpha_permille > 1000 {
            return Err(SurfaceError::EwmaAlphaAboveOne);
        }
        if self.max_move_bps_per_sweep == 0 {
            return Err(SurfaceError::ZeroMaxMove);
        }
        if self.max_move_bps_per_sweep > 10_000 {
            return Err(SurfaceError::MaxMoveAboveOne);
        }
        if self.staleness_ms == 0 {
            return Err(SurfaceError::ZeroStaleness);
        }
        if self.min_touch_lots == 0 {
            return Err(SurfaceError::ZeroMinTouch);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Per-market surface state (read access via [`VolSurface::surface`]).
///
/// Fields are readable for views and tests; mutation is crate-internal —
/// only the governed [`VolSurface::observe`] / [`VolSurface::sweep`] paths
/// ever move a mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketSurface {
    /// Configured anchor IV in bps — the mark starts here and decays back
    /// here once the book goes stale. The fail-safe mark.
    pub anchor_iv_bps: u64,
    /// The governed mark IV in bps. This is what the engine uses as the
    /// marking IV.
    pub mark_iv_bps: u64,
    /// Last sanity-passing book IV in bps (the sweep chase target), if any.
    pub last_book_iv_bps: Option<u64>,
    /// Timestamp of the last sanity-passing book observation, if any.
    pub last_book_ts: Option<TimestampMs>,
}

/// The governed volatility surface for all registered option markets.
#[derive(Debug, Clone)]
pub struct VolSurface {
    config: SurfaceConfig,
    markets: BTreeMap<Symbol, MarketSurface>,
}

impl VolSurface {
    /// Build a surface governed by `config`, which must pass
    /// [`SurfaceConfig::validate`].
    pub fn new(config: SurfaceConfig) -> Result<Self, SurfaceError> {
        config.validate()?;
        Ok(Self {
            config,
            markets: BTreeMap::new(),
        })
    }

    /// The governance parameters in force.
    #[must_use]
    pub fn config(&self) -> &SurfaceConfig {
        &self.config
    }

    /// Register a market with its configured anchor IV (bps, e.g. 5500 =
    /// 55%). The mark IV starts at the anchor.
    ///
    /// Re-registering an existing symbol resets it to the new anchor and
    /// clears the book state. A zero anchor is legal but fail-safe: the
    /// zero-width sanity band rejects every positive book IV, freezing the
    /// mark at zero.
    pub fn register(&mut self, symbol: &str, anchor_iv_bps: u64) {
        self.markets.insert(
            symbol.to_string(),
            MarketSurface {
                anchor_iv_bps,
                mark_iv_bps: anchor_iv_bps,
                last_book_iv_bps: None,
                last_book_ts: None,
            },
        );
    }

    /// The governed mark IV for a market (bps), or `None` when unregistered.
    /// This is what the engine uses as the marking IV.
    #[must_use]
    pub fn mark_iv_bps(&self, symbol: &str) -> Option<u64> {
        self.markets.get(symbol).map(|m| m.mark_iv_bps)
    }

    /// Read access to a market's surface state, for views and tests.
    #[must_use]
    pub fn surface(&self, symbol: &str) -> Option<&MarketSurface> {
        self.markets.get(symbol)
    }

    /// One book observation for a market.
    ///
    /// `spot_quote_minor` / `strike_quote_minor` are quote-minor per `1.0`
    /// base; `is_call` selects the option kind; `tte_ms` is the time to
    /// expiry in milliseconds (for everlasting options the engine passes
    /// the effective maturity implied by the funding interval), converted
    /// internally to years with poc-engine's exact 365-day convention;
    /// `bid_quote_minor` / `ask_quote_minor` are the observed resting option
    /// premiums per `1.0` base with `bid_lots` / `ask_lots` resting at
    /// touch; `now` is engine wall-clock.
    ///
    /// Blend stage — an unusable observation leaves the surface untouched:
    ///
    /// 1. Ignored when the market is unregistered, either side is missing
    ///    (zero price) or rests below `min_touch_lots`, the book is crossed
    ///    (`bid > ask`), `tte_ms` is zero, or spot/strike is zero.
    /// 2. Mid premium = `(bid + ask) / 2` (integer, rounded down).
    /// 3. Invert Black-76 via poc-margin's implied-vol solver to get the
    ///    book IV in bps; solver failures (below intrinsic, above the
    ///    no-arbitrage bound, degenerate inputs) ignore the observation.
    /// 4. Sanity band: `|book_iv − anchor|` must be within
    ///    `sanity_band_bps` of the anchor — a manipulated or dislocated
    ///    book must not move the mark.
    /// 5. Blend: `mark_iv += alpha × (book_iv − mark_iv) / 1000` in integer
    ///    arithmetic with the update magnitude floored, so the mark moves
    ///    toward the book monotonically and never overshoots it.
    /// 6. Remember the book IV and `now` as the chase target of the next
    ///    [`VolSurface::sweep`].
    // The flat engine-feed shape is deliberate (G-04 spec): one call per
    // book tick, no intermediate structs on the hot path.
    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        &mut self,
        symbol: &str,
        spot_quote_minor: u128,
        strike_quote_minor: u128,
        is_call: bool,
        tte_ms: u128,
        bid_quote_minor: u128,
        ask_quote_minor: u128,
        bid_lots: u64,
        ask_lots: u64,
        now: TimestampMs,
    ) {
        self.observe_inner(
            symbol,
            spot_quote_minor,
            strike_quote_minor,
            is_call,
            tte_ms,
            bid_quote_minor,
            ask_quote_minor,
            bid_lots,
            ask_lots,
            now,
            false,
        );
    }

    /// American-quote observation: identical gating, but the mid premium
    /// is inverted through the Barone-Adesi-Whaley American pricer
    /// instead of Black-Scholes.
    #[allow(clippy::too_many_arguments)]
    ///
    /// At the crate's zero-rate convention the two inversions coincide
    /// exactly (early exercise is worthless when `r = 0`, `b = 0` — a
    /// property the tests assert), so this flag is a correctness rail for
    /// the day the surface carries a non-zero rate: American quotes must
    /// never be inverted through the European curve.
    pub fn observe_american(
        &mut self,
        symbol: &str,
        spot_quote_minor: u128,
        strike_quote_minor: u128,
        is_call: bool,
        tte_ms: u128,
        bid_quote_minor: u128,
        ask_quote_minor: u128,
        bid_lots: u64,
        ask_lots: u64,
        now: TimestampMs,
    ) {
        self.observe_inner(
            symbol,
            spot_quote_minor,
            strike_quote_minor,
            is_call,
            tte_ms,
            bid_quote_minor,
            ask_quote_minor,
            bid_lots,
            ask_lots,
            now,
            true,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn observe_inner(
        &mut self,
        symbol: &str,
        spot_quote_minor: u128,
        strike_quote_minor: u128,
        is_call: bool,
        tte_ms: u128,
        bid_quote_minor: u128,
        ask_quote_minor: u128,
        bid_lots: u64,
        ask_lots: u64,
        now: TimestampMs,
        american: bool,
    ) {
        let cfg = self.config;
        let Some(m) = self.markets.get_mut(symbol) else {
            return; // unregistered: nothing to mark
        };
        // Gate 1 — a usable, uncrossed, two-sided book.
        if spot_quote_minor == 0
            || strike_quote_minor == 0
            || tte_ms == 0
            || bid_quote_minor == 0
            || ask_quote_minor == 0
            || bid_quote_minor > ask_quote_minor
            || bid_lots < cfg.min_touch_lots
            || ask_lots < cfg.min_touch_lots
        {
            return;
        }
        // Gate 2 — mid premium, floored.
        let Some(sum) = bid_quote_minor.checked_add(ask_quote_minor) else {
            return;
        };
        let mid = sum / 2;
        // Gate 3 — Black-76 inversion (European) or BAW inversion
        // (American), by exercise style.
        let Some(book_iv) = book_iv_bps(
            spot_quote_minor,
            strike_quote_minor,
            is_call,
            tte_ms,
            mid,
            american,
        ) else {
            return;
        };
        // Gate 4 — sanity band around the anchor.
        if !within_band(m.anchor_iv_bps, book_iv, cfg.sanity_band_bps) {
            return;
        }
        // Blend, and remember the chase target.
        m.mark_iv_bps = blend(m.mark_iv_bps, book_iv, cfg.ewma_alpha_permille);
        m.last_book_iv_bps = Some(book_iv);
        m.last_book_ts = Some(now);
    }

    /// Governance sweep, called once per engine tick interval.
    ///
    /// Pure function of state + `now` (deterministic, replayable):
    ///
    /// * **Stale book** — the last usable observation is older than
    ///   `staleness_ms` (or never happened): decay the mark toward the
    ///   anchor by one clamped step. The anchor is the fail-safe mark, and
    ///   an untrusted target is not chased.
    /// * **Fresh book** — chase the last book IV, clamped to at most
    ///   `max_move_bps_per_sweep` bps of the *current* mark per sweep.
    ///
    /// Either way the mark moves at most `max_move_bps_per_sweep` of itself
    /// toward its target and never past it.
    pub fn sweep(&mut self, now: TimestampMs) {
        let cfg = self.config;
        for m in self.markets.values_mut() {
            let stale = match m.last_book_ts {
                None => true,
                Some(ts) => now.saturating_sub(ts) > cfg.staleness_ms,
            };
            let target = if stale {
                m.anchor_iv_bps
            } else {
                match m.last_book_iv_bps {
                    Some(book) => book,
                    None => m.mark_iv_bps, // unreachable in practice: no-op
                }
            };
            m.mark_iv_bps = clamped_step_toward(m.mark_iv_bps, target, cfg.max_move_bps_per_sweep);
        }
    }
}

// ---------------------------------------------------------------------------
// Integer helpers (pure, unit-tested below)
// ---------------------------------------------------------------------------

/// `|iv − anchor| <= sanity_band_bps` of the anchor, band width floored.
///
/// An overflowing width computation (absurd anchor × band) degrades to a
/// zero width — reject, never panic.
fn within_band(anchor_iv_bps: u64, iv_bps: u64, sanity_band_bps: u64) -> bool {
    let Some(width) = mul_div_floor(
        u128::from(anchor_iv_bps),
        u128::from(sanity_band_bps),
        10_000,
    ) else {
        return false;
    };
    let diff = if iv_bps >= anchor_iv_bps {
        u128::from(iv_bps - anchor_iv_bps)
    } else {
        u128::from(anchor_iv_bps - iv_bps)
    };
    diff <= width
}

/// One EWMA blend step: `mark + alpha_permille × (book − mark) / 1000`.
///
/// Rust's integer division truncates toward zero, which floors the
/// magnitude of the update for both signs — the mark therefore moves at
/// most the exact fractional distance and can never overshoot the book
/// (nor leave the mark↔book interval).
fn blend(mark_iv_bps: u64, book_iv_bps: u64, alpha_permille: u64) -> u64 {
    let diff = i128::from(book_iv_bps) - i128::from(mark_iv_bps);
    if diff == 0 {
        return mark_iv_bps;
    }
    let update = diff * i128::from(alpha_permille) / 1000;
    let next = i128::from(mark_iv_bps) + update;
    // `|update| <= |diff|` keeps `next` inside the mark↔book interval, so
    // the conversion cannot fail; the fallback merely refuses corruption.
    u64::try_from(next).unwrap_or(mark_iv_bps)
}

/// Move `from` toward `target` by at most `max_move_bps_per_sweep` bps of
/// `from` (floored), never overshooting the target.
fn clamped_step_toward(from: u64, target: u64, max_move_bps_per_sweep: u64) -> u64 {
    if from == target {
        return from;
    }
    let step = mul_div_floor(u128::from(from), u128::from(max_move_bps_per_sweep), 10_000)
        .and_then(|s| u64::try_from(s).ok())
        .unwrap_or(0);
    if target > from {
        from.saturating_add(step.min(target - from))
    } else {
        from.saturating_sub(step.min(from - target))
    }
}

/// Milliseconds → years for analytics, mirroring poc-engine's conversion
/// exactly so that surface IVs and engine marks share one time base.
fn tau_years_from_ms(tte_ms: u128) -> f64 {
    tte_ms as f64 / MS_PER_YEAR
}

/// IV fraction → bps, rounded to nearest; `None` on non-finite input.
///
/// Defensive only: poc-margin's solver already promises a finite result
/// inside `[1e-6, 5.0]` (i.e. at most 50 000 bps).
fn iv_fraction_to_bps(iv_fraction: f64) -> Option<u64> {
    if !iv_fraction.is_finite() || iv_fraction < 0.0 {
        return None;
    }
    let bps = (iv_fraction * 10_000.0).round();
    if bps <= u64::MAX as f64 {
        Some(bps as u64)
    } else {
        None
    }
}

/// Solve the book IV (bps) implied by a mid premium.
///
/// Thin adapter over poc-margin's Black-76 implied-vol solver — not a
/// reimplementation: integer quote-minor inputs are cast to the f64
/// analytics units the solver expects (exactly how poc-margin's portfolio
/// scanner builds its leg views) and the solved fraction is rounded to bps
/// at the output boundary. The risk-free rate is 0 — the workspace
/// default — under which BSM and Black-76 on spot coincide.
fn book_iv_bps(
    spot_quote_minor: u128,
    strike_quote_minor: u128,
    is_call: bool,
    tte_ms: u128,
    mid_premium_quote_minor: u128,
    american: bool,
) -> Option<u64> {
    let flavour = if is_call { Flavour::Call } else { Flavour::Put };
    let solved = if american {
        AmericanAnalytics::implied_vol(
            flavour,
            spot_quote_minor as f64,
            strike_quote_minor as f64,
            tau_years_from_ms(tte_ms),
            0.0,
            0.0,
            mid_premium_quote_minor as f64,
        )
    } else {
        OptionAnalytics::implied_vol(
            flavour,
            spot_quote_minor as f64,
            strike_quote_minor as f64,
            tau_years_from_ms(tte_ms),
            0.0,
            mid_premium_quote_minor as f64,
        )
    };
    iv_fraction_to_bps(solved.ok()?)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Market under test.
    const SYM: &str = "BTC-80000-C";
    /// ATM option book constants: $80 000 spot/strike, 90 days to expiry
    /// (spot/strike/premium all in quote-minor per 1.0 base).
    const SPOT: u128 = 8_000_000;
    const STRIKE: u128 = 8_000_000;
    const TTE_MS: u128 = 90 * 24 * 3_600_000;

    /// Default-config surface with one registered market.
    fn surface(anchor_bps: u64) -> VolSurface {
        surface_with(SurfaceConfig::default(), anchor_bps)
    }

    /// Custom-config surface with one registered market.
    fn surface_with(cfg: SurfaceConfig, anchor_bps: u64) -> VolSurface {
        let mut s = VolSurface::new(cfg).expect("test config is valid");
        s.register(SYM, anchor_bps);
        s
    }

    /// Resting premium (quote-minor per base) that the solver maps back to
    /// `iv`: price the option at `iv`, then feed that premium back in.
    fn premium_at(spot: u128, strike: u128, is_call: bool, iv: f64) -> u128 {
        let p = OptionAnalytics::price(
            if is_call { Flavour::Call } else { Flavour::Put },
            spot as f64,
            strike as f64,
            tau_years_from_ms(TTE_MS),
            iv,
            0.0,
        );
        p.round() as u128
    }

    /// Observe a two-sided, well-sized ATM-call book whose mid solves to
    /// approximately `iv`.
    fn observe_iv(s: &mut VolSurface, symbol: &str, iv: f64, now: TimestampMs) {
        let p = premium_at(SPOT, STRIKE, true, iv);
        s.observe(symbol, SPOT, STRIKE, true, TTE_MS, p, p, 5, 5, now);
    }

    /// The solved book IV (bps) for a premium priced at `iv`, read off a
    /// disposable alpha=100% surface (one blend lands exactly on the book).
    fn solved_bps(iv: f64, anchor_bps: u64) -> u64 {
        let mut t = surface_with(
            SurfaceConfig {
                ewma_alpha_permille: 1000,
                ..SurfaceConfig::default()
            },
            anchor_bps,
        );
        observe_iv(&mut t, SYM, iv, 1_000);
        t.mark_iv_bps(SYM).expect("alpha=100% adopts the book IV")
    }

    #[test]
    fn unregistered_market_has_no_mark() {
        let mut s = surface(5_500);
        assert_eq!(s.mark_iv_bps("NOPE"), None);
        assert!(s.surface("NOPE").is_none());
        // Observing an unregistered market is a silent no-op.
        s.observe("NOPE", SPOT, STRIKE, true, TTE_MS, 100, 100, 9, 9, 1_000);
        assert_eq!(s.mark_iv_bps("NOPE"), None);
    }

    #[test]
    fn mark_starts_at_anchor() {
        let s = surface(5_500);
        assert_eq!(s.mark_iv_bps(SYM), Some(5_500));
        let m = s.surface(SYM).unwrap();
        assert_eq!(m.anchor_iv_bps, 5_500);
        assert_eq!(m.mark_iv_bps, 5_500);
        assert_eq!(m.last_book_iv_bps, None);
        assert_eq!(m.last_book_ts, None);
    }

    #[test]
    fn thin_book_is_ignored() {
        // Control: a sized book moves the mark...
        let mut s = surface(5_500);
        let p = premium_at(SPOT, STRIKE, true, 0.60);
        s.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 1, 1, 1_000);
        assert!(s.mark_iv_bps(SYM).unwrap() > 5_500);

        // ...but a book resting below the touch minimum does not.
        let mut thin = surface(5_500);
        thin.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 0, 1, 1_000);
        thin.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 1, 0, 1_001);
        thin.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 0, 0, 1_002);
        assert_eq!(thin.mark_iv_bps(SYM), Some(5_500));
        assert_eq!(thin.surface(SYM).unwrap().last_book_ts, None);

        // Same with a higher configured minimum.
        let mut picky = surface_with(
            SurfaceConfig {
                min_touch_lots: 5,
                ..SurfaceConfig::default()
            },
            5_500,
        );
        picky.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 4, 9, 1_000);
        assert_eq!(picky.mark_iv_bps(SYM), Some(5_500));
    }

    #[test]
    fn one_sided_book_is_ignored() {
        let mut s = surface(5_500);
        let p = premium_at(SPOT, STRIKE, true, 0.60);
        // Missing bid price / missing ask price / zero size on either side.
        s.observe(SYM, SPOT, STRIKE, true, TTE_MS, 0, p, 5, 5, 1_000);
        s.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, 0, 5, 5, 1_001);
        s.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 5, 0, 1_002);
        s.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 0, 5, 1_003);
        // A crossed book is equally unusable.
        s.observe(SYM, SPOT, STRIKE, true, TTE_MS, p + 10, p, 5, 5, 1_004);
        assert_eq!(s.mark_iv_bps(SYM), Some(5_500));
        assert_eq!(s.surface(SYM).unwrap().last_book_ts, None);
        // Control: the same premium two-sided moves the mark.
        observe_iv(&mut s, SYM, 0.60, 2_000);
        assert!(s.mark_iv_bps(SYM).unwrap() > 5_500);
    }

    #[test]
    fn out_of_band_book_is_ignored() {
        // Default band 2500 around anchor 5500 -> allowed [4125, 6875].
        let mut s = surface(5_500);
        for iv in [0.95_f64, 0.10] {
            // Control: a wide-band twin accepts the same premium, proving
            // the solver succeeds and only the band rejects it.
            let mut wide = surface_with(
                SurfaceConfig {
                    sanity_band_bps: 10_000,
                    ..SurfaceConfig::default()
                },
                5_500,
            );
            observe_iv(&mut wide, SYM, iv, 1_000);
            assert_ne!(wide.mark_iv_bps(SYM), Some(5_500), "control accepts {iv}");

            let p = premium_at(SPOT, STRIKE, true, iv);
            s.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 5, 5, 1_000);
            assert_eq!(s.mark_iv_bps(SYM), Some(5_500));
            assert_eq!(s.surface(SYM).unwrap().last_book_iv_bps, None);
        }
    }

    #[test]
    fn ewma_blends_monotone_without_overshoot() {
        // Upward: anchor 5500, book ~6000.
        let target = solved_bps(0.60, 5_500);
        assert!((5_995..=6_005).contains(&target), "solved {target}");
        let mut s = surface(5_500);
        let mut prev = 5_500_u64;
        for i in 0..40_u64 {
            observe_iv(&mut s, SYM, 0.60, 1_000 + i);
            let m = s.mark_iv_bps(SYM).unwrap();
            assert!(m >= prev, "monotone up at step {i}");
            assert!(m <= target, "never past the book at step {i}");
            prev = m;
        }
        // The floored blend can stall a few bps short of the target; one
        // fresh sweep closes the remainder exactly.
        assert!(prev >= target - 3, "converged to {prev} vs {target}");
        s.sweep(1_050);
        assert_eq!(s.mark_iv_bps(SYM), Some(target));

        // Downward: anchor 5500, book ~5000.
        let target = solved_bps(0.50, 5_500);
        assert!((4_995..=5_005).contains(&target), "solved {target}");
        let mut d = surface(5_500);
        let mut prev = 5_500_u64;
        for i in 0..40_u64 {
            observe_iv(&mut d, SYM, 0.50, 1_000 + i);
            let m = d.mark_iv_bps(SYM).unwrap();
            assert!(m <= prev, "monotone down at step {i}");
            assert!(m >= target, "never past the book at step {i}");
            prev = m;
        }
        d.sweep(1_050);
        assert_eq!(d.mark_iv_bps(SYM), Some(target));
    }

    #[test]
    fn sweep_moves_at_most_max_move_of_the_mark() {
        let mut s = surface(5_500); // clamp 500 bps = 5% of the mark
        observe_iv(&mut s, SYM, 0.68, 1_000); // ~6800, inside the band
        let m = s.surface(SYM).unwrap();
        let book = m.last_book_iv_bps.unwrap();
        let after_obs = m.mark_iv_bps;
        assert!(book > 5_500 && book <= 6_875, "book {book}");
        // Blend formula: anchor + floor(0.3 x (book - anchor)).
        assert_eq!(
            after_obs,
            u64::try_from(5_500 + (i128::from(book) - 5_500) * 300 / 1000).unwrap()
        );

        // A fresh sweep chases the book, clamped to 5% of the current mark.
        let before = s.mark_iv_bps(SYM).unwrap();
        s.sweep(1_010); // age 10 ms << staleness
        let after = s.mark_iv_bps(SYM).unwrap();
        let step = before * 500 / 10_000; // floored
        assert_eq!(after, before + step.min(book - before));
        assert!(after > before && after <= book);

        // Repeated fresh sweeps converge toward the book, never past it,
        // each moving at most max_move bps of the pre-sweep mark.
        let mut prev = after;
        for k in 0..100_u64 {
            let t = 1_100 + 10 * k;
            s.sweep(t);
            let m = s.mark_iv_bps(SYM).unwrap();
            assert!(m >= prev && m <= book, "step {k}: {prev} -> {m}");
            assert!(m - prev <= prev * 500 / 10_000, "clamp at step {k}");
            prev = m;
        }
        assert_eq!(prev, book, "converges exactly onto the book");
    }

    #[test]
    fn stale_book_decays_toward_the_anchor() {
        let mut s = surface(5_500); // staleness 60 000 ms
        observe_iv(&mut s, SYM, 0.63, 1_000); // ~6300, inside the band
        let above = s.mark_iv_bps(SYM).unwrap();
        assert!(above > 5_500 && above < 6_300);

        // A fresh sweep chases the book (away from the anchor here).
        s.sweep(2_000);
        let chased = s.mark_iv_bps(SYM).unwrap();
        assert!(chased >= above);

        // At exactly `staleness_ms` of age the book still counts...
        s.sweep(1_000 + 60_000);
        let chased2 = s.mark_iv_bps(SYM).unwrap();
        assert!(chased2 >= chased);
        // ...one ms older and the sweep decays toward the anchor instead.
        s.sweep(1_000 + 60_001);
        let decayed = s.mark_iv_bps(SYM).unwrap();
        let step = chased2 * 500 / 10_000;
        assert_eq!(decayed, chased2 - step.min(chased2 - 5_500));
        assert!(decayed < chased2 && decayed >= 5_500);

        // Repeated stale sweeps converge to the anchor exactly.
        let mut t = 1_000 + 60_002;
        for _ in 0..300 {
            t += 10;
            s.sweep(t);
            assert!(s.mark_iv_bps(SYM).unwrap() >= 5_500);
        }
        assert_eq!(s.mark_iv_bps(SYM), Some(5_500));

        // A never-observed market is stale by definition: the mark sits at
        // the anchor and stays there.
        let mut fresh = surface(5_500);
        fresh.sweep(123_456);
        assert_eq!(fresh.mark_iv_bps(SYM), Some(5_500));
    }

    #[test]
    fn degenerate_inputs_are_ignored() {
        let mut s = surface(5_500);
        let p = premium_at(SPOT, STRIKE, true, 0.60);
        s.observe(SYM, 0, STRIKE, true, TTE_MS, p, p, 5, 5, 1_000); // spot 0
        s.observe(SYM, SPOT, 0, true, TTE_MS, p, p, 5, 5, 1_001); // strike 0
        s.observe(SYM, SPOT, STRIKE, true, 0, p, p, 5, 5, 1_002); // tte 0
        assert_eq!(s.mark_iv_bps(SYM), Some(5_500));
        assert_eq!(s.surface(SYM).unwrap().last_book_ts, None);
    }

    #[test]
    fn below_intrinsic_premium_is_ignored() {
        let mut s = surface(5_500);
        // Deep-ITM call: intrinsic = 3 000 000 minor; a 1 000 000 premium
        // is below intrinsic -> no volatility can price it.
        s.observe(
            SYM, 8_000_000, 5_000_000, true, TTE_MS, 1_000_000, 1_000_000, 5, 5, 1_000,
        );
        assert_eq!(s.mark_iv_bps(SYM), Some(5_500));
        assert_eq!(s.surface(SYM).unwrap().last_book_iv_bps, None);
        // Control: an intrinsic-plus premium on the same market solves.
        let q = premium_at(8_000_000, 7_000_000, true, 0.60);
        s.observe(SYM, 8_000_000, 7_000_000, true, TTE_MS, q, q, 5, 5, 1_001);
        assert_ne!(s.mark_iv_bps(SYM), Some(5_500));
    }

    #[test]
    fn identical_input_sequences_produce_identical_surfaces() {
        let cfg = SurfaceConfig {
            sanity_band_bps: 2_000,
            ewma_alpha_permille: 400,
            max_move_bps_per_sweep: 750,
            staleness_ms: 30_000,
            min_touch_lots: 2,
        };

        // Feed a mixed stream: accepted, thin, out-of-band, degenerate
        // observations, fresh sweeps, a stale gap, then a returning book.
        let run_feed = |s: &mut VolSurface| {
            s.register("BTC-80000-C", 5_500);
            s.register("BTC-60000-P", 5_800);
            let p_call = premium_at(SPOT, STRIKE, true, 0.60); // ~6000 bps
            let p_wide = premium_at(SPOT, STRIKE, true, 0.95); // ~9500 bps
            let p_put = premium_at(SPOT, 6_000_000, false, 0.60); // ~6000 bps
            for t in [1_000_u64, 2_000, 3_000, 4_000, 60_000, 61_000] {
                s.observe(
                    "BTC-80000-C",
                    SPOT,
                    STRIKE,
                    true,
                    TTE_MS,
                    p_call,
                    p_call,
                    5,
                    5,
                    t,
                );
                s.observe(
                    "BTC-80000-C",
                    SPOT,
                    STRIKE,
                    true,
                    TTE_MS,
                    p_call,
                    p_call,
                    1,
                    5,
                    t,
                );
                s.observe(
                    "BTC-80000-C",
                    SPOT,
                    STRIKE,
                    true,
                    TTE_MS,
                    p_wide,
                    p_wide,
                    5,
                    5,
                    t,
                );
                s.observe(
                    "BTC-80000-C",
                    SPOT,
                    STRIKE,
                    true,
                    0,
                    p_call,
                    p_call,
                    5,
                    5,
                    t,
                );
                s.observe(
                    "BTC-60000-P",
                    SPOT,
                    6_000_000,
                    false,
                    TTE_MS,
                    p_put,
                    p_put,
                    1,
                    1,
                    t,
                );
                s.observe(
                    "BTC-60000-P",
                    SPOT,
                    6_000_000,
                    false,
                    TTE_MS,
                    p_put,
                    p_put,
                    2,
                    2,
                    t,
                );
                s.sweep(t);
            }
            // A long quiet gap (>> staleness): sweeps decay toward anchors.
            for t in [120_000_u64, 121_000, 122_000] {
                s.sweep(t);
            }
            // The book returns.
            s.observe(
                "BTC-80000-C",
                SPOT,
                STRIKE,
                true,
                TTE_MS,
                p_call,
                p_call,
                5,
                5,
                130_000,
            );
            s.observe(
                "BTC-60000-P",
                SPOT,
                6_000_000,
                false,
                TTE_MS,
                p_put,
                p_put,
                2,
                2,
                130_000,
            );
            s.sweep(130_001);
        };

        let mut a = VolSurface::new(cfg).expect("scenario config valid");
        let mut b = VolSurface::new(cfg).expect("scenario config valid");
        let mut c = VolSurface::new(cfg).expect("scenario config valid");
        run_feed(&mut a);
        run_feed(&mut b);
        run_feed(&mut c);
        for sym in ["BTC-80000-C", "BTC-60000-P"] {
            assert_eq!(a.surface(sym), b.surface(sym), "{sym}: a == b");
            assert_eq!(b.surface(sym), c.surface(sym), "{sym}: b == c");
        }
        // The feed actually moved the markets (non-vacuous equality).
        assert_eq!(
            a.surface("BTC-80000-C").unwrap().last_book_ts,
            Some(130_000)
        );
        assert_ne!(a.mark_iv_bps("BTC-80000-C"), Some(5_500));
        assert_ne!(a.mark_iv_bps("BTC-60000-P"), Some(5_800));
    }

    #[test]
    fn config_validation_rejects_bad_values() {
        let ok = SurfaceConfig::default();
        assert_eq!(ok.validate(), Ok(()));
        assert_eq!(
            SurfaceConfig {
                sanity_band_bps: 0,
                ..ok
            }
            .validate(),
            Err(SurfaceError::ZeroSanityBand)
        );
        assert_eq!(
            SurfaceConfig {
                ewma_alpha_permille: 1001,
                ..ok
            }
            .validate(),
            Err(SurfaceError::EwmaAlphaAboveOne)
        );
        assert_eq!(
            SurfaceConfig {
                ewma_alpha_permille: 1000,
                ..ok
            }
            .validate(),
            Ok(())
        );
        assert_eq!(
            SurfaceConfig {
                max_move_bps_per_sweep: 0,
                ..ok
            }
            .validate(),
            Err(SurfaceError::ZeroMaxMove)
        );
        assert_eq!(
            SurfaceConfig {
                max_move_bps_per_sweep: 10_001,
                ..ok
            }
            .validate(),
            Err(SurfaceError::MaxMoveAboveOne)
        );
        assert_eq!(
            SurfaceConfig {
                max_move_bps_per_sweep: 10_000,
                ..ok
            }
            .validate(),
            Ok(())
        );
        assert_eq!(
            SurfaceConfig {
                staleness_ms: 0,
                ..ok
            }
            .validate(),
            Err(SurfaceError::ZeroStaleness)
        );
        assert_eq!(
            SurfaceConfig {
                min_touch_lots: 0,
                ..ok
            }
            .validate(),
            Err(SurfaceError::ZeroMinTouch)
        );
        // The constructor enforces the same contract.
        assert!(VolSurface::new(SurfaceConfig {
            ewma_alpha_permille: 1001,
            ..ok
        })
        .is_err());
    }

    #[test]
    fn markets_do_not_interfere() {
        let mut s = VolSurface::new(SurfaceConfig::default()).expect("default config valid");
        s.register("BTC-80000-C", 5_500);
        s.register("BTC-60000-P", 4_000);
        let p_call = premium_at(SPOT, STRIKE, true, 0.60); // ~6000, in band for 5500
        let p_put = premium_at(SPOT, 6_000_000, false, 0.35); // ~3500, in band for 4000

        // A moves; B stays at its anchor.
        s.observe(
            "BTC-80000-C",
            SPOT,
            STRIKE,
            true,
            TTE_MS,
            p_call,
            p_call,
            5,
            5,
            1_000,
        );
        let a1 = s.mark_iv_bps("BTC-80000-C").unwrap();
        assert!(a1 > 5_500);
        assert_eq!(s.mark_iv_bps("BTC-60000-P"), Some(4_000));

        // Garbage on A leaves both alone.
        let p_wide = premium_at(SPOT, STRIKE, true, 0.95);
        s.observe(
            "BTC-80000-C",
            SPOT,
            STRIKE,
            true,
            TTE_MS,
            p_wide,
            p_wide,
            5,
            5,
            1_001,
        );
        assert_eq!(s.mark_iv_bps("BTC-80000-C"), Some(a1));
        assert_eq!(s.mark_iv_bps("BTC-60000-P"), Some(4_000));

        // B moves on its own observation; A keeps its state.
        s.observe(
            "BTC-60000-P",
            SPOT,
            6_000_000,
            false,
            TTE_MS,
            p_put,
            p_put,
            5,
            5,
            1_002,
        );
        assert_eq!(s.mark_iv_bps("BTC-80000-C"), Some(a1));
        let b1 = s.mark_iv_bps("BTC-60000-P").unwrap();
        assert!(b1 < 4_000, "blend toward ~3500, got {b1}");

        // A sweep chases each market toward its own book target.
        s.sweep(1_010);
        assert!(s.mark_iv_bps("BTC-80000-C").unwrap() >= a1);
        assert!(s.mark_iv_bps("BTC-60000-P").unwrap() <= b1);
    }

    #[test]
    fn sweep_is_noop_when_mark_already_at_target() {
        // alpha = 100%: one observation lands exactly on the book IV.
        let mut s = surface_with(
            SurfaceConfig {
                ewma_alpha_permille: 1000,
                ..SurfaceConfig::default()
            },
            5_500,
        );
        observe_iv(&mut s, SYM, 0.60, 1_000);
        let at_book = s.mark_iv_bps(SYM).unwrap();
        assert_eq!(s.surface(SYM).unwrap().last_book_iv_bps, Some(at_book));
        s.sweep(1_010); // fresh, mark == target: no movement
        assert_eq!(s.mark_iv_bps(SYM), Some(at_book));

        // A never-observed market is likewise unmoved (mark == anchor).
        let mut fresh = surface(5_500);
        fresh.sweep(9_999_999);
        assert_eq!(fresh.mark_iv_bps(SYM), Some(5_500));
    }

    #[test]
    fn mid_premium_floors_to_the_integer_half() {
        let p = premium_at(SPOT, STRIKE, true, 0.60);
        let mut a = surface(5_500);
        let mut b = surface(5_500);
        a.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 5, 5, 1_000); // mid = p
        b.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p + 1, 5, 5, 1_000); // (2p+1)/2 -> p
        assert!(a.mark_iv_bps(SYM).unwrap() > 5_500); // non-vacuous
        assert_eq!(a.mark_iv_bps(SYM), b.mark_iv_bps(SYM));
        assert_eq!(a.surface(SYM), b.surface(SYM));
    }

    #[test]
    fn re_register_resets_the_market() {
        let mut s = surface(5_500);
        observe_iv(&mut s, SYM, 0.60, 1_000);
        assert!(s.mark_iv_bps(SYM).unwrap() > 5_500);
        s.register(SYM, 4_800);
        let m = s.surface(SYM).unwrap();
        assert_eq!(m.anchor_iv_bps, 4_800);
        assert_eq!(m.mark_iv_bps, 4_800);
        assert_eq!(m.last_book_iv_bps, None);
        assert_eq!(m.last_book_ts, None);
    }

    #[test]
    fn zero_anchor_is_fail_safe_frozen() {
        let mut s = surface(0);
        // Band width is zero: only a zero book IV could pass, so every
        // real book is rejected and the mark stays frozen at zero.
        observe_iv(&mut s, SYM, 0.60, 1_000);
        assert_eq!(s.mark_iv_bps(SYM), Some(0));
        assert_eq!(s.surface(SYM).unwrap().last_book_iv_bps, None);
        s.sweep(1_000);
        assert_eq!(s.mark_iv_bps(SYM), Some(0));
    }

    #[test]
    fn integer_helpers_round_as_specified() {
        // Band: width = floor(anchor * band / 10 000).
        assert!(within_band(5_500, 6_875, 2_500)); // exactly at the edge
        assert!(!within_band(5_500, 6_876, 2_500));
        assert!(within_band(5_500, 4_125, 2_500));
        assert!(!within_band(5_500, 4_124, 2_500));
        assert!(within_band(5_500, 5_500, 2_500));
        assert!(!within_band(5_500, 5_501, 0)); // zero width: exact only
        assert!(within_band(0, 0, 2_500)); // zero anchor: only zero passes

        // Blend: floor the magnitude of the update, never overshoot.
        assert_eq!(blend(5_500, 6_800, 300), 5_890); // 0.3 x 1300 exact
        assert_eq!(blend(6_800, 5_500, 300), 6_410);
        assert_eq!(blend(5_500, 5_510, 333), 5_503); // 3.33 -> 3
        assert_eq!(blend(5_510, 5_500, 333), 5_507); // -3.33 -> -3
        assert_eq!(blend(5_500, 5_500, 999), 5_500); // no gap, no move
        assert_eq!(blend(1_000, 9_000, 1000), 9_000); // alpha 100% adopts

        // Clamped step: at most max_move of the *current* mark, floored.
        assert_eq!(clamped_step_toward(1_000, 2_000, 500), 1_050);
        assert_eq!(clamped_step_toward(1_000, 1_030, 500), 1_030); // gap < step
        assert_eq!(clamped_step_toward(2_000, 1_000, 500), 1_900);
        assert_eq!(clamped_step_toward(2_000, 1_970, 500), 1_970);
        assert_eq!(clamped_step_toward(1_234, 1_234, 500), 1_234);
        assert_eq!(clamped_step_toward(199, 5_000, 500), 208); // floor(9.95)=9
        assert_eq!(clamped_step_toward(1_000, 2_000, 10_000), 2_000); // 100%

        // IV fraction -> bps, rounded to nearest.
        assert_eq!(iv_fraction_to_bps(0.55), Some(5_500));
        assert_eq!(iv_fraction_to_bps(0.550_04), Some(5_500));
        assert_eq!(iv_fraction_to_bps(0.550_06), Some(5_501));
        assert_eq!(iv_fraction_to_bps(f64::NAN), None);
        assert_eq!(iv_fraction_to_bps(-0.1), None);
    }

    #[test]
    fn american_inversion_matches_european_at_zero_rate() {
        // r = 0, b = 0: early exercise is worthless, so the American
        // and European inversions of the same mid premium agree exactly.
        let p = premium_at(SPOT, STRIKE, true, 0.44);
        let mut amer = surface(5_000);
        amer.observe_american(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 10, 10, 1_000);
        let mut eur = surface(5_000);
        eur.observe(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 10, 10, 1_000);
        let a = amer.mark_iv_bps(SYM).unwrap();
        let e = eur.mark_iv_bps(SYM).unwrap();
        assert_eq!(a, e, "zero-rate inversions must coincide: {a} vs {e}");
    }

    #[test]
    fn american_inversion_gates_like_european() {
        // Thin one-sided books are ignored identically under both styles.
        let p = premium_at(SPOT, STRIKE, true, 0.44);
        let mut s = surface(5_000);
        s.observe_american(SYM, SPOT, STRIKE, true, TTE_MS, p, p, 0, 10, 1_000);
        assert!(s.surface(SYM).unwrap().last_book_ts.is_none());
    }
}
