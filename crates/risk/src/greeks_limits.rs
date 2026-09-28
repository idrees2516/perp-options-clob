//! Portfolio greeks limits (G-41): vega and gamma caps per subaccount.
//!
//! Portfolio margin prices option books off a surface; a desk that is
//! margin-clean can still carry vega that the venue does not want
//! (concentrated volatility risk that liquidates violently on a vol
//! spike). Every professional options venue (Deribit PM limits, Derive
//! V3 risk universes) caps per-account greeks on top of margin. This
//! module is the check; the engine calls it pre-trade with the
//! portfolio's current greeks and the order's worst-case increment.

/// Portfolio greeks limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GreeksLimits {
    /// Maximum absolute portfolio vega, quote minor per unit of vol
    /// (`dP/dsigma`, the same units `Engine::greeks_view` reports;
    /// `0` disables).
    pub max_abs_vega_quote_minor_per_pct: i128,
    /// Maximum absolute portfolio gamma, quote minor per unit of
    /// relative spot move (`d2P/dS2` scaled to the lot; `0` disables).
    pub max_abs_gamma_quote_minor_per_pct: i128,
}

/// A greeks-limit rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GreeksRejection {
    /// Absolute vega cap exceeded.
    VegaCap {
        /// Resulting absolute vega.
        would_be: i128,
        /// The cap.
        cap: i128,
    },
    /// Absolute gamma cap exceeded.
    GammaCap {
        /// Resulting absolute gamma.
        would_be: i128,
        /// The cap.
        cap: i128,
    },
}

impl std::fmt::Display for GreeksRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GreeksRejection::VegaCap { would_be, cap } => {
                write!(f, "vega cap exceeded: {would_be} > {cap}")
            }
            GreeksRejection::GammaCap { would_be, cap } => {
                write!(f, "gamma cap exceeded: {would_be} > {cap}")
            }
        }
    }
}

impl std::error::Error for GreeksRejection {}

impl GreeksLimits {
    /// Whether both caps are disabled.
    #[must_use]
    pub fn disabled(&self) -> bool {
        self.max_abs_vega_quote_minor_per_pct == 0 && self.max_abs_gamma_quote_minor_per_pct == 0
    }

    /// Check a hypothetical post-order portfolio.
    ///
    /// * `current_*` — the account's greeks right now.
    /// * `delta_*` — the order's worst-case increment (signed).
    pub fn check(
        &self,
        current_vega: i128,
        current_gamma: i128,
        delta_vega: i128,
        delta_gamma: i128,
    ) -> Result<(), GreeksRejection> {
        if self.disabled() {
            return Ok(());
        }
        let vega_after = current_vega.saturating_add(delta_vega).abs();
        let gamma_after = current_gamma.saturating_add(delta_gamma).abs();
        if self.max_abs_vega_quote_minor_per_pct != 0
            && vega_after > self.max_abs_vega_quote_minor_per_pct
        {
            return Err(GreeksRejection::VegaCap {
                would_be: vega_after,
                cap: self.max_abs_vega_quote_minor_per_pct,
            });
        }
        if self.max_abs_gamma_quote_minor_per_pct != 0
            && gamma_after > self.max_abs_gamma_quote_minor_per_pct
        {
            return Err(GreeksRejection::GammaCap {
                would_be: gamma_after,
                cap: self.max_abs_gamma_quote_minor_per_pct,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default_allows_anything() {
        let l = GreeksLimits::default();
        assert!(l.check(1, 2, 3, 4).is_ok());
    }

    #[test]
    fn vega_cap_blocks_increment() {
        let l = GreeksLimits {
            max_abs_vega_quote_minor_per_pct: 1_000,
            max_abs_gamma_quote_minor_per_pct: 0,
        };
        // Current 600 + increment 500 = 1100 > 1000.
        assert_eq!(
            l.check(600, 0, 500, 0).unwrap_err(),
            GreeksRejection::VegaCap {
                would_be: 1100,
                cap: 1000
            }
        );
        // Reducing vega is always allowed.
        assert!(l.check(600, 0, -500, 0).is_ok());
        // Exactly at the cap passes.
        assert!(l.check(500, 0, 500, 0).is_ok());
    }

    #[test]
    fn gamma_cap_blocks_and_sign_matters_only_via_abs() {
        let l = GreeksLimits {
            max_abs_vega_quote_minor_per_pct: 0,
            max_abs_gamma_quote_minor_per_pct: 200,
        };
        assert!(l.check(0, 150, 40, 0).is_ok());
        assert!(matches!(
            l.check(0, -150, 0, -60).unwrap_err(),
            GreeksRejection::GammaCap {
                would_be: 210,
                cap: 200
            }
        ));
        // A short order reducing a long gamma book is fine.
        assert!(l.check(0, 180, 0, -100).is_ok());
    }
}
