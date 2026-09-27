//! # poc-oracle
//!
//! Defense-in-depth price oracles: multi-provider **median** aggregation,
//! **staleness** windows, and **deviation quarantine**, with time-weighted
//! average prices for funding indices and option settlement.
//!
//! ## Threat model
//!
//! A single compromised feed must not be able to move the mark used for
//! liquidations and funding. Chainlink-style layered defenses:
//!
//! 1. **Median across providers** — an outlier cannot drag the mark.
//! 2. **Staleness** — providers that stop updating are excluded.
//! 3. **Deviation quarantine** — a provider that deviates from the last
//!    accepted mark beyond `max_deviation_bps` is quarantined until it
//!    returns within range.
//! 4. **Quorum** — if fewer than `min_providers` healthy providers remain,
//!    the mark goes stale (`None`) and the engine **halts** new activity on
//!    that underlying (fail-safe, trading resumes on quorum recovery).
//!
//! TWAP windows are ring-buffered so manipulation requires sustained
//! capital across the whole window, not a single print.

use std::collections::BTreeMap;

use poc_core::TimestampMs;

/// Oracle hardening configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleConfig {
    /// Maximum age of a provider price to count as fresh.
    pub staleness_ms: TimestampMs,
    /// Deviation from the last accepted mark (bps) beyond which a provider
    /// is quarantined.
    pub max_deviation_bps: u64,
    /// Minimum healthy providers required for a valid mark.
    pub min_providers: usize,
    /// Maximum number of mark samples retained for TWAP.
    pub max_samples: usize,
}

impl Default for OracleConfig {
    fn default() -> Self {
        Self {
            staleness_ms: 60_000,
            max_deviation_bps: 500, // 5%
            min_providers: 2,
            max_samples: 3_600,
        }
    }
}

/// A single provider observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPrice {
    /// Observed price in quote minor units per `1.0` base.
    pub value_quote_minor: u128,
    /// Observation timestamp (ms).
    pub ts: TimestampMs,
}

/// A fresh observation tagged with its provider name (cluster unit).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observation {
    provider: String,
    value_quote_minor: u128,
    ts: TimestampMs,
}

/// Outcome of a provider update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Provider accepted; the recomputed mark (if quorum held).
    Accepted(Option<u128>),
    /// Provider rejected and quarantined for deviating too far.
    Quarantined {
        /// The rejected observation.
        value: u128,
        /// The last accepted mark it deviated from.
        last_mark: u128,
    },
}

/// Per-underlying oracle aggregator.
pub struct AssetOracle {
    base_symbol: String,
    config: OracleConfig,
    providers: BTreeMap<String, ProviderPrice>,
    /// Reporting view of the providers currently outside the authoritative
    /// consensus cluster (soft quarantine: their latest observations are
    /// retained in `providers` and they rejoin automatically).
    quarantined: BTreeMap<String, ProviderPrice>,
    /// Accepted mark history for TWAP: `(ts, mark)`.
    marks: std::collections::VecDeque<(TimestampMs, u128)>,
    last_mark: Option<u128>,
}

impl AssetOracle {
    /// Create an oracle for `base_symbol` (e.g. `"BTC"`).
    #[must_use]
    pub fn new(base_symbol: impl Into<String>, config: OracleConfig) -> Self {
        Self {
            base_symbol: base_symbol.into(),
            config,
            providers: BTreeMap::new(),
            quarantined: BTreeMap::new(),
            marks: std::collections::VecDeque::new(),
            last_mark: None,
        }
    }

    /// Underlying base symbol.
    #[must_use]
    pub fn base_symbol(&self) -> &str {
        &self.base_symbol
    }

    /// Last accepted mark (quote minor per base), if any.
    #[must_use]
    pub fn last_mark(&self) -> Option<u128> {
        self.last_mark
    }

    /// Healthy (fresh, authoritative-cluster) provider count.
    #[must_use]
    pub fn healthy_providers(&self, now: TimestampMs) -> usize {
        let fresh = self.fresh_sorted(now);
        self.select_mark(&fresh)
            .map_or(0, |(_, auth, _)| auth.len())
    }

    /// Fresh provider observations, sorted by value (name as tie-break).
    fn fresh_sorted(&self, now: TimestampMs) -> Vec<Observation> {
        let mut out: Vec<Observation> = self
            .providers
            .iter()
            .filter(|(_, p)| now.saturating_sub(p.ts) <= self.config.staleness_ms)
            .map(|(name, p)| Observation {
                provider: name.clone(),
                value_quote_minor: p.value_quote_minor,
                ts: p.ts,
            })
            .collect();
        out.sort_by(|a, b| {
            a.value_quote_minor
                .cmp(&b.value_quote_minor)
                .then_with(|| a.provider.cmp(&b.provider))
        });
        out
    }

    /// Split sorted observations into coherence clusters: an adjacent gap
    /// wider than `max_deviation_bps` (relative to the lower value) breaks
    /// the cluster.
    fn cluster(sorted: &[Observation], max_deviation_bps: u64) -> Vec<Vec<Observation>> {
        let mut out: Vec<Vec<Observation>> = Vec::new();
        let mut current: Vec<Observation> = Vec::new();
        for obs in sorted {
            if let Some(last) = current.last() {
                let gap = obs.value_quote_minor.saturating_sub(last.value_quote_minor);
                let exceeds = gap.checked_mul(10_000).is_some_and(|g| {
                    g > last
                        .value_quote_minor
                        .saturating_mul(u128::from(max_deviation_bps))
                });
                if exceeds {
                    out.push(std::mem::take(&mut current));
                }
            }
            current.push(Observation {
                provider: obs.provider.clone(),
                value_quote_minor: obs.value_quote_minor,
                ts: obs.ts,
            });
        }
        if !current.is_empty() {
            out.push(current);
        }
        out
    }

    /// The consensus mark: median of the authoritative cluster.
    ///
    /// Authority, in order:
    /// 1. a single coherent cluster (all fresh providers within the
    ///    deviation band) with quorum — the common case;
    /// 2. the cluster nearest the last accepted mark, if it has quorum —
    ///    a single rogue print cannot drag the mark;
    /// 3. the largest quorate cluster (ties: newest observation, then the
    ///    lower median) — every provider moved together, a genuine market
    ///    jump must be accepted, not suppressed.
    ///
    /// Without a quorate cluster the mark is `None`: the venue halts on
    /// that underlying (fail-safe), which is the correct response to a
    /// split the oracle cannot resolve.
    fn select_mark(&self, fresh: &[Observation]) -> Option<(u128, Vec<String>, Vec<String>)> {
        if fresh.len() < self.config.min_providers {
            return None;
        }
        let clusters = Self::cluster(fresh, self.config.max_deviation_bps);
        if clusters.len() == 1 {
            return Some((
                median(
                    &fresh
                        .iter()
                        .map(|o| o.value_quote_minor)
                        .collect::<Vec<_>>(),
                ),
                fresh.iter().map(|o| o.provider.clone()).collect(),
                Vec::new(),
            ));
        }

        let quorate: Vec<&Vec<Observation>> = clusters
            .iter()
            .filter(|c| c.len() >= self.config.min_providers)
            .collect();
        if quorate.is_empty() {
            return None;
        }

        let cluster_median = |c: &Vec<Observation>| -> u128 {
            median(&c.iter().map(|o| o.value_quote_minor).collect::<Vec<_>>())
        };

        // Rule 2: continuity — prefer the cluster nearest the last mark.
        if let Some(winner) = candidates_of(&quorate, self.last_mark, cluster_median) {
            let names: Vec<String> = winner.iter().map(|o| o.provider.clone()).collect();
            let losers: Vec<String> = clusters
                .iter()
                .filter(|c| !std::ptr::eq(*c, winner))
                .flat_map(|c| c.iter().map(|o| o.provider.clone()).collect::<Vec<_>>())
                .collect();
            return Some((cluster_median(winner), names, losers));
        }

        // Rule 3: everyone moved together — largest quorate cluster wins.
        let mut ranked = quorate.clone();
        ranked.sort_by(|a, b| {
            b.len()
                .cmp(&a.len())
                .then_with(|| {
                    let ta = a.iter().map(|o| o.ts).max().unwrap_or(0);
                    let tb = b.iter().map(|o| o.ts).max().unwrap_or(0);
                    tb.cmp(&ta)
                })
                .then_with(|| cluster_median(a).cmp(&cluster_median(b)))
        });
        let winner = ranked.first().copied()?;
        let names: Vec<String> = winner.iter().map(|o| o.provider.clone()).collect();
        let losers: Vec<String> = clusters
            .iter()
            .filter(|c| !std::ptr::eq(*c, winner))
            .flat_map(|c| c.iter().map(|o| o.provider.clone()).collect::<Vec<_>>())
            .collect();
        Some((cluster_median(winner), names, losers))
    }

    /// Record a provider observation and recompute the mark.
    ///
    /// Observations are always retained (soft quarantine): a provider that
    /// rejoins consensus is reinstated automatically on its next print,
    /// with no operator action and no liveness cliff.
    pub fn update(
        &mut self,
        provider: &str,
        ts: TimestampMs,
        value_quote_minor: u128,
    ) -> UpdateOutcome {
        let obs = ProviderPrice {
            value_quote_minor,
            ts,
        };
        self.providers.insert(provider.into(), obs);

        let fresh = self.fresh_sorted(ts);
        match self.select_mark(&fresh) {
            Some((mark, authoritative, outliers)) => {
                self.last_mark = Some(mark);
                self.push_mark(ts, mark);
                // Refresh the reporting view of the consensus split.
                self.quarantined.clear();
                for name in &outliers {
                    if let Some(p) = self.providers.get(name) {
                        self.quarantined.insert(name.clone(), *p);
                    }
                }
                if authoritative.iter().any(|n| n == provider) {
                    UpdateOutcome::Accepted(Some(mark))
                } else {
                    UpdateOutcome::Quarantined {
                        value: value_quote_minor,
                        last_mark: mark,
                    }
                }
            }
            None => UpdateOutcome::Accepted(self.last_mark),
        }
    }

    fn push_mark(&mut self, ts: TimestampMs, mark: u128) {
        // Collapse duplicate timestamps (keep latest value).
        if let Some(back) = self.marks.back_mut() {
            if back.0 == ts {
                back.1 = mark;
                return;
            }
        }
        self.marks.push_back((ts, mark));
        while self.marks.len() > self.config.max_samples {
            self.marks.pop_front();
        }
    }

    /// Current mark at time `now`, or `None` if quorum is lost.
    #[must_use]
    pub fn mark(&self, now: TimestampMs) -> Option<u128> {
        let fresh = self.fresh_sorted(now);
        self.select_mark(&fresh).map(|(mark, _, _)| mark)
    }

    /// Time-weighted average mark over `[now - window, now]`.
    ///
    /// Samples are step functions held until the next sample; segments are
    /// clipped to the window. Returns `None` with insufficient history.
    #[must_use]
    pub fn twap(&self, now: TimestampMs, window_ms: TimestampMs) -> Option<u128> {
        if self.marks.is_empty() || window_ms == 0 {
            return None;
        }
        let start = now.saturating_sub(window_ms);
        let first_ts = self.marks.front()?.0;
        if first_ts > now {
            return None;
        }

        // Build clipped segments [seg_start, seg_end) with the leading price
        // of the first sample inside the window extended backwards.
        let mut weighted: u128 = 0;
        let mut total: u128 = 0;
        let mut iter = self.marks.iter().peekable();
        while let Some(&(t, p)) = iter.next() {
            let next_t = iter.peek().map_or(now, |&&(nt, _)| nt.min(now));
            let seg_end = next_t.max(t); // degenerate guard
            let (a, b) = (t.max(start), seg_end.max(start));
            if b > a {
                let dur = u128::from(b - a);
                weighted = weighted.saturating_add(p.saturating_mul(dur));
                total = total.saturating_add(dur);
            }
            if t > now {
                break;
            }
        }

        // If the history starts inside the window, extend the first price
        // backwards to the window start (standard step-TWAP behavior).
        if first_ts > start {
            let extend = u128::from(first_ts - start);
            if let Some(&(_, p)) = self.marks.front() {
                weighted = weighted.saturating_add(p.saturating_mul(extend));
                total = total.saturating_add(extend);
            }
        }

        if total == 0 {
            return None;
        }
        Some(weighted / total)
    }
}

/// Pick the quorate cluster nearest the last accepted mark, if any.
fn candidates_of<'a>(
    quorate: &[&'a Vec<Observation>],
    last_mark: Option<u128>,
    cluster_median: impl Fn(&Vec<Observation>) -> u128,
) -> Option<&'a Vec<Observation>> {
    let mark = last_mark?;
    quorate
        .iter()
        .copied()
        .min_by_key(|c| cluster_median(c).abs_diff(mark))
}

/// Median of a non-empty sorted list; even counts average the middle pair
/// (rounded down).
fn median(sorted: &[u128]) -> u128 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        let (a, b) = (sorted[n / 2 - 1], sorted[n / 2]);
        (a / 2).saturating_add(b / 2) + ((a % 2) + (b % 2)) / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> AssetOracle {
        AssetOracle::new("BTC", OracleConfig::default())
    }

    #[test]
    fn median_aggregation_odd_and_even() {
        let mut o = oracle();
        // One provider alone cannot form a quorum.
        assert_eq!(o.update("a", 100, 1_000), UpdateOutcome::Accepted(None));
        // Even count: median(1000, 1020) = 1010.
        assert_eq!(
            o.update("b", 101, 1_020),
            UpdateOutcome::Accepted(Some(1_010))
        );
        // Odd count: median(1000, 1005, 1020) = 1005.
        assert_eq!(
            o.update("c", 102, 1_005),
            UpdateOutcome::Accepted(Some(1_005))
        );
        assert_eq!(o.mark(102), Some(1_005));
    }

    #[test]
    fn staleness_excludes_provider() {
        let mut o = oracle();
        o.update("a", 100, 1_000);
        o.update("b", 100, 1_000);
        o.update("a", 200, 1_000);
        o.update("b", 200, 1_010);
        assert_eq!(o.mark(200), Some(1_005));
        // At t=200+61s, both stale (default 60s window) -> quorum lost.
        assert_eq!(o.mark(200 + 61_000), None);
        // One fresh provider is not enough (the other is stale).
        o.update("a", 61_300, 999);
        assert_eq!(o.mark(61_300), None, "one provider cannot form a mark");
    }

    #[test]
    fn deviation_quarantine_and_recovery() {
        let mut o = oracle();
        o.update("a", 100, 1_000);
        o.update("b", 100, 1_000);
        o.update("c", 100, 1_000);
        // A rogue provider prints 2x: >5% deviation -> quarantined.
        match o.update("rogue", 101, 2_000) {
            UpdateOutcome::Quarantined { .. } => {}
            other => panic!("expected quarantine, got {other:?}"),
        }
        // Mark unaffected: median of the three honest providers.
        assert_eq!(o.mark(101), Some(1_000));
        // Rogue returns within range and is reinstated.
        o.update("rogue", 102, 1_020);
        assert!(o.mark(102).is_some());
    }

    #[test]
    fn twap_constant_and_step() {
        let mut o = oracle();
        o.update("a", 1_000, 100);
        o.update("b", 1_000, 100);
        // Mark held at 100 for the whole window.
        assert_eq!(o.twap(2_000, 1_000), Some(100));
        o.update("a", 1_500, 104);
        o.update("b", 1_500, 104);
        // 500ms at 100, 500ms at 104 -> TWAP = 102.
        assert_eq!(o.twap(2_000, 1_000), Some(102));
        // Window larger than history: step-extension backwards from t=1000.
        // 1000ms at 100 (extension) + 500ms@100 + 500ms@104 -> 101.
        assert_eq!(o.twap(2_000, 2_000), Some(101));
    }

    #[test]
    fn twap_insufficient_history() {
        let o = oracle();
        assert_eq!(o.twap(1_000, 100), None);
    }

    #[test]
    fn genuine_jump_accepted_when_all_providers_move() {
        let mut o = oracle();
        o.update("pyth", 100, 8_000_000);
        o.update("chainlink", 100, 8_000_000);
        assert_eq!(o.mark(100), Some(8_000_000));
        // A genuine +15% market jump with every provider agreeing must be
        // accepted, not suppressed by the deviation band.
        assert_eq!(
            o.update("pyth", 200, 9_200_000),
            UpdateOutcome::Accepted(Some(8_000_000)) // transient: quorum reforming
        );
        assert_eq!(
            o.update("chainlink", 200, 9_200_000),
            UpdateOutcome::Accepted(Some(9_200_000))
        );
        assert_eq!(o.mark(200), Some(9_200_000));
    }

    #[test]
    fn split_cluster_keeps_continuity_with_quorate_old_level() {
        let mut o = oracle();
        o.update("a", 100, 1_000);
        o.update("b", 100, 1_000);
        o.update("c", 100, 1_000);
        o.update("d", 100, 1_000);
        // Two providers print 5x: a real split, but the old level still has
        // a quorum and continuity wins.
        o.update("c", 110, 5_000);
        o.update("d", 111, 5_000);
        assert_eq!(o.mark(111), Some(1_000));
        // The old level ages out: the moved cluster takes over cleanly.
        assert_eq!(o.mark(110 + 61_000), None);
        o.update("c", 61_400, 5_000);
        o.update("d", 61_400, 5_000);
        assert_eq!(o.mark(61_400), Some(5_000));
    }

    #[test]
    fn two_provider_disagreement_loses_quorum_fail_safe() {
        let mut o = oracle();
        o.update("a", 100, 1_000);
        o.update("b", 100, 1_000);
        // With only two providers, a split cannot be adjudicated: no
        // cluster holds quorum, so the mark drops (venue halts that
        // underlying — fail-safe) and neither side is blamed.
        assert_eq!(
            o.update("b", 110, 2_500),
            UpdateOutcome::Accepted(Some(1_000)) // last known mark reported
        );
        assert_eq!(o.mark(110), None);
        // The rogue returns within range: quorum reforms immediately.
        o.update("b", 120, 1_010);
        assert_eq!(o.mark(120), Some(1_005));
    }
}
