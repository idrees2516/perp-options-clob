//! Black-Scholes analytics (f64 — never posted to a ledger).
//!
//! Cash-settled European pricing is the analytics backbone of scenario
//! margin: the risk grid reprices every option leg under shocked spot and
//! volatility inputs, and implied-vol inversion derives the per-market
//! mark volatility from the order book.
//!
//! The implementation is dependency-free (norm-CDF via the
//! Abramowitz–Stegun 7.1.26 erf approximation, |ε| < 1.5e-7 — more
//! precision than the scenario grid can resolve) and handles degenerate
//! inputs (τ → 0, σ → 0) by collapsing to discounted intrinsic, the same
//! convention production risk systems use to avoid NaN propagation.

/// European option flavor for analytics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    /// Call.
    Call,
    /// Put.
    Put,
}

/// Black-Scholes-Merton helpers.
pub struct OptionAnalytics;

/// Failure modes of the implied-vol solver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpliedVolError {
    /// The option price is not achievable by any volatility (below
    /// discounted intrinsic or above the arbitrage bound).
    NoSolution,
    /// Inputs were degenerate (non-positive time or strike).
    Degenerate,
}

impl OptionAnalytics {
    /// Standard normal CDF (A&S 7.1.26).
    #[must_use]
    pub fn norm_cdf(x: f64) -> f64 {
        0.5 * (1.0 + Self::erf(x / std::f64::consts::SQRT_2))
    }

    /// Standard normal PDF.
    #[must_use]
    pub fn norm_pdf(x: f64) -> f64 {
        let c = 1.0 / (2.0 * std::f64::consts::PI).sqrt();
        c * (-0.5 * x * x).exp()
    }

    /// Error function, |ε| < 1.5e-7.
    #[must_use]
    pub fn erf(x: f64) -> f64 {
        let sign = if x < 0.0 { -1.0 } else { 1.0 };
        let ax = x.abs();
        // Abramowitz & Stegun 7.1.26, Horner form:
        // erf(x) = 1 − t·(a1 + t·(a2 + t·(a3 + t·(a4 + t·a5))))·e^(−x²)
        let t = 1.0 / (1.0 + 0.327_591_1 * ax);
        let inner = 0.254_829_592
            + t * (-0.284_496_736 + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429)));
        let poly = t * inner;
        sign * (1.0 - poly * (-ax * ax).exp())
    }

    /// European option price.
    ///
    /// Degenerate cases collapse to discounted intrinsic: τ ≤ 0 or σ ≤ 0
    /// are priced deterministically rather than erroring, because scenario
    /// grids legitimately probe σ = 0 (vol crush).
    #[must_use]
    pub fn price(
        flavour: Flavour,
        spot: f64,
        strike: f64,
        tau_years: f64,
        vol: f64,
        rate: f64,
    ) -> f64 {
        if spot <= 0.0 || strike <= 0.0 {
            return 0.0;
        }
        if tau_years <= 0.0 || vol <= 0.0 {
            return Self::discounted_intrinsic(flavour, spot, strike, tau_years.max(0.0), rate);
        }
        let sqrt_t = tau_years.sqrt();
        let sigma_sqrt_t = vol * sqrt_t;
        let d1 = ((spot / strike).ln() + (rate + 0.5 * vol * vol) * tau_years) / sigma_sqrt_t;
        let d2 = d1 - sigma_sqrt_t;
        let disc = (-rate * tau_years).exp();
        match flavour {
            Flavour::Call => spot * Self::norm_cdf(d1) - strike * disc * Self::norm_cdf(d2),
            Flavour::Put => strike * disc * Self::norm_cdf(-d2) - spot * Self::norm_cdf(-d1),
        }
    }

    /// Discounted intrinsic value (exercise logic).
    #[must_use]
    pub fn discounted_intrinsic(
        flavour: Flavour,
        spot: f64,
        strike: f64,
        tau_years: f64,
        rate: f64,
    ) -> f64 {
        let disc = (-rate * tau_years.max(0.0)).exp();
        match flavour {
            Flavour::Call => (spot - strike * disc).max(0.0),
            Flavour::Put => (strike * disc - spot).max(0.0),
        }
    }

    /// Delta: ∂V/∂S in share-equivalent units.
    #[must_use]
    pub fn delta(
        flavour: Flavour,
        spot: f64,
        strike: f64,
        tau_years: f64,
        vol: f64,
        rate: f64,
    ) -> f64 {
        if tau_years <= 0.0 || vol <= 0.0 || spot <= 0.0 || strike <= 0.0 {
            return match flavour {
                Flavour::Call => {
                    if spot > strike {
                        1.0
                    } else {
                        0.0
                    }
                }
                Flavour::Put => {
                    if spot < strike {
                        -1.0
                    } else {
                        0.0
                    }
                }
            };
        }
        let sqrt_t = tau_years.sqrt();
        let d1 = ((spot / strike).ln() + (rate + 0.5 * vol * vol) * tau_years) / (vol * sqrt_t);
        match flavour {
            Flavour::Call => Self::norm_cdf(d1),
            Flavour::Put => Self::norm_cdf(d1) - 1.0,
        }
    }

    /// Gamma: ∂²V/∂S².
    #[must_use]
    pub fn gamma(spot: f64, strike: f64, tau_years: f64, vol: f64, rate: f64) -> f64 {
        if tau_years <= 0.0 || vol <= 0.0 || spot <= 0.0 || strike <= 0.0 {
            return 0.0;
        }
        let sqrt_t = tau_years.sqrt();
        let d1 = ((spot / strike).ln() + (rate + 0.5 * vol * vol) * tau_years) / (vol * sqrt_t);
        Self::norm_pdf(d1) / (spot * vol * sqrt_t)
    }

    /// Vega: ∂V/∂σ per 1.00 of vol (divide by 100 for per-vol-point).
    #[must_use]
    pub fn vega(spot: f64, strike: f64, tau_years: f64, vol: f64, rate: f64) -> f64 {
        if tau_years <= 0.0 || vol <= 0.0 || spot <= 0.0 || strike <= 0.0 {
            return 0.0;
        }
        let sqrt_t = tau_years.sqrt();
        let d1 = ((spot / strike).ln() + (rate + 0.5 * vol * vol) * tau_years) / (vol * sqrt_t);
        spot * Self::norm_pdf(d1) * sqrt_t
    }

    /// Theta: ∂V/∂τ per year (positive = value gained with time).
    #[must_use]
    pub fn theta(
        flavour: Flavour,
        spot: f64,
        strike: f64,
        tau_years: f64,
        vol: f64,
        rate: f64,
    ) -> f64 {
        if tau_years <= 0.0 || vol <= 0.0 || spot <= 0.0 || strike <= 0.0 {
            return 0.0;
        }
        let sqrt_t = tau_years.sqrt();
        let d1 = ((spot / strike).ln() + (rate + 0.5 * vol * vol) * tau_years) / (vol * sqrt_t);
        let d2 = d1 - vol * sqrt_t;
        let disc = (-rate * tau_years).exp();
        let common = -spot * Self::norm_pdf(d1) * vol / (2.0 * sqrt_t);
        match flavour {
            Flavour::Call => common - rate * strike * disc * Self::norm_cdf(d2),
            Flavour::Put => common + rate * strike * disc * Self::norm_cdf(-d2),
        }
    }

    /// Implied volatility by Newton–Raphson with bisection fallback.
    ///
    /// `target_price` is the observed market premium per `1.0` base unit in
    /// the same units as `spot`/`strike`. Returns `None` when the price is
    /// outside the no-arbitrage interval for any σ.
    pub fn implied_vol(
        flavour: Flavour,
        spot: f64,
        strike: f64,
        tau_years: f64,
        rate: f64,
        target_price: f64,
    ) -> Result<f64, ImpliedVolError> {
        if tau_years <= 0.0 || spot <= 0.0 || strike <= 0.0 || target_price < 0.0 {
            return Err(ImpliedVolError::Degenerate);
        }
        let lower_bound = Self::discounted_intrinsic(flavour, spot, strike, tau_years, rate);
        if target_price < lower_bound - 1e-12 {
            return Err(ImpliedVolError::NoSolution);
        }
        // European call upper bound (tightest across flavours).
        if target_price > spot {
            return Err(ImpliedVolError::NoSolution);
        }

        let price_at = |vol: f64| Self::price(flavour, spot, strike, tau_years, vol, rate);
        let lo = 1e-6_f64;
        let hi = 5.0_f64;

        // Newton from a mid guess.
        let mut vol = 0.2_f64.clamp(lo, hi);
        for _ in 0..50 {
            let p = price_at(vol);
            let diff = p - target_price;
            if diff.abs() < 1e-10 {
                return Ok(vol);
            }
            let v = Self::vega(spot, strike, tau_years, vol, rate);
            if v < 1e-10 {
                break; // flat zone: fall through to bisection
            }
            vol = (vol - diff / v).clamp(lo, hi);
        }

        // Bisection fallback (robust, slower).
        let (mut a, mut b) = (lo, hi);
        let pa = price_at(a);
        let pb = price_at(b);
        if target_price < pa - 1e-12 || target_price > pb + 1e-12 {
            return Err(ImpliedVolError::NoSolution);
        }
        for _ in 0..100 {
            let mid = 0.5 * (a + b);
            let pm = price_at(mid);
            if pm < target_price {
                a = mid;
            } else {
                b = mid;
            }
        }
        Ok(0.5 * (a + b))
    }
}

/// A ready-to-margin option leg: everything scenario repricing needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OptionLegView {
    /// Spot of the underlying (quote per base, f64 analytics units).
    pub spot: f64,
    /// Strike (same units as spot).
    pub strike: f64,
    /// Time to expiry in years.
    pub tau_years: f64,
    /// Current implied volatility used for marking.
    pub iv: f64,
    /// Risk-free rate.
    pub rate: f64,
    /// Call or put.
    pub flavour: Flavour,
    /// Exercise style: American legs reprice with the early-exercise
    /// premium (BAW), European legs with plain Black-Scholes. Under the
    /// venue's carry convention (`b = r`, no dividends) the two coincide
    /// while `r = 0`, so the default configuration is migration-safe.
    pub american: bool,
}

impl OptionLegView {
    /// Price under this leg's exercise style at an arbitrary spot and IV.
    #[must_use]
    fn price_under_style(&self, spot: f64, iv: f64) -> f64 {
        if self.american {
            crate::american::AmericanAnalytics::baw_price(
                self.flavour,
                spot,
                self.strike,
                self.tau_years,
                iv,
                self.rate,
                self.rate, // b = r: non-dividend carry convention
            )
        } else {
            OptionAnalytics::price(
                self.flavour,
                spot,
                self.strike,
                self.tau_years,
                iv,
                self.rate,
            )
        }
    }

    /// Current mark value per `1.0` base unit.
    #[must_use]
    pub fn mark(&self) -> f64 {
        self.price_under_style(self.spot, self.iv)
    }

    /// Value under a shocked spot and vol.
    #[must_use]
    pub fn reprice(&self, spot_shock: f64, vol_shock: f64) -> f64 {
        self.price_under_style(
            (self.spot * (1.0 + spot_shock)).max(1e-12),
            (self.iv * (1.0 + vol_shock)).max(0.0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-4;

    #[test]
    fn norm_cdf_known_values() {
        assert!((OptionAnalytics::norm_cdf(0.0) - 0.5).abs() < 1e-9);
        assert!((OptionAnalytics::norm_cdf(1.96) - 0.975_002).abs() < 1e-4);
        assert!((OptionAnalytics::norm_cdf(-1.96) - 0.024_998).abs() < 1e-4);
        assert!(OptionAnalytics::norm_cdf(9.0) > 0.999_999);
    }

    #[test]
    fn put_call_parity() {
        // C − P = S − K·e^(−rT)
        let (s, k, t, v, r) = (100.0, 95.0, 0.5, 0.3, 0.05);
        let c = OptionAnalytics::price(Flavour::Call, s, k, t, v, r);
        let p = OptionAnalytics::price(Flavour::Put, s, k, t, v, r);
        let parity = s - k * (-r * t).exp();
        assert!((c - p - parity).abs() < EPS, "c={c} p={p} parity={parity}");
    }

    #[test]
    fn price_bounded_and_monotone() {
        let c = OptionAnalytics::price(Flavour::Call, 100.0, 90.0, 0.25, 0.4, 0.0);
        assert!(c > 10.0 && c < 100.0);
        // More vol, more value (both flavours).
        let c2 = OptionAnalytics::price(Flavour::Call, 100.0, 90.0, 0.25, 0.6, 0.0);
        assert!(c2 > c);
        let p1 = OptionAnalytics::price(Flavour::Put, 100.0, 110.0, 0.25, 0.4, 0.0);
        let p2 = OptionAnalytics::price(Flavour::Put, 100.0, 110.0, 0.25, 0.6, 0.0);
        assert!(p2 > p1);
    }

    #[test]
    fn degenerate_inputs_collapse_to_intrinsic() {
        // τ = 0
        let c = OptionAnalytics::price(Flavour::Call, 105.0, 100.0, 0.0, 0.3, 0.0);
        assert!((c - 5.0).abs() < EPS);
        // σ = 0
        let p = OptionAnalytics::price(Flavour::Put, 95.0, 100.0, 0.5, 0.0, 0.0);
        assert!((p - 5.0).abs() < EPS);
    }

    #[test]
    fn greeks_sanity() {
        let (s, k, t, v, r) = (100.0, 100.0, 0.5, 0.3, 0.0);
        let d = OptionAnalytics::delta(Flavour::Call, s, k, t, v, r);
        assert!(d > 0.49 && d < 0.62, "ATM call delta ~0.56, got {d}");
        let dp = OptionAnalytics::delta(Flavour::Put, s, k, t, v, r);
        assert!((d - dp - 1.0).abs() < 1e-9, "put delta = call delta − 1");
        let g = OptionAnalytics::gamma(s, k, t, v, r);
        assert!(g > 0.0);
        let vega = OptionAnalytics::vega(s, k, t, v, r);
        assert!(vega > 0.0);
    }

    #[test]
    fn implied_vol_roundtrip() {
        for true_vol in [0.15_f64, 0.4, 0.9] {
            let (s, k, t, r) = (100.0, 110.0, 0.25, 0.03);
            let price = OptionAnalytics::price(Flavour::Call, s, k, t, true_vol, r);
            let iv =
                OptionAnalytics::implied_vol(Flavour::Call, s, k, t, r, price).unwrap_or(f64::NAN);
            assert!((iv - true_vol).abs() < 1e-3, "true={true_vol} iv={iv}");
        }
    }

    #[test]
    fn implied_vol_rejects_arbitrage_prices() {
        // Below intrinsic: no vol can price this.
        let err = OptionAnalytics::implied_vol(Flavour::Call, 100.0, 90.0, 0.5, 0.0, 5.0);
        assert_eq!(err, Err(ImpliedVolError::NoSolution));
        // Above spot: impossible for a call.
        let err = OptionAnalytics::implied_vol(Flavour::Call, 100.0, 90.0, 0.5, 0.0, 150.0);
        assert_eq!(err, Err(ImpliedVolError::NoSolution));
        // Degenerate time.
        let err = OptionAnalytics::implied_vol(Flavour::Call, 100.0, 90.0, 0.0, 0.0, 5.0);
        assert_eq!(err, Err(ImpliedVolError::Degenerate));
    }

    #[test]
    fn leg_view_reprice_respects_shocks() {
        let leg = OptionLegView {
            spot: 100.0,
            strike: 100.0,
            tau_years: 0.25,
            iv: 0.4,
            rate: 0.0,
            flavour: Flavour::Call,
            american: false,
        };
        let base = leg.mark();
        assert!(leg.reprice(0.10, 0.0) > base, "up-spot helps long call");
        assert!(leg.reprice(0.0, 0.10) > base, "up-vol helps long call");
        assert!(leg.reprice(-0.10, -0.50) < base);
    }

    #[test]
    fn american_leg_dominates_european_leg() {
        // r > 0: an American put leg marks above the European leg.
        let euro = OptionLegView {
            spot: 90.0,
            strike: 100.0,
            tau_years: 0.5,
            iv: 0.3,
            rate: 0.05,
            flavour: Flavour::Put,
            american: false,
        };
        let amer = OptionLegView {
            american: true,
            ..euro
        };
        assert!(
            amer.mark() > euro.mark(),
            "american {} must exceed european {}",
            amer.mark(),
            euro.mark()
        );
        // Scenario repricing stays monotone in the adverse shock.
        assert!(amer.reprice(-0.20, 0.0) > amer.mark());
    }
}
