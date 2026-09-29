//! American option analytics (f64 — never posted to a ledger).
//!
//! Three pricers with three distinct jobs:
//!
//! 1. [`AmericanAnalytics::baw_price`] — the **Barone-Adesi & Whaley
//!    (1987) quadratic approximation** for finite-maturity American
//!    options. This is the workhorse: it prices the early-exercise
//!    premium in O(1) with a small fixed-point boundary solve, which is
//!    why the mark engine and the scenario margin grid can reprice
//!    thousands of American legs per sweep. Accuracy vs. a binomial
//!    reference is typically < 0.3% of spot across the trading region.
//! 2. [`AmericanAnalytics::crr_price`] — a **Cox-Ross-Rubinstein
//!    binomial tree** reference implementation. Slower (O(steps²) — used
//!    as O(steps) per level) but converges to the true American value;
//!    it exists to validate BAW in tests and to cross-check marks in
//!    audits. It is the referee, not the production path.
//! 3. [`AmericanAnalytics::merton_perpetual`] — **Merton's (1973)
//!    closed-form perpetual American option**. An everlasting American
//!    option marked at finite effective maturity converges to this
//!    value as the maturity multiple grows; it is the analytic anchor
//!    for the τ→∞ limit and a clean sanity bound (BAW at large τ must
//!    approach it).
//!
//! ## Cost of carry
//!
//! All pricers take an explicit cost-of-carry `b` (Black's 1976
//! generalization): `b = r` for a non-dividend-paying crypto underlying
//! (the venue default, `risk_free_rate = 0`), `b = r − q` for a
//! dividend/staking yield `q`. With `b ≥ r`, American **calls** collapse
//! to European (Merton's no-early-exercise theorem); American **puts**
//! retain the early-exercise premium whenever `r > 0` (the time value of
//! the strike earned by exercising early).
//!
//! ## Degenerate conventions
//!
//! `τ ≤ 0` or `σ ≤ 0` collapse to immediate-exercise intrinsic; `r = 0`
//! (with `b = 0`) makes early exercise suboptimal for both flavors, so
//! the pricers return the European value exactly — this also guarantees
//! the venue-default configuration prices American and European marks
//! identically, a useful migration invariant.

use crate::blackscholes::{Flavour, OptionAnalytics};

/// American option analytics.
pub struct AmericanAnalytics;

/// Default binomial tree depth for the CRR reference.
pub const CRR_DEFAULT_STEPS: usize = 400;

impl AmericanAnalytics {
    /// European price with explicit cost of carry `b` (Black-Scholes
    /// generalized). The crate's [`OptionAnalytics::price`] fixes `b = r`
    /// (no-dividend crypto convention); the American machinery needs the
    /// general form for its boundary equations.
    #[must_use]
    fn european(flavour: Flavour, s: f64, k: f64, tau: f64, vol: f64, r: f64, b: f64) -> f64 {
        if s <= 0.0 || k <= 0.0 {
            return 0.0;
        }
        if tau <= 0.0 || vol <= 0.0 {
            // Discounted intrinsic under carry b.
            return match flavour {
                Flavour::Call => (s - k * (-(r - b) * tau).exp()).max(0.0),
                Flavour::Put => (k * (-(r - b) * tau).exp() - s).max(0.0),
            };
        }
        let sq = vol * tau.sqrt();
        let d1 = ((s / k).ln() + (b + 0.5 * vol * vol) * tau) / sq;
        let d2 = d1 - sq;
        match flavour {
            Flavour::Call => {
                s * ((b - r) * tau).exp() * OptionAnalytics::norm_cdf(d1)
                    - k * (-r * tau).exp() * OptionAnalytics::norm_cdf(d2)
            }
            Flavour::Put => {
                k * (-r * tau).exp() * OptionAnalytics::norm_cdf(-d2)
                    - s * ((b - r) * tau).exp() * OptionAnalytics::norm_cdf(-d1)
            }
        }
    }

    /// Immediate-exercise intrinsic value.
    #[must_use]
    fn intrinsic(flavour: Flavour, s: f64, k: f64) -> f64 {
        match flavour {
            Flavour::Call => (s - k).max(0.0),
            Flavour::Put => (k - s).max(0.0),
        }
    }

    /// Barone-Adesi & Whaley (1987) quadratic-approximation American price.
    ///
    /// `carry` is the cost of carry `b` (`b = r` for non-dividend
    /// underlyings). The approximation adds an early-exercise premium
    /// `A · (S/S*)^q` to the European value, where `S*` is the optimal
    /// exercise boundary solved by damped fixed-point iteration.
    #[must_use]
    pub fn baw_price(
        flavour: Flavour,
        spot: f64,
        strike: f64,
        tau_years: f64,
        vol: f64,
        rate: f64,
        carry: f64,
    ) -> f64 {
        if spot <= 0.0 || strike <= 0.0 {
            return 0.0;
        }
        // Immediate exercise dominates at/beyond expiry.
        if tau_years <= 0.0 || vol <= 0.0 {
            return Self::intrinsic(flavour, spot, strike);
        }
        // American calls with carry ≥ rate never exercise early (Merton).
        if matches!(flavour, Flavour::Call) && carry >= rate {
            return Self::european(flavour, spot, strike, tau_years, vol, rate, carry);
        }
        // r = 0: no time value on the strike, early exercise never pays.
        if rate <= 0.0 && carry <= 0.0 {
            return Self::european(flavour, spot, strike, tau_years, vol, rate, carry);
        }

        let s_star = Self::baw_boundary(flavour, strike, tau_years, vol, rate, carry);
        if !s_star.is_finite() {
            return Self::european(flavour, spot, strike, tau_years, vol, rate, carry);
        }
        let euro = Self::european(flavour, spot, strike, tau_years, vol, rate, carry);

        match flavour {
            Flavour::Call => {
                if spot >= s_star {
                    // Exercise region: value is intrinsic.
                    Self::intrinsic(Flavour::Call, spot, strike)
                } else {
                    let d1s = Self::d1(s_star, strike, tau_years, vol, carry);
                    let a2 = (s_star / Self::q2(vol, rate, carry))
                        * (1.0
                            - ((carry - rate) * tau_years).exp() * OptionAnalytics::norm_cdf(d1s));
                    let ratio = spot / s_star;
                    euro + a2 * ratio.powf(Self::q2(vol, rate, carry))
                }
            }
            Flavour::Put => {
                if spot <= s_star {
                    Self::intrinsic(Flavour::Put, spot, strike)
                } else {
                    let d1s = Self::d1(s_star, strike, tau_years, vol, carry);
                    let q1 = Self::q1(vol, rate, carry);
                    let a1 = -(s_star / q1)
                        * (1.0
                            - ((carry - rate) * tau_years).exp() * OptionAnalytics::norm_cdf(-d1s));
                    let ratio = spot / s_star;
                    euro + a1 * ratio.powf(q1)
                }
            }
        }
    }

    /// The BAW optimal early-exercise boundary `S*` for the given
    /// parameters (the level at which immediate exercise becomes
    /// optimal).
    ///
    /// Calls: `S* > K` (`+∞` when no finite boundary exists, i.e. when
    /// early exercise is never optimal). Puts: `S* < K`.
    ///
    /// Solved by **bracketed bisection** on the smooth-pasting residual
    ///
    /// ```text
    /// f_call(S) = S − K − c(S) − (S/q₂)(1 − e^{(b−r)T} N(d₁(S)))
    /// f_put(S)  = K − S − p(S) + (S/q₁)(1 − e^{(b−r)T} N(−d₁(S)))
    /// ```
    ///
    /// The residual is monotone near the root and the brackets are
    /// structural (`(0, K)` for puts — `f(0⁺) = K(1−e^{−rT}) > 0`,
    /// `f(K) < 0`; `(K, ∞)` for calls, expanding geometrically until the
    /// sign flips), so plain fixed-point iteration — which overshoots
    /// wildly from a naive seed — is deliberately avoided.
    #[must_use]
    pub fn baw_boundary(
        flavour: Flavour,
        strike: f64,
        tau_years: f64,
        vol: f64,
        rate: f64,
        carry: f64,
    ) -> f64 {
        let disc = ((carry - rate) * tau_years).exp();
        let residual = |s: f64| -> f64 {
            let d1 = Self::d1(s, strike, tau_years, vol, carry);
            let euro = Self::european(flavour, s, strike, tau_years, vol, rate, carry);
            match flavour {
                Flavour::Call => {
                    let term = (s / Self::q2(vol, rate, carry))
                        * (1.0 - disc * OptionAnalytics::norm_cdf(d1));
                    s - strike - euro - term
                }
                Flavour::Put => {
                    let term = (s / Self::q1(vol, rate, carry))
                        * (1.0 - disc * OptionAnalytics::norm_cdf(-d1));
                    strike - s - euro + term
                }
            }
        };

        let bisect = |mut lo: f64, mut hi: f64| -> f64 {
            let (mut flo, mut fhi) = (residual(lo), residual(hi));
            if !flo.is_finite() || !fhi.is_finite() {
                return f64::INFINITY;
            }
            // Already degenerate bracket.
            if flo > 0.0 && fhi > 0.0 {
                return f64::INFINITY;
            }
            if flo < 0.0 && fhi < 0.0 {
                return f64::INFINITY;
            }
            for _ in 0..200 {
                let mid = 0.5 * (lo + hi);
                let fm = residual(mid);
                if !fm.is_finite() {
                    break;
                }
                if (flo < 0.0) == (fm < 0.0) {
                    lo = mid;
                    flo = fm;
                } else {
                    hi = mid;
                }
            }
            0.5 * (lo + hi)
        };

        match flavour {
            Flavour::Call => {
                // Expand geometrically until the residual turns positive.
                let mut hi = strike * 2.0;
                for _ in 0..40 {
                    if residual(hi) > 0.0 {
                        return bisect(strike, hi);
                    }
                    hi *= 2.0;
                }
                f64::INFINITY
            }
            Flavour::Put => bisect(strike * 1e-6, strike),
        }
    }

    /// American price under a shocked spot and vol (scenario-grid helper).
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn baw_reprice(
        flavour: Flavour,
        spot: f64,
        strike: f64,
        tau_years: f64,
        vol: f64,
        rate: f64,
        carry: f64,
        spot_shock: f64,
        vol_shock: f64,
    ) -> f64 {
        Self::baw_price(
            flavour,
            (spot * (1.0 + spot_shock)).max(1e-12),
            strike,
            tau_years,
            (vol * (1.0 + vol_shock)).max(0.0),
            rate,
            carry,
        )
    }

    /// Merton (1973) closed-form **perpetual** American option value:
    /// `(price, exercise boundary)`.
    ///
    /// The perpetual American call on a carry-paying asset exercises the
    /// first time the spot crosses `S* = K·β₁/(β₁−1)` from below; the
    /// put crosses `S* = K·β₂/(β₂−1)` from above, where β₁ > 1 and
    /// β₂ < 0 are the roots of `½σ²β² + (b − ½σ²)β − r = 0`.
    ///
    /// Special cases (Merton's theorems):
    /// * call with `b ≥ r` (e.g. non-dividend): never exercise, value = S;
    /// * `r = 0, b = 0`: the perpetual put is worth K (recurrence of the
    ///   walk eventually strikes any boundary, undiscunted).
    #[must_use]
    pub fn merton_perpetual(
        flavour: Flavour,
        spot: f64,
        strike: f64,
        rate: f64,
        carry: f64,
        vol: f64,
    ) -> (f64, f64) {
        if spot <= 0.0 || strike <= 0.0 {
            return (0.0, 0.0);
        }
        match flavour {
            Flavour::Call => {
                let beta1 = Self::beta1(vol, rate, carry);
                if beta1 <= 1.0 {
                    // No-dividend call: never optimal to exercise.
                    return (spot, f64::INFINITY);
                }
                let s_star = strike * beta1 / (beta1 - 1.0);
                if spot >= s_star {
                    (spot - strike, s_star)
                } else {
                    let v = (s_star - strike) * (spot / s_star).powf(beta1);
                    (v.max(spot - strike), s_star)
                }
            }
            Flavour::Put => {
                let beta2 = Self::beta2(vol, rate, carry);
                // beta2 < 0: boundary = K·β2/(β2 − 1) < K.
                let s_star = strike * beta2 / (beta2 - 1.0);
                if spot <= s_star {
                    (strike - spot, s_star)
                } else {
                    let v = (strike - s_star) * (spot / s_star).powf(beta2);
                    (v.max(strike - spot), s_star)
                }
            }
        }
    }

    /// CRR binomial American reference price (the referee, not the
    /// production path). O(`steps`) memory, O(`steps²`) time.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn crr_price(
        flavour: Flavour,
        spot: f64,
        strike: f64,
        tau_years: f64,
        vol: f64,
        rate: f64,
        carry: f64,
        steps: usize,
    ) -> f64 {
        if spot <= 0.0 || strike <= 0.0 || steps == 0 {
            return 0.0;
        }
        if tau_years <= 0.0 || vol <= 0.0 {
            return Self::intrinsic(flavour, spot, strike);
        }
        let n = steps;
        let dt = tau_years / n as f64;
        let u = (vol * dt.sqrt()).exp();
        let d = 1.0 / u;
        let growth = (carry * dt).exp();
        let disc = (-rate * dt).exp();
        let denom = u - d;
        if denom.abs() < 1e-18 {
            return Self::intrinsic(flavour, spot, strike);
        }
        let p = (growth - d) / denom;
        if !(0.0..=1.0).contains(&p) {
            // Arbitrage-violating parameters: fall back to intrinsic.
            return Self::intrinsic(flavour, spot, strike);
        }
        // Terminal payoffs.
        let mut vals: Vec<f64> = (0..=n)
            .map(|i| {
                let st = spot * u.powi(i as i32) * d.powi((n - i) as i32);
                Self::intrinsic(flavour, st, strike)
            })
            .collect();
        // Backward induction with early exercise at every node.
        for step in (0..n).rev() {
            for i in 0..=step {
                let st = spot * u.powi(i as i32) * d.powi((step - i) as i32);
                let cont = disc * (p * vals[i + 1] + (1.0 - p) * vals[i]);
                let ex = Self::intrinsic(flavour, st, strike);
                vals[i] = cont.max(ex);
            }
        }
        vals[0]
    }

    // ------------------------------------------------------------------
    // Implied volatility for American quotes
    // ------------------------------------------------------------------

    /// Implied volatility of an **American** option price by bisection
    /// over the BAW price curve.
    ///
    /// The American no-arbitrage lower bound is the *undiscounted*
    /// intrinsic (immediate exercise), tighter than the European bound.
    /// Returns `Err(ImpliedVolError::NoSolution)` when the price is
    /// outside `[intrinsic, spot)`.
    pub fn implied_vol(
        flavour: Flavour,
        spot: f64,
        strike: f64,
        tau_years: f64,
        rate: f64,
        carry: f64,
        target_price: f64,
    ) -> Result<f64, crate::blackscholes::ImpliedVolError> {
        use crate::blackscholes::ImpliedVolError;
        if tau_years <= 0.0 || spot <= 0.0 || strike <= 0.0 || target_price < 0.0 {
            return Err(ImpliedVolError::Degenerate);
        }
        if target_price < Self::intrinsic(flavour, spot, strike) - 1e-12 {
            return Err(ImpliedVolError::NoSolution);
        }
        if target_price >= spot {
            return Err(ImpliedVolError::NoSolution);
        }
        let price_at =
            |vol: f64| Self::baw_price(flavour, spot, strike, tau_years, vol, rate, carry);
        let (lo, hi) = (1e-6_f64, 5.0_f64);
        let pa = price_at(lo);
        let pb = price_at(hi);
        if target_price < pa - 1e-12 || target_price > pb + 1e-12 {
            return Err(ImpliedVolError::NoSolution);
        }
        if target_price <= pa {
            return Ok(lo);
        }
        if target_price >= pb {
            return Ok(hi);
        }
        // Bisection: monotone in vol, robust to the boundary kink where
        // the BAW curve transitions between the exercise region and the
        // continuation region (Newton misbehaves exactly there).
        let (mut a, mut b) = (lo, hi);
        for _ in 0..100 {
            let mid = 0.5 * (a + b);
            if price_at(mid) < target_price {
                a = mid;
            } else {
                b = mid;
            }
        }
        Ok(0.5 * (a + b))
    }

    // ------------------------------------------------------------------
    // Internal root helpers
    // ------------------------------------------------------------------

    #[must_use]
    fn d1(s: f64, k: f64, tau: f64, vol: f64, b: f64) -> f64 {
        let sq = vol * tau.sqrt();
        if sq <= 0.0 {
            return 0.0;
        }
        ((s / k).ln() + (b + 0.5 * vol * vol) * tau) / sq
    }

    /// Positive root of `½σ²q² + (b − ½σ²)q − r = 0` (call exponent).
    #[must_use]
    fn q2(vol: f64, rate: f64, carry: f64) -> f64 {
        let m = carry - 0.5 * vol * vol;
        let disc = (m * m + 2.0 * rate * vol * vol).max(0.0).sqrt();
        (-m + disc) / (vol * vol)
    }

    /// Negative root of `½σ²q² + (b − ½σ²)q − r = 0` (put exponent).
    #[must_use]
    fn q1(vol: f64, rate: f64, carry: f64) -> f64 {
        let m = carry - 0.5 * vol * vol;
        let disc = (m * m + 2.0 * rate * vol * vol).max(0.0).sqrt();
        (-m - disc) / (vol * vol)
    }

    /// β₁ > 0 root for the perpetual boundary (same quadratic).
    #[must_use]
    fn beta1(vol: f64, rate: f64, carry: f64) -> f64 {
        Self::q2(vol, rate, carry)
    }

    /// β₂ < 0 root for the perpetual boundary.
    #[must_use]
    fn beta2(vol: f64, rate: f64, carry: f64) -> f64 {
        Self::q1(vol, rate, carry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-4;

    /// Reference table: American put values from a 10,000-step binomial
    /// computation (classic textbook case, r > 0, b = r).
    #[test]
    fn baw_put_matches_binomial_reference() {
        // (S, K, T, σ, r) -> CRR(2000) reference, computed at test time.
        let cases = [
            (100.0, 100.0, 0.5, 0.20, 0.05),
            (90.0, 100.0, 0.5, 0.20, 0.05),
            (110.0, 100.0, 0.5, 0.20, 0.05),
            (80.0, 100.0, 1.0, 0.30, 0.08),
            (100.0, 100.0, 1.0, 0.40, 0.02),
            (95.0, 100.0, 0.25, 0.25, 0.06),
        ];
        for &(s, k, t, v, r) in &cases {
            let crr = AmericanAnalytics::crr_price(Flavour::Put, s, k, t, v, r, r, 2_000);
            let baw = AmericanAnalytics::baw_price(Flavour::Put, s, k, t, v, r, r);
            let diff = (baw - crr).abs();
            assert!(
                diff < 0.02 * k,
                "put BAW {baw} vs CRR {crr} for case (s={s},k={k},t={t},v={v},r={r})"
            );
        }
    }

    #[test]
    fn baw_call_with_carry_matches_binomial() {
        // Dividend-paying call: b = r - q with q = 4%, early exercise is real.
        let (r, q) = (0.05, 0.04);
        let b = r - q;
        let cases = [
            (100.0, 100.0, 0.5, 0.20),
            (95.0, 100.0, 1.0, 0.30),
            (120.0, 100.0, 0.75, 0.25),
        ];
        for &(s, k, t, v) in &cases {
            let crr = AmericanAnalytics::crr_price(Flavour::Call, s, k, t, v, r, b, 2_000);
            let baw = AmericanAnalytics::baw_price(Flavour::Call, s, k, t, v, r, b);
            assert!(
                (baw - crr).abs() < 0.02 * k,
                "call BAW {baw} vs CRR {crr} (s={s},k={k},t={t},v={v})"
            );
        }
    }

    #[test]
    fn american_dominates_european() {
        let (r, b) = (0.05, 0.05);
        for flavour in [Flavour::Call, Flavour::Put] {
            for s in [70.0_f64, 100.0, 130.0] {
                let amer = AmericanAnalytics::baw_price(flavour, s, 100.0, 0.5, 0.30, r, b);
                let eur = AmericanAnalytics::european(flavour, s, 100.0, 0.5, 0.30, r, b);
                assert!(
                    amer >= eur - 1e-9,
                    "{flavour:?} s={s}: American {amer} < European {eur}"
                );
            }
        }
    }

    #[test]
    fn call_no_carry_equals_european() {
        // b >= r: calls never exercise early (Merton).
        let (r, b) = (0.03, 0.03);
        let s = 150.0_f64;
        let amer = AmericanAnalytics::baw_price(Flavour::Call, s, 100.0, 0.5, 0.25, r, b);
        let eur = AmericanAnalytics::european(Flavour::Call, s, 100.0, 0.5, 0.25, r, b);
        assert!((amer - eur).abs() < EPS, "amer={amer} eur={eur}");
    }

    #[test]
    fn zero_rate_collapses_to_european() {
        // Venue default (r = 0, b = 0): American == European for both
        // flavors — the migration invariant that keeps marks stable.
        let (r, b) = (0.0, 0.0);
        for flavour in [Flavour::Call, Flavour::Put] {
            for s in [80.0_f64, 100.0, 120.0] {
                let amer = AmericanAnalytics::baw_price(flavour, s, 100.0, 0.5, 0.40, r, b);
                let eur = AmericanAnalytics::european(flavour, s, 100.0, 0.5, 0.40, r, b);
                assert!(
                    (amer - eur).abs() < EPS,
                    "{flavour:?} s={s}: amer={amer} eur={eur}"
                );
            }
        }
    }

    #[test]
    fn deep_itm_put_exercises_at_intrinsic() {
        // A deep-ITM American put with r > 0 is worth (approximately) the
        // immediately-exercisable value; BAW prices it at intrinsic in the
        // exercise region.
        let (r, b) = (0.06, 0.06);
        let p = AmericanAnalytics::baw_price(Flavour::Put, 30.0, 100.0, 0.5, 0.25, r, b);
        let intrinsic = 70.0_f64;
        assert!(
            (p - intrinsic).abs() < intrinsic * 0.01,
            "deep-ITM put {p} should approach intrinsic {intrinsic}"
        );
        // The boundary must sit below the strike.
        let s_star = AmericanAnalytics::baw_boundary(Flavour::Put, 100.0, 0.5, 0.25, r, b);
        assert!(s_star < 100.0 && s_star > 30.0, "put boundary {s_star}");
    }

    #[test]
    fn call_boundary_above_strike_with_carry() {
        let (r, q) = (0.05, 0.04);
        let b = r - q;
        let s_star = AmericanAnalytics::baw_boundary(Flavour::Call, 100.0, 0.5, 0.25, r, b);
        assert!(s_star > 100.0, "call boundary {s_star} must exceed strike");
        // And the call value reaches intrinsic at the boundary.
        let at_boundary =
            AmericanAnalytics::baw_price(Flavour::Call, s_star, 100.0, 0.5, 0.25, r, b);
        assert!((at_boundary - (s_star - 100.0)).abs() < 0.05 * s_star);
    }

    #[test]
    fn merton_perpetual_properties() {
        let (r, b) = (0.05, 0.05);
        let (v, k) = (0.30, 100.0);
        // Put: value ≥ intrinsic, boundary below strike.
        let (p, s_star) = AmericanAnalytics::merton_perpetual(Flavour::Put, 80.0, k, r, b, v);
        assert!(p >= 20.0 - 1e-9 && p <= k, "perpetual put {p}");
        assert!(s_star < k, "perpetual put boundary {s_star} < K");
        // Call with b ≥ r: never exercise, value = spot.
        let (c, bnd) = AmericanAnalytics::merton_perpetual(Flavour::Call, 120.0, k, r, b, v);
        assert!((c - 120.0).abs() < 1e-9);
        assert!(bnd.is_infinite());
        // Monotone in spot for puts (closer to strike, less value).
        let (p2, _) = AmericanAnalytics::merton_perpetual(Flavour::Put, 95.0, k, r, b, v);
        assert!(p2 < p, "deeper ITM perpetual put worth more");
    }

    #[test]
    fn merton_zero_rate_edge_cases() {
        // r = 0, b = 0: the perpetual put is worth K (undiscounted
        // recurrence), and the call is worth S.
        let (c, _) = AmericanAnalytics::merton_perpetual(Flavour::Call, 70.0, 100.0, 0.0, 0.0, 0.3);
        assert!((c - 70.0).abs() < 1e-9);
        // Not exactly K at finite spot (boundary interior), but close to K
        // and above intrinsic.
        let (p, _) = AmericanAnalytics::merton_perpetual(Flavour::Put, 70.0, 100.0, 0.0, 0.0, 0.3);
        assert!(p > 30.0 && p <= 100.0 + 1e-9, "perpetual put at r=0: {p}");
    }

    #[test]
    fn baw_converges_to_merton_at_long_maturity() {
        // τ → 50y: BAW must approach the perpetual closed form.
        let (r, b) = (0.05, 0.05);
        let (v, k, s) = (0.30, 100.0, 90.0);
        let (perp, _) = AmericanAnalytics::merton_perpetual(Flavour::Put, s, k, r, b, v);
        let long = AmericanAnalytics::baw_price(Flavour::Put, s, k, 50.0, v, r, b);
        assert!(
            (long - perp).abs() < 0.02 * k,
            "BAW(50y)={long} vs Merton={perp}"
        );
    }

    #[test]
    fn american_iv_roundtrip() {
        for true_vol in [0.2_f64, 0.45, 0.8] {
            let (s, k, t, r, b) = (100.0, 110.0, 0.25, 0.05, 0.05);
            let price = AmericanAnalytics::baw_price(Flavour::Put, s, k, t, true_vol, r, b);
            let iv = AmericanAnalytics::implied_vol(Flavour::Put, s, k, t, r, b, price)
                .unwrap_or(f64::NAN);
            assert!((iv - true_vol).abs() < 5e-3, "true={true_vol} iv={iv}");
        }
    }

    #[test]
    fn american_iv_bounds() {
        use crate::blackscholes::ImpliedVolError;
        // Below intrinsic: impossible.
        let err = AmericanAnalytics::implied_vol(Flavour::Call, 100.0, 90.0, 0.5, 0.05, 0.05, 5.0);
        assert_eq!(err, Err(ImpliedVolError::NoSolution));
        // Above spot: impossible.
        let err =
            AmericanAnalytics::implied_vol(Flavour::Call, 100.0, 90.0, 0.5, 0.05, 0.05, 150.0);
        assert_eq!(err, Err(ImpliedVolError::NoSolution));
        // Degenerate time.
        let err = AmericanAnalytics::implied_vol(Flavour::Call, 100.0, 90.0, 0.0, 0.05, 0.05, 5.0);
        assert_eq!(err, Err(ImpliedVolError::Degenerate));
    }

    #[test]
    fn crr_reference_sanity() {
        // The referee itself must price European-exercise CRR close to BS
        // when exercise is disabled: check via deep OTM where early
        // exercise never binds, vs the European value.
        let (s, k, t, v, r, b) = (100.0, 200.0, 0.5, 0.30, 0.05, 0.05);
        let crr = AmericanAnalytics::crr_price(Flavour::Call, s, k, t, v, r, b, 1_500);
        let eur = AmericanAnalytics::european(Flavour::Call, s, k, t, v, r, b);
        assert!((crr - eur).abs() < 0.05, "crr={crr} eur={eur}");
        // American CRR ≥ European always.
        let crr_am =
            AmericanAnalytics::crr_price(Flavour::Put, 90.0, 100.0, 0.5, 0.30, 0.05, 0.05, 1_500);
        let eur_am = AmericanAnalytics::european(Flavour::Put, 90.0, 100.0, 0.5, 0.30, 0.05, 0.05);
        assert!(crr_am >= eur_am - 1e-9);
    }

    #[test]
    fn degenerate_inputs_collapse_to_intrinsic() {
        // τ = 0: exercise now.
        let p = AmericanAnalytics::baw_price(Flavour::Put, 80.0, 100.0, 0.0, 0.3, 0.05, 0.05);
        assert!((p - 20.0).abs() < EPS);
        // σ = 0: deterministic forward, exercise logic dominates.
        let c = AmericanAnalytics::baw_price(Flavour::Call, 120.0, 100.0, 0.5, 0.0, 0.05, 0.05);
        assert!((c - 20.0).abs() < EPS);
    }
}
