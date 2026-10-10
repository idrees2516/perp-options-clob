//! Dealer program (audit gap G-15): the firm-quote enforcement and
//! reliability-scored dealer registry that makes the RFQ network (G-11)
//! safe to route institutional flow through.
//!
//! # Why a dealer program
//!
//! The RFQ system is a negotiation network: takers publish size, dealers
//! answer with **firm** quotes, and the taker executes against the best
//! one. The entire economic value of that network rests on one property —
//! quotes are firm while they live. A dealer who answers a request, waits
//! until the taker (or a rival) commits elsewhere, and then pulls the
//! quote is not providing liquidity; they are harvesting request patterns
//! (adverse-selection reconnaissance) at zero cost. Paradigm's network
//! solves this with program-level enforcement: dealers that pull quotes
//! after seeing request flow get degraded or dropped from the program, and
//! program membership is the admission ticket to the flow itself.
//!
//! This module formalizes that as a deterministic, event-driven state
//! machine. Every input is a journaled engine event (enroll, quote
//! pulled, quote executed, quote lapsed), every output is derived from the
//! event history, and the suspension ladder is pure arithmetic on
//! timestamps — so a journal replay reconstructs program state bit for bit.
//!
//! # The ladder
//!
//! `pull` (cancel of a live quote on an open RFQ) → **strike**. Strikes
//! inside the rolling window ≥ threshold → **suspension** (cannot quote,
//! RFQ routing skips them). Each suspension doubles the next one. After
//! `drop_after_suspensions` suspensions → **dropped** (permanent removal;
//! enrollment again is an explicit governance act). Lapses (quotes that
//! simply expired unexecuted) never suspend — expiry is a market condition,
//! not bad faith — but they erode the reliability score that gates tier
//! promotion.
//!
//! # Tiers
//!
//! Tier is earned by executed volume **and** a reliability floor:
//! `Basic -> Silver -> Gold`. Tier is the routing priority the venue owns:
//! directed RFQs surface first to Gold, then Silver; the tier is the
//! strongest incentive the venue can grant and it costs nothing.

use std::collections::BTreeMap;

use poc_core::SubaccountId;

/// Dealer program tier — earned reliability + volume class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DealerTier {
    /// Entry tier: enrolled, may quote open-to-all RFQs.
    Basic,
    /// Earned: priority routing below Gold.
    Silver,
    /// Earned: first look at directed RFQ flow.
    Gold,
}

impl DealerTier {
    /// Rank used for routing order (higher = earlier look).
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            DealerTier::Basic => 0,
            DealerTier::Silver => 1,
            DealerTier::Gold => 2,
        }
    }
}

/// Why a dealer cannot currently quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgramGate {
    /// The account is not enrolled in the dealer program.
    NotEnrolled,
    /// Suspended until the stored timestamp (engine wall-clock, ms).
    SuspendedUntil(TimestampMsAlias),
    /// Permanently removed from the program.
    Dropped,
}

/// Re-exported timestamp alias keeps the public surface tidy.
pub type TimestampMsAlias = u64;

/// The program's tunable policy. All defaults are venue-grade; every field
/// is validated by [`ProgramConfig::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramConfig {
    /// Rolling window in which strikes accumulate toward a suspension.
    pub strike_window_ms: u64,
    /// Strikes inside the window that trip the next suspension.
    pub strike_threshold: u32,
    /// Length of the first suspension.
    pub suspension_ms: u64,
    /// Multiplier applied to each subsequent suspension (>= 1).
    pub suspension_multiplier: u32,
    /// Suspensions after which the dealer is dropped permanently.
    pub drop_after_suspensions: u32,
    /// Weight of one lapsed quote in the reliability score, per-mille of
    /// one execution (0..=1000).
    pub lapse_weight_permille: u32,
    /// Cumulative executed lots that promote to Silver (with score floor).
    pub silver_executed_lots: u128,
    /// Cumulative executed lots that promote to Gold (with score floor).
    pub gold_executed_lots: u128,
    /// Minimum reliability score (per-mille) to hold Silver or Gold.
    pub tier_score_floor_permille: u32,
}

impl Default for ProgramConfig {
    fn default() -> Self {
        Self {
            strike_window_ms: 3_600_000,
            strike_threshold: 3,
            suspension_ms: 3_600_000,
            suspension_multiplier: 2,
            drop_after_suspensions: 3,
            lapse_weight_permille: 100,
            silver_executed_lots: 5_000,
            gold_executed_lots: 50_000,
            tier_score_floor_permille: 700,
        }
    }
}

impl ProgramConfig {
    /// Validate the policy. Returns `Err(reason)` for any field that would
    /// make the ladder non-monotone or the score meaningless.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.strike_window_ms == 0 {
            return Err("strike window must be positive");
        }
        if self.strike_threshold == 0 {
            return Err("strike threshold must be positive");
        }
        if self.suspension_ms == 0 {
            return Err("suspension length must be positive");
        }
        if self.suspension_multiplier == 0 {
            return Err("suspension multiplier must be >= 1");
        }
        if self.drop_after_suspensions == 0 {
            return Err("drop threshold must be positive");
        }
        if self.lapse_weight_permille > 1_000 {
            return Err("lapse weight must be <= 1000 permille");
        }
        if self.silver_executed_lots >= self.gold_executed_lots {
            return Err("silver threshold must be below gold");
        }
        if self.tier_score_floor_permille > 1_000 {
            return Err("tier score floor must be <= 1000 permille");
        }
        Ok(())
    }
}

/// One dealer's program record — pure derived state, rebuilt exactly by
/// replaying enroll/pull/execute/lapse events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DealerRecord {
    /// The dealer's subaccount.
    pub dealer: SubaccountId,
    /// Current earned tier (never demoted by inactivity alone; demotion is
    /// an explicit governance event, not a scoring side effect).
    pub tier: DealerTier,
    /// Enrollment timestamp.
    pub enrolled_ts: u64,
    /// Timestamps of recorded pulls (strikes), ascending.
    pub strikes: Vec<u64>,
    /// Executed quote count.
    pub executions: u64,
    /// Lapsed quote count.
    pub lapses: u64,
    /// Cumulative executed lots.
    pub executed_lots: u128,
    /// Suspensions so far.
    pub suspension_count: u32,
    /// Suspended-until timestamp (0 = not suspended).
    pub suspended_until: u64,
    /// Permanently dropped.
    pub dropped: bool,
}

impl DealerRecord {
    /// Reliability score in per-mille of executions against everything
    /// that should have been firm: `1000 * ex / (ex + pulls + w * lapses)`.
    /// A dealer with no history scores a perfect 1000 (innocent until
    /// struck); the floor only bites once there is history.
    #[must_use]
    pub fn score_permille(&self, config: &ProgramConfig) -> u32 {
        let ex = self.executions;
        let w_lapses = self
            .lapses
            .saturating_mul(u64::from(config.lapse_weight_permille))
            / 1_000;
        let denom = ex + self.strikes.len() as u64 + w_lapses;
        if denom == 0 {
            return 1_000;
        }
        let score = ex * 1_000 / denom;
        u32::try_from(score).unwrap_or(1_000)
    }

    /// The tier this dealer currently holds (explicit tier, not the
    /// promotion ladder — see [`DealerProgram::earned_tier`]).
    #[must_use]
    pub fn held_tier(&self) -> DealerTier {
        self.tier
    }
}

/// The outcome of recording a pull — what the caller must journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    /// Strike recorded; nothing else changed.
    Strike,
    /// Strike recorded AND the dealer is now suspended until `ts`.
    Suspended(u64),
    /// Strike recorded AND the dealer is now permanently dropped.
    Dropped,
}

/// The outcome of recording an execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecuteOutcome {
    /// Recorded; nothing else changed.
    Recorded,
    /// Recorded AND the dealer earned a tier promotion.
    Promoted(DealerTier),
}

/// The deterministic dealer program state machine.
///
/// Pure state: the engine journals `DealerEnrolled` / `DealerPulled` /
/// `DealerExecuted` / `DealerLapsed` / `DealerSuspended` / `DealerDropped`
/// events on apply and mirrors each into these methods, so replay
/// reconstructs the registry exactly.
#[derive(Debug)]
pub struct DealerProgram {
    config: ProgramConfig,
    dealers: BTreeMap<SubaccountId, DealerRecord>,
}

impl Default for DealerProgram {
    fn default() -> Self {
        let config = ProgramConfig::default();
        debug_assert!(config.validate().is_ok());
        Self {
            config,
            dealers: BTreeMap::new(),
        }
    }
}

impl DealerProgram {
    /// New program under a validated policy.
    pub fn new(config: ProgramConfig) -> Result<Self, &'static str> {
        config.validate()?;
        Ok(Self {
            config,
            dealers: BTreeMap::new(),
        })
    }

    /// The active policy.
    #[must_use]
    pub fn config(&self) -> &ProgramConfig {
        &self.config
    }

    /// A dealer's record, if enrolled.
    #[must_use]
    pub fn record(&self, dealer: SubaccountId) -> Option<&DealerRecord> {
        self.dealers.get(&dealer)
    }

    /// All enrolled dealer ids, ascending (deterministic iteration).
    #[must_use]
    pub fn dealers(&self) -> Vec<SubaccountId> {
        self.dealers.keys().copied().collect()
    }

    /// Enroll a dealer at a starting tier. Re-enrolling a dropped dealer
    /// is allowed (an explicit governance act) and resets the record.
    pub fn enroll(
        &mut self,
        dealer: SubaccountId,
        tier: DealerTier,
        now: u64,
    ) -> Result<(), &'static str> {
        if let Some(existing) = self.dealers.get(&dealer) {
            if existing.dropped {
                // Explicit re-enrollment is permitted but must be visible:
                // it resets the ladder from zero.
                self.dealers.insert(
                    dealer,
                    DealerRecord {
                        dealer,
                        tier,
                        enrolled_ts: now,
                        strikes: Vec::new(),
                        executions: 0,
                        lapses: 0,
                        executed_lots: 0,
                        suspension_count: 0,
                        suspended_until: 0,
                        dropped: false,
                    },
                );
                return Ok(());
            }
            if existing.suspended_until > now {
                return Err("dealer is suspended");
            }
            return Err("dealer already enrolled");
        }
        self.dealers.insert(
            dealer,
            DealerRecord {
                dealer,
                tier,
                enrolled_ts: now,
                strikes: Vec::new(),
                executions: 0,
                lapses: 0,
                executed_lots: 0,
                suspension_count: 0,
                suspended_until: 0,
                dropped: false,
            },
        );
        Ok(())
    }

    /// Whether the dealer may send quotes right now. Returns the tier the
    /// engine should attach for routing, or the gate that blocks them.
    pub fn can_quote(&self, dealer: SubaccountId, now: u64) -> Result<DealerTier, ProgramGate> {
        match self.dealers.get(&dealer) {
            None => Err(ProgramGate::NotEnrolled),
            Some(r) if r.dropped => Err(ProgramGate::Dropped),
            Some(r) if r.suspended_until > now => {
                Err(ProgramGate::SuspendedUntil(r.suspended_until))
            }
            Some(r) => Ok(r.tier),
        }
    }

    /// Routing view: enrolled, not-suspended dealers ranked by tier
    /// (Gold first) then subaccount id — the deterministic order in which
    /// a directed RFQ surfaces.
    #[must_use]
    pub fn eligible_dealers(&self, now: u64) -> Vec<(SubaccountId, DealerTier)> {
        let mut out: Vec<(SubaccountId, DealerTier)> = self
            .dealers
            .values()
            .filter(|r| !r.dropped && r.suspended_until <= now)
            .map(|r| (r.dealer, r.tier))
            .collect();
        out.sort_by(|a, b| b.1.rank().cmp(&a.1.rank()).then(a.0.cmp(&b.0)));
        out
    }

    /// Record a quote pull (cancel of a live quote on a still-open RFQ —
    /// the "pulled after seeing the request" signal). Returns what the
    /// caller must journal. Idempotent per call site: the engine calls
    /// this exactly once per journaled pull.
    pub fn record_pull(&mut self, dealer: SubaccountId, now: u64) -> PullOutcome {
        let Some(r) = self.dealers.get(&dealer) else {
            return PullOutcome::Strike;
        };
        // Evaluate first (pure), then mutate — plan stages use
        // [`preview_pull`](Self::preview_pull) for the same decision.
        let outcome = eval_pull(r, &self.config, now);
        let Some(r) = self.dealers.get_mut(&dealer) else {
            return PullOutcome::Strike;
        };
        apply_pull(r, now, outcome);
        outcome
    }

    /// **Pure** pull evaluation (plan stage): what `record_pull` would do
    /// to this dealer at `now`, without mutating. The engine's planner
    /// calls this to decide which events to journal (`DealerStruck`,
    /// `DealerSuspended`, `DealerDropped`); apply replays through
    /// `record_pull`, which deterministically reproduces the outcome.
    #[must_use]
    pub fn preview_pull(&self, dealer: SubaccountId, now: u64) -> PullOutcome {
        match self.dealers.get(&dealer) {
            None => PullOutcome::Strike,
            Some(r) => eval_pull(r, &self.config, now),
        }
    }

    /// Record an executed quote (credit). Returns what the caller must
    /// journal (possible promotion once thresholds + score floor are met).
    pub fn record_execution(&mut self, dealer: SubaccountId, lots: u128) -> ExecuteOutcome {
        let Some(r) = self.dealers.get_mut(&dealer) else {
            return ExecuteOutcome::Recorded;
        };
        if r.dropped {
            return ExecuteOutcome::Recorded;
        }
        r.executions = r.executions.saturating_add(1);
        r.executed_lots = r.executed_lots.saturating_add(lots);

        // Promotion is one-way (Basic -> Silver -> Gold) and requires the
        // reliability floor at the moment of credit.
        let score = r.score_permille(&self.config);
        let tier = if r.executed_lots >= self.config.gold_executed_lots
            && score >= self.config.tier_score_floor_permille
        {
            DealerTier::Gold
        } else if r.executed_lots >= self.config.silver_executed_lots
            && score >= self.config.tier_score_floor_permille
        {
            DealerTier::Silver
        } else {
            r.tier
        };
        if tier.rank() > r.tier.rank() {
            r.tier = tier;
            return ExecuteOutcome::Promoted(tier);
        }
        ExecuteOutcome::Recorded
    }

    /// Record a lapsed quote (expired unexecuted). Never suspends —
    /// expiry is a market condition — but erodes the score.
    pub fn record_lapse(&mut self, dealer: SubaccountId) {
        if let Some(r) = self.dealers.get_mut(&dealer) {
            if !r.dropped {
                r.lapses = r.lapses.saturating_add(1);
            }
        }
    }

    /// Explicit demotion / tier set (governance event). Fails on an
    /// unknown dealer.
    pub fn set_tier(&mut self, dealer: SubaccountId, tier: DealerTier) -> Result<(), &'static str> {
        match self.dealers.get_mut(&dealer) {
            None => Err("dealer not enrolled"),
            Some(r) => {
                r.tier = tier;
                Ok(())
            }
        }
    }

    /// The tier this dealer would hold on the promotion ladder right now
    /// (volume + score floor), independent of any explicit set.
    #[must_use]
    pub fn earned_tier(&self, dealer: SubaccountId) -> Option<DealerTier> {
        let r = self.dealers.get(&dealer)?;
        let score = r.score_permille(&self.config);
        if r.executed_lots >= self.config.gold_executed_lots
            && score >= self.config.tier_score_floor_permille
        {
            Some(DealerTier::Gold)
        } else if r.executed_lots >= self.config.silver_executed_lots
            && score >= self.config.tier_score_floor_permille
        {
            Some(DealerTier::Silver)
        } else {
            Some(DealerTier::Basic)
        }
    }
}

/// Pure pull-ladder evaluation: what a pull at `now` would do to `r`.
fn eval_pull(r: &DealerRecord, config: &ProgramConfig, now: u64) -> PullOutcome {
    if r.dropped {
        return PullOutcome::Dropped;
    }
    // Strikes inside the rolling window *including the one about to be
    // recorded*.
    let window_start = now.saturating_sub(config.strike_window_ms);
    let in_window = r.strikes.iter().filter(|&&ts| ts > window_start).count() as u32 + 1;
    if in_window < config.strike_threshold {
        return PullOutcome::Strike;
    }
    // Trip: escalate the suspension ladder.
    let next_count = r.suspension_count.saturating_add(1);
    if next_count >= config.drop_after_suspensions {
        return PullOutcome::Dropped;
    }
    let length = config.suspension_ms.saturating_mul(u64::from(
        config.suspension_multiplier.saturating_pow(next_count - 1),
    ));
    PullOutcome::Suspended(now.saturating_add(length))
}

/// Mutation side of the pull ladder — mirrors `eval_pull`'s decision.
fn apply_pull(r: &mut DealerRecord, now: u64, outcome: PullOutcome) {
    match outcome {
        PullOutcome::Strike => {
            r.strikes.push(now);
        }
        PullOutcome::Suspended(until) => {
            r.strikes.push(now);
            r.suspension_count = r.suspension_count.saturating_add(1);
            r.suspended_until = until;
            // The window resets: the dealer starts a fresh count after
            // the suspension lapses.
            r.strikes.clear();
        }
        PullOutcome::Dropped => {
            r.strikes.push(now);
            r.suspension_count = r.suspension_count.saturating_add(1);
            r.dropped = true;
            r.suspended_until = u64::MAX;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D1: SubaccountId = 11;
    const D2: SubaccountId = 22;

    fn program() -> DealerProgram {
        DealerProgram::new(ProgramConfig::default()).unwrap()
    }

    fn enroll_all(p: &mut DealerProgram) {
        p.enroll(D1, DealerTier::Basic, 1_000).unwrap();
        p.enroll(D2, DealerTier::Basic, 1_000).unwrap();
    }

    // 1. enroll / can_quote / eligible routing order ---------------------------

    #[test]
    fn enroll_and_gate() {
        let mut p = program();
        assert_eq!(p.can_quote(D1, 2_000), Err(ProgramGate::NotEnrolled));
        p.enroll(D1, DealerTier::Basic, 1_000).unwrap();
        assert_eq!(p.can_quote(D1, 2_000), Ok(DealerTier::Basic));
        // Double enrollment (not dropped) is rejected.
        assert_eq!(
            p.enroll(D1, DealerTier::Silver, 2_000).unwrap_err(),
            "dealer already enrolled"
        );
        // Eligible list contains exactly the enrolled, Basic tier.
        assert_eq!(p.eligible_dealers(2_000), vec![(D1, DealerTier::Basic)]);
        // Duplicate idempotency: pull on unknown dealer is a no-op Strike.
        assert_eq!(p.record_pull(999, 1_500), PullOutcome::Strike);
    }

    // 2. the suspension ladder -------------------------------------------------

    #[test]
    fn suspension_ladder() {
        let mut p = program();
        enroll_all(&mut p);
        let t0 = 1_000_000_u64;

        // 3 pulls inside the window trip the first suspension.
        assert_eq!(p.record_pull(D1, t0), PullOutcome::Strike);
        assert_eq!(p.record_pull(D1, t0 + 1), PullOutcome::Strike);
        assert_eq!(
            p.record_pull(D1, t0 + 2),
            PullOutcome::Suspended(t0 + 2 + 3_600_000)
        );
        assert_eq!(
            p.can_quote(D1, t0 + 3),
            Err(ProgramGate::SuspendedUntil(t0 + 2 + 3_600_000))
        );
        // D2 unaffected.
        assert_eq!(p.can_quote(D2, t0 + 3), Ok(DealerTier::Basic));

        // Strikes were reset by the suspension: the ladder restarts at
        // zero, so the SECOND trip needs three fresh pulls after the
        // suspension lapses.
        let after = t0 + 2 + 3_600_000 + 1;
        assert_eq!(p.can_quote(D1, after), Ok(DealerTier::Basic));
        assert_eq!(p.record_pull(D1, after), PullOutcome::Strike);
        assert_eq!(p.record_pull(D1, after + 1), PullOutcome::Strike);
        assert_eq!(
            p.record_pull(D1, after + 2),
            PullOutcome::Suspended(after + 2 + 7_200_000)
        );

        // Third suspension escalates to the permanent drop.
        let after2 = after + 2 + 7_200_000 + 1;
        assert_eq!(p.record_pull(D1, after2), PullOutcome::Strike);
        assert_eq!(p.record_pull(D1, after2 + 1), PullOutcome::Strike);
        assert_eq!(p.record_pull(D1, after2 + 2), PullOutcome::Dropped);
        assert_eq!(p.can_quote(D1, after2 + 3), Err(ProgramGate::Dropped));
        // Dropped dealers vanish from routing.
        assert_eq!(
            p.eligible_dealers(after2 + 3),
            vec![(D2, DealerTier::Basic)]
        );
        // Further pulls on a dropped dealer stay dropped.
        assert_eq!(p.record_pull(D1, after2 + 4), PullOutcome::Dropped);
    }

    // 2b. pulls DURING a suspension still count toward the next trip -----

    #[test]
    fn pulls_during_suspension_count() {
        let mut p = program();
        p.enroll(D1, DealerTier::Basic, 0).unwrap();
        let t0 = 1_000_000_u64;

        // Trip the first suspension.
        for i in 0..3u64 {
            p.record_pull(D1, t0 + i);
        }
        let until = t0 + 2 + 3_600_000;

        // Two pulls while suspended (quotes sent before the suspension can
        // still be pulled) plus one fresh pull after it lapses re-trips.
        assert_eq!(p.record_pull(D1, t0 + 10), PullOutcome::Strike);
        assert_eq!(p.record_pull(D1, until - 1), PullOutcome::Strike);
        let after = until + 1;
        assert_eq!(
            p.record_pull(D1, after),
            PullOutcome::Suspended(after + 7_200_000)
        );
    }

    // 3. strikes outside the window do not accumulate --------------------------

    #[test]
    fn strikes_expire_with_window() {
        let mut p = program();
        p.enroll(D1, DealerTier::Basic, 0).unwrap();
        let w = ProgramConfig::default().strike_window_ms;

        // Two pulls, then sleep far past the window, then one more: the
        // first two are outside the window, so the third is strike 1, not 3.
        assert_eq!(p.record_pull(D1, 1_000), PullOutcome::Strike);
        assert_eq!(p.record_pull(D1, 1_000 + 1), PullOutcome::Strike);
        let late = 1_000 + w + 10;
        assert_eq!(p.record_pull(D1, late), PullOutcome::Strike);
        // Boundary: a strike exactly `window` old is OUTSIDE (`> window_start`).
        // Two fresh ones now trip (this one at `late` plus two more).
        assert_eq!(p.record_pull(D1, late + 1), PullOutcome::Strike);
        assert_eq!(
            p.record_pull(D1, late + 2),
            PullOutcome::Suspended(late + 2 + 3_600_000)
        );
    }

    // 4. executions credit, lapses erode, promotion ladder ---------------------

    #[test]
    fn score_and_promotion() {
        let mut p = program();
        enroll_all(&mut p);
        let c = p.config().clone();

        // No history: perfect score.
        assert_eq!(p.record(D1).unwrap().score_permille(&c), 1_000);

        // 2 executions + 20 lapses at w=100 permille: the integer lapse
        // weight is 20*100/1000 = 2, so score = 2*1000/(2+0+2) = 500.
        p.record_execution(D1, 100);
        p.record_execution(D1, 100);
        for _ in 0..20 {
            p.record_lapse(D1);
        }
        assert_eq!(p.record(D1).unwrap().score_permille(&c), 500);
        // Volume (200 lots) is far below Silver and the score is below
        // the 700 floor: earned tier stays Basic.
        assert_eq!(p.earned_tier(D1), Some(DealerTier::Basic));

        // Clean executions push volume past Silver and the score back
        // over the floor: 18 executions total, score 18*1000/(18+2) = 900.
        for _ in 0..16 {
            p.record_execution(D1, 400);
        }
        assert_eq!(p.record(D1).unwrap().score_permille(&c), 900);
        assert_eq!(p.record(D1).unwrap().held_tier(), DealerTier::Silver);
        assert_eq!(p.earned_tier(D1), Some(DealerTier::Silver));

        // The volume path to Gold with a rehabilitated record: 28 total
        // executions, 66_600 lots, score 28*1000/(28+2) = 933.
        for _ in 0..10 {
            p.record_execution(D1, 6_000);
        }
        assert_eq!(p.record(D1).unwrap().score_permille(&c), 933);
        assert_eq!(p.earned_tier(D1), Some(DealerTier::Gold));
        assert_eq!(p.record(D1).unwrap().held_tier(), DealerTier::Gold);

        // Volume with a clean-at-the-moment record promotes D2 to Silver
        // immediately; later lapses erode the EARNED tier but never the
        // HELD tier (demotion is an explicit governance act).
        p.record_execution(D2, 6_000);
        assert_eq!(p.record(D2).unwrap().held_tier(), DealerTier::Silver);
        for _ in 0..60 {
            p.record_lapse(D2);
        }
        // score = 1000/(1 + 0 + 60*100/1000) = 1000/7 = 142 < 700.
        assert_eq!(p.earned_tier(D2), Some(DealerTier::Basic));
        assert_eq!(p.record(D2).unwrap().held_tier(), DealerTier::Silver);
    }

    // 5. re-enrollment of a dropped dealer resets the ladder -------------------

    #[test]
    fn reenroll_after_drop_resets() {
        let mut p = program();
        p.enroll(D1, DealerTier::Basic, 0).unwrap();
        for i in 0..3u64 {
            p.record_pull(D1, 1_000 + i);
        }
        for i in 0..3u64 {
            p.record_pull(D1, 10_000_000 + i);
        }
        for i in 0..3u64 {
            p.record_pull(D1, 20_000_000 + i);
        }
        // Wait: threshold 3 per window -> these 9 pulls are 3 windows ->
        // suspension 1, suspension 2, drop. Verify:
        assert_eq!(p.can_quote(D1, 30_000_000), Err(ProgramGate::Dropped));

        // Explicit re-enrollment resets the record.
        p.enroll(D1, DealerTier::Basic, 30_000_000).unwrap();
        assert_eq!(p.can_quote(D1, 30_000_001), Ok(DealerTier::Basic));
        assert_eq!(p.record(D1).unwrap().executions, 0);
        assert_eq!(p.record(D1).unwrap().strikes.len(), 0);
    }

    // 6. config validation ------------------------------------------------------

    #[test]
    fn config_validation() {
        let base = ProgramConfig::default();
        assert!(base.validate().is_ok());
        assert!(ProgramConfig {
            strike_window_ms: 0,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(ProgramConfig {
            suspension_multiplier: 0,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(ProgramConfig {
            silver_executed_lots: base.gold_executed_lots,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(ProgramConfig {
            lapse_weight_permille: 1_001,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(ProgramConfig {
            tier_score_floor_permille: 1_001,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(ProgramConfig {
            strike_threshold: 0,
            ..base.clone()
        }
        .validate()
        .is_err());
        assert!(ProgramConfig {
            drop_after_suspensions: 0,
            ..base
        }
        .validate()
        .is_err());
    }

    // 7. routing priority: Gold before Silver before Basic, id tiebreak -------

    #[test]
    fn routing_priority_order() {
        let mut p = program();
        p.enroll(3, DealerTier::Basic, 0).unwrap();
        p.enroll(1, DealerTier::Gold, 0).unwrap();
        p.enroll(2, DealerTier::Silver, 0).unwrap();
        p.enroll(4, DealerTier::Silver, 0).unwrap();
        let order: Vec<SubaccountId> = p.eligible_dealers(1).into_iter().map(|(d, _)| d).collect();
        assert_eq!(order, vec![1, 2, 4, 3]);
        // Gold dealer holds the highest rank.
        assert_eq!(p.record(1).unwrap().held_tier(), DealerTier::Gold);
    }

    // 8. set_tier governance + determinism --------------------------------------

    #[test]
    fn set_tier_and_determinism() {
        let mut a = program();
        let mut b = program();
        for p in [&mut a, &mut b] {
            p.enroll(D1, DealerTier::Basic, 5).unwrap();
            p.set_tier(D1, DealerTier::Gold).unwrap();
            p.record_execution(D1, 42);
            p.record_lapse(D1);
        }
        assert_eq!(a.record(D1), b.record(D1));
        assert_eq!(
            a.set_tier(999, DealerTier::Basic).unwrap_err(),
            "dealer not enrolled"
        );
    }

    // 9. preview_pull agrees with record_pull at every rung ---------------------

    #[test]
    fn preview_matches_record() {
        let mut p = program();
        p.enroll(D1, DealerTier::Basic, 0).unwrap();
        let mut t = 1_000_u64;

        // Drive the whole ladder, checking the pure preview first.
        for expected in [
            PullOutcome::Strike,
            PullOutcome::Strike,
            PullOutcome::Suspended(t + 2 + 3_600_000),
        ] {
            assert_eq!(p.preview_pull(D1, t), expected);
            assert_eq!(p.record_pull(D1, t), expected);
            t += 1;
        }
        // Serve the suspension, then run to the drop.
        t += 3_600_000;
        for expected in [
            PullOutcome::Strike,
            PullOutcome::Strike,
            PullOutcome::Suspended(t + 2 + 7_200_000),
        ] {
            assert_eq!(p.preview_pull(D1, t), expected);
            assert_eq!(p.record_pull(D1, t), expected);
            t += 1;
        }
        t += 7_200_000;
        for expected in [
            PullOutcome::Strike,
            PullOutcome::Strike,
            PullOutcome::Dropped,
        ] {
            assert_eq!(p.preview_pull(D1, t), expected);
            assert_eq!(p.record_pull(D1, t), expected);
            t += 1;
        }
        assert_eq!(p.preview_pull(D1, t), PullOutcome::Dropped);
    }
}
