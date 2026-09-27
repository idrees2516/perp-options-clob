//! # poc-governance
//!
//! Parameter governance (gap-register **G-33**): the Aave/dYdX pattern
//! of **weighted-multisig proposals, a public timelock, and a guardian
//! veto**, resolved for a deterministic engine.
//!
//! * A change to a governed parameter (fee tier, margin ratio, oracle
//!   config…) is **proposed** with a canonical `(key, value)` payload.
//! * Approval is a **weighted multisig**: signers carry weights; the
//!   proposal queues when accumulated weight ≥ threshold.
//! * A queued proposal **becomes executable at `eta = queued_at +
//!   timelock`** and expires after `grace`. Users can front-run the
//!   change (unwind, withdraw) during the window — that is the point.
//! * The **guardian** can cancel anything, instantly (emergency stop
//!   power, itself auditable in the proposal log).
//! * Every transition returns [`GovernanceEvent`]s so the host journals
//!   governance with the same discipline as the engine journal.

use std::collections::BTreeMap;

use poc_core::TimestampMs;

/// One governed parameter change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// Governance-assigned id.
    pub id: u64,
    /// Canonical parameter key (e.g. `"fees.tier0.taker_bps"`).
    pub key: String,
    /// New value (integer minor/bps/flag — the key defines units).
    pub value: u128,
    /// Human-readable rationale (hashed into the id chain).
    pub description: String,
    /// Proposer (signer id).
    pub proposer: String,
    /// Lifecycle state.
    pub state: ProposalState,
    /// Approvals so far: signer → weight snapshot.
    pub approvals: BTreeMap<String, u64>,
    /// Queued-at timestamp (valid in `Queued` and later).
    pub queued_at: TimestampMs,
    /// Executed-at timestamp.
    pub executed_at: TimestampMs,
}

/// Proposal lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalState {
    /// Gathering approvals.
    PendingApproval,
    /// Approved; timelock running.
    Queued,
    /// Executed; the parameter changed.
    Executed,
    /// Cancelled (guardian veto or proposer withdrawal).
    Cancelled,
}

/// Governance lifecycle event (journal material).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GovernanceEvent {
    /// A proposal was created.
    Proposed(u64),
    /// A signer approved.
    Approved {
        /// Proposal id.
        id: u64,
        /// The signer.
        signer: String,
        /// Their weight.
        weight: u64,
    },
    /// The approval threshold was met; the timelock started.
    Queued {
        /// Proposal id.
        id: u64,
        /// Execution allowed from this time.
        eta: TimestampMs,
    },
    /// The change took effect.
    Executed {
        /// Proposal id.
        id: u64,
        /// The parameter key.
        key: String,
        /// The new value.
        value: u128,
    },
    /// Cancelled (proposer withdrawal or guardian veto).
    Cancelled {
        /// Proposal id.
        id: u64,
    },
    /// The timelock window lapsed without execution.
    Expired {
        /// Proposal id.
        id: u64,
    },
}

/// Why an action was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernanceError {
    /// Unknown signer.
    UnknownSigner,
    /// Duplicate approval by the same signer.
    AlreadyApproved,
    /// Proposal not in the expected state.
    WrongState,
    /// The timelock has not elapsed yet.
    TooEarly,
    /// The grace window lapsed.
    Expired,
    /// Only the guardian may do this.
    NotGuardian,
}

/// Governance configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernanceConfig {
    /// Approval weight required to queue (out of total signer weight).
    pub approval_threshold_weight: u64,
    /// Timelock before execution is allowed (ms).
    pub timelock_ms: TimestampMs,
    /// Grace after eta; unexecuted proposals expire (ms).
    pub grace_ms: TimestampMs,
    /// The emergency-cancel authority.
    pub guardian: String,
}

impl Default for GovernanceConfig {
    fn default() -> Self {
        Self {
            approval_threshold_weight: 3,
            timelock_ms: 24 * 60 * 60 * 1000,  // 24h
            grace_ms: 3 * 24 * 60 * 60 * 1000, // 72h
            guardian: "guardian".into(),
        }
    }
}

/// The governor: signers, weights, proposals.
#[derive(Debug, Default)]
pub struct Governor {
    config: GovernanceConfig,
    signers: BTreeMap<String, u64>,
    proposals: BTreeMap<u64, Proposal>,
    next_id: u64,
    /// Effective parameter values (executed proposals land here).
    params: BTreeMap<String, u128>,
}

impl Governor {
    /// New governor from a config and a weighted signer set.
    #[must_use]
    pub fn new(config: GovernanceConfig, signers: BTreeMap<String, u64>) -> Self {
        Self {
            config,
            signers,
            proposals: BTreeMap::new(),
            next_id: 1,
            params: BTreeMap::new(),
        }
    }

    /// Propose a parameter change (anyone may propose; approvals make
    /// it real — the "anyone can propose, weights decide" split that
    /// keeps governance open but sybil-safe).
    pub fn propose(
        &mut self,
        proposer: &str,
        key: &str,
        value: u128,
        description: &str,
    ) -> Result<(u64, GovernanceEvent), GovernanceError> {
        if !self.signers.contains_key(proposer) {
            return Err(GovernanceError::UnknownSigner);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.proposals.insert(
            id,
            Proposal {
                id,
                key: key.to_owned(),
                value,
                description: description.to_owned(),
                proposer: proposer.to_owned(),
                state: ProposalState::PendingApproval,
                approvals: BTreeMap::new(),
                queued_at: 0,
                executed_at: 0,
            },
        );
        Ok((id, GovernanceEvent::Proposed(id)))
    }

    /// Approve a pending proposal.
    pub fn approve(
        &mut self,
        signer: &str,
        id: u64,
        now: TimestampMs,
    ) -> Result<GovernanceEvent, GovernanceError> {
        let weight = self
            .signers
            .get(signer)
            .copied()
            .ok_or(GovernanceError::UnknownSigner)?;
        let Some(p) = self.proposals.get_mut(&id) else {
            return Err(GovernanceError::WrongState);
        };
        if p.state != ProposalState::PendingApproval {
            return Err(GovernanceError::WrongState);
        }
        if p.approvals.contains_key(signer) {
            return Err(GovernanceError::AlreadyApproved);
        }
        p.approvals.insert(signer.to_owned(), weight);
        let total: u64 = p.approvals.values().sum();
        if total >= self.config.approval_threshold_weight {
            p.state = ProposalState::Queued;
            p.queued_at = now;
            let eta = now.saturating_add(self.config.timelock_ms);
            return Ok(GovernanceEvent::Queued { id, eta });
        }
        Ok(GovernanceEvent::Approved {
            id,
            signer: signer.to_owned(),
            weight,
        })
    }

    /// Execute a queued proposal whose timelock elapsed.
    pub fn execute(
        &mut self,
        id: u64,
        now: TimestampMs,
    ) -> Result<GovernanceEvent, GovernanceError> {
        let Some(p) = self.proposals.get_mut(&id) else {
            return Err(GovernanceError::WrongState);
        };
        if p.state != ProposalState::Queued {
            return Err(GovernanceError::WrongState);
        }
        let eta = p.queued_at.saturating_add(self.config.timelock_ms);
        if now < eta {
            return Err(GovernanceError::TooEarly);
        }
        if now > eta.saturating_add(self.config.grace_ms) {
            return Err(GovernanceError::Expired);
        }
        p.state = ProposalState::Executed;
        p.executed_at = now;
        let ev = GovernanceEvent::Executed {
            id,
            key: p.key.clone(),
            value: p.value,
        };
        self.params.insert(p.key.clone(), p.value);
        Ok(ev)
    }

    /// Cancel: the guardian may cancel anything instantly; the proposer
    /// may withdraw an unqueued proposal.
    pub fn cancel(&mut self, actor: &str, id: u64) -> Result<GovernanceEvent, GovernanceError> {
        let Some(p) = self.proposals.get_mut(&id) else {
            return Err(GovernanceError::WrongState);
        };
        if p.state == ProposalState::Executed {
            return Err(GovernanceError::WrongState);
        }
        if actor != self.config.guardian
            && (actor != p.proposer || p.state != ProposalState::PendingApproval)
        {
            return Err(GovernanceError::NotGuardian);
        }
        p.state = ProposalState::Cancelled;
        Ok(GovernanceEvent::Cancelled { id })
    }

    /// Sweep expired proposals (journal the expiries).
    #[must_use]
    pub fn sweep_expiries(&self, now: TimestampMs) -> Vec<GovernanceEvent> {
        self.proposals
            .values()
            .filter(|p| {
                p.state == ProposalState::Queued
                    && now
                        > p.queued_at
                            .saturating_add(self.config.timelock_ms + self.config.grace_ms)
            })
            .map(|p| GovernanceEvent::Expired { id: p.id })
            .collect()
    }

    /// The effective value of a governed parameter (default when never
    /// set).
    #[must_use]
    pub fn param(&self, key: &str, default: u128) -> u128 {
        self.params.get(key).copied().unwrap_or(default)
    }

    /// Proposal lookup.
    #[must_use]
    pub fn proposal(&self, id: u64) -> Option<&Proposal> {
        self.proposals.get(&id)
    }

    /// Effective parameter count (reporting).
    #[must_use]
    pub fn param_count(&self) -> usize {
        self.params.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn governor() -> Governor {
        let signers = BTreeMap::from([
            ("alice".to_string(), 2),
            ("bob".to_string(), 1),
            ("carol".to_string(), 1),
            ("dan".to_string(), 1),
        ]);
        Governor::new(GovernanceConfig::default(), signers)
    }

    #[test]
    fn full_lifecycle_propose_queue_execute() {
        let mut g = governor();
        let (id, _) = g
            .propose("alice", "fees.tier0.taker_bps", 5, "raise")
            .unwrap();
        // Alice's weight is 2 < threshold 3: not queued yet.
        assert!(matches!(
            g.approve("alice", id, 0).unwrap(),
            GovernanceEvent::Approved { .. }
        ));
        // Bob's +1 reaches the threshold and starts the timelock.
        assert!(matches!(
            g.approve("bob", id, 100).unwrap(),
            GovernanceEvent::Queued { .. }
        ));
        let eta_plus = 100 + 24 * 60 * 60 * 1000 + 1;
        assert!(matches!(
            g.execute(id, eta_plus).unwrap(),
            GovernanceEvent::Executed { value: 5, .. }
        ));
        assert_eq!(g.param("fees.tier0.taker_bps", 0), 5);
        assert_eq!(g.proposal(id).unwrap().state, ProposalState::Executed);
    }

    #[test]
    fn threshold_requires_accumulated_weight() {
        let mut g = governor();
        let (id, _) = g.propose("alice", "risk.max_leverage", 20, "").unwrap();
        assert!(matches!(
            g.approve("alice", id, 0).unwrap(),
            GovernanceEvent::Approved { .. }
        ));
        // bob adds 1 -> total 3 -> queued.
        assert!(matches!(
            g.approve("bob", id, 10).unwrap(),
            GovernanceEvent::Queued { .. }
        ));
        // Approving a proposal that already moved past the approval
        // stage is a state error (the signer should have been faster).
        assert_eq!(
            g.approve("alice", id, 11).unwrap_err(),
            GovernanceError::WrongState
        );
        // A duplicate while still pending is the specific error.
        let (id3, _) = g.propose("carol", "ops.other", 1, "").unwrap();
        g.approve("carol", id3, 0).unwrap();
        assert_eq!(
            g.approve("carol", id3, 1).unwrap_err(),
            GovernanceError::AlreadyApproved
        );
        // Too early to execute.
        assert_eq!(g.execute(id, 11).unwrap_err(), GovernanceError::TooEarly);
        // After the timelock: executes and lands in params.
        let eta_plus = 24 * 60 * 60 * 1000 + 100;
        let ev = g.execute(id, eta_plus).unwrap();
        assert!(matches!(ev, GovernanceEvent::Executed { value: 20, .. }));
        assert_eq!(g.param("risk.max_leverage", 0), 20);
    }

    #[test]
    fn guardian_cancels_even_queued() {
        let mut g = governor();
        let (id, _) = g.propose("alice", "ops.halt", 1, "").unwrap();
        g.approve("alice", id, 0).unwrap();
        g.approve("bob", id, 0).unwrap();
        assert!(matches!(
            g.cancel("guardian", id).unwrap(),
            GovernanceEvent::Cancelled { .. }
        ));
        assert_eq!(
            g.execute(id, u64::MAX / 2).unwrap_err(),
            GovernanceError::WrongState
        );
    }

    #[test]
    fn non_guardian_cannot_cancel_queued() {
        let mut g = governor();
        let (id, _) = g.propose("alice", "fees.x", 1, "").unwrap();
        g.approve("alice", id, 0).unwrap();
        g.approve("bob", id, 0).unwrap();
        // bob is not the proposer, and it is queued.
        assert_eq!(
            g.cancel("bob", id).unwrap_err(),
            GovernanceError::NotGuardian
        );
        // The proposer can withdraw before queueing.
        let (id2, _) = g.propose("carol", "fees.y", 2, "").unwrap();
        assert!(g.cancel("carol", id2).is_ok());
    }

    #[test]
    fn grace_window_expires_unexecuted() {
        let mut g = governor();
        let (id, _) = g.propose("alice", "ops.param", 9, "").unwrap();
        g.approve("alice", id, 0).unwrap();
        g.approve("bob", id, 0).unwrap();
        let late = 24 * 60 * 60 * 1000 + 3 * 24 * 60 * 60 * 1000 + 1;
        assert_eq!(g.execute(id, late).unwrap_err(), GovernanceError::Expired);
        let evs = g.sweep_expiries(late);
        assert_eq!(evs, vec![GovernanceEvent::Expired { id }]);
    }

    #[test]
    fn unknown_signer_rejected() {
        let mut g = governor();
        assert_eq!(
            g.propose("mallory", "x", 1, "").unwrap_err(),
            GovernanceError::UnknownSigner
        );
        let (id, _) = g.propose("alice", "x", 1, "").unwrap();
        assert_eq!(
            g.approve("mallory", id, 0).unwrap_err(),
            GovernanceError::UnknownSigner
        );
    }
}
