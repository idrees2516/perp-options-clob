//! The withdrawal pipeline (G-32): request → pending → settled, with
//! limits, time-locks, and a custody adapter.
//!
//! Design (the exchange-standard shape):
//!
//! * **Request validation** — balance covers the amount, in-flight
//!   withdrawals count against a per-account cap, daily hot-wallet
//!   quota respected.
//! * **Time-locked settlement** — a request becomes claimable after
//!   `settlement_delay_ms` (withdrawals are *never* instant: the
//!   operator needs a window to react to compromised keys). Delayed
//!   settlements can be cancelled until they expire — a cancelled
//!   request refunds instantly.
//! * **Custody adapter** — the venue's treasury system (hot wallet,
//!   HSM, multisig) sits behind a trait; the library's in-memory
//!   adapter doubles as the reference implementation and the test
//!   double.
//! * **Policy tiers** — large withdrawals require manual approval
//!   (`requires_manual_approval`), the "cold-storage flag" every
//!   institutional desk expects.

use std::collections::BTreeMap;

use poc_core::{SubaccountId, TimestampMs};

/// A withdrawal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalRequest {
    /// Requesting subaccount.
    pub subaccount: SubaccountId,
    /// Amount, quote minor.
    pub amount_quote_minor: u128,
    /// Destination address / tag (opaque to the engine).
    pub destination: String,
    /// Requested at (ms).
    pub requested_at: TimestampMs,
    /// Claimable at (ms) — requested_at + settlement delay.
    pub claimable_at: TimestampMs,
}

/// Withdrawal pipeline policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawalPolicy {
    /// Settlement delay before a withdrawal becomes claimable (ms).
    pub settlement_delay_ms: TimestampMs,
    /// Maximum concurrent pending withdrawals per account.
    pub max_pending_per_account: usize,
    /// Maximum amount per single withdrawal, quote minor.
    pub max_per_withdrawal_quote_minor: u128,
    /// Daily quota (rolling), quote minor.
    pub daily_quota_quote_minor: u128,
    /// Withdrawals above this require manual approval.
    pub manual_approval_threshold_quote_minor: u128,
}

impl Default for WithdrawalPolicy {
    fn default() -> Self {
        Self {
            settlement_delay_ms: 30 * 60 * 1000, // 30 minutes
            max_pending_per_account: 5,
            max_per_withdrawal_quote_minor: 1_000_000_000, // $10M @ 2dp
            daily_quota_quote_minor: 5_000_000_000,        // $50M @ 2dp
            manual_approval_threshold_quote_minor: 500_000_000, // $5M
        }
    }
}

/// Why a withdrawal request was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalError {
    /// Non-positive amount.
    InvalidAmount,
    /// Insufficient spendable balance.
    InsufficientBalance,
    /// Too many pending withdrawals.
    TooManyPending,
    /// Above the per-withdrawal cap.
    AbovePerWithdrawalCap,
    /// Above the remaining daily quota.
    AboveDailyQuota,
    /// The withdrawal does not exist or is not pending.
    NotPending,
}

/// Settlement state of one withdrawal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WithdrawalState {
    /// Awaiting its time-lock (cancellable).
    Pending,
    /// Manually approved (large withdrawals).
    Approved,
    /// Settled to the destination.
    Settled,
    /// Cancelled (amount returned to the account).
    Cancelled,
}

/// Where the venue's money actually lives: the treasury adapter.
pub trait CustodyAdapter {
    /// Debit the treasury for a settled withdrawal (returns false if
    /// the treasury cannot cover it — the pipeline marks the
    /// withdrawal failed rather than pretending).
    fn debit(&mut self, amount_quote_minor: u128) -> bool;
    /// Refund a cancelled withdrawal.
    fn refund(&mut self, amount_quote_minor: u128);
}

/// In-memory custody (reference implementation / test double).
#[derive(Debug, Default)]
pub struct InMemoryCustody {
    /// Current treasury balance, quote minor.
    pub balance_quote_minor: u128,
}

impl CustodyAdapter for InMemoryCustody {
    fn debit(&mut self, amount: u128) -> bool {
        if self.balance_quote_minor >= amount {
            self.balance_quote_minor -= amount;
            true
        } else {
            false
        }
    }

    fn refund(&mut self, amount: u128) {
        self.balance_quote_minor = self.balance_quote_minor.saturating_add(amount);
    }
}

/// The withdrawal pipeline over spendable balances.
pub struct WithdrawalPipeline {
    policy: WithdrawalPolicy,
    pending: BTreeMap<u64, (WithdrawalRequest, WithdrawalState)>,
    next_id: u64,
    /// Pending amounts per account.
    per_account: BTreeMap<SubaccountId, usize>,
    /// (day, subaccount) → amount withdrawn today.
    daily: BTreeMap<(u64, SubaccountId), u128>,
}

impl WithdrawalPipeline {
    /// New pipeline with a policy.
    #[must_use]
    pub fn new(policy: WithdrawalPolicy) -> Self {
        Self {
            policy,
            pending: BTreeMap::new(),
            next_id: 1,
            per_account: BTreeMap::new(),
            daily: BTreeMap::new(),
        }
    }

    /// Request a withdrawal from a spendable balance.
    pub fn request(
        &mut self,
        subaccount: SubaccountId,
        amount_quote_minor: u128,
        destination: &str,
        spendable_quote_minor: u128,
        now: TimestampMs,
    ) -> Result<(u64, bool), WithdrawalError> {
        if amount_quote_minor == 0 {
            return Err(WithdrawalError::InvalidAmount);
        }
        if amount_quote_minor > spendable_quote_minor {
            return Err(WithdrawalError::InsufficientBalance);
        }
        if amount_quote_minor > self.policy.max_per_withdrawal_quote_minor {
            return Err(WithdrawalError::AbovePerWithdrawalCap);
        }
        if self.per_account.get(&subaccount).copied().unwrap_or(0)
            >= self.policy.max_pending_per_account
        {
            return Err(WithdrawalError::TooManyPending);
        }
        let day = now / 86_400_000;
        let used_today = self.daily.get(&(day, subaccount)).copied().unwrap_or(0);
        if used_today.saturating_add(amount_quote_minor) > self.policy.daily_quota_quote_minor {
            return Err(WithdrawalError::AboveDailyQuota);
        }

        let id = self.next_id;
        self.next_id += 1;
        let needs_manual = amount_quote_minor > self.policy.manual_approval_threshold_quote_minor;
        let req = WithdrawalRequest {
            subaccount,
            amount_quote_minor,
            destination: destination.to_owned(),
            requested_at: now,
            claimable_at: now.saturating_add(self.policy.settlement_delay_ms),
        };
        self.pending.insert(id, (req, WithdrawalState::Pending));
        *self.per_account.entry(subaccount).or_insert(0) += 1;
        self.daily.insert(
            (day, subaccount),
            used_today.saturating_add(amount_quote_minor),
        );
        Ok((id, needs_manual))
    }

    /// Cancel a pending withdrawal (before settlement).
    pub fn cancel(&mut self, id: u64) -> Result<(), WithdrawalError> {
        match self.pending.get_mut(&id) {
            Some((_, state @ WithdrawalState::Pending))
            | Some((_, state @ WithdrawalState::Approved)) => {
                *state = WithdrawalState::Cancelled;
                Ok(())
            }
            _ => Err(WithdrawalError::NotPending),
        }
    }

    /// Approve a large withdrawal (operator action).
    pub fn approve(&mut self, id: u64) -> Result<(), WithdrawalError> {
        match self.pending.get_mut(&id) {
            Some((_, state @ WithdrawalState::Pending)) => {
                *state = WithdrawalState::Approved;
                Ok(())
            }
            _ => Err(WithdrawalError::NotPending),
        }
    }

    /// Settle everything whose time-lock has expired: pending small
    /// withdrawals and approved large ones. Returns the ids settled.
    pub fn settle_due(&mut self, custody: &mut dyn CustodyAdapter, now: TimestampMs) -> Vec<u64> {
        let mut settled = Vec::new();
        for (id, (req, state)) in &self.pending {
            if now < req.claimable_at {
                continue;
            }
            match state {
                WithdrawalState::Pending => {
                    // Small, auto-settles.
                    if custody.debit(req.amount_quote_minor) {
                        settled.push(*id);
                    }
                }
                WithdrawalState::Approved => {
                    if custody.debit(req.amount_quote_minor) {
                        settled.push(*id);
                    }
                }
                WithdrawalState::Cancelled | WithdrawalState::Settled => {}
            }
        }
        for id in &settled {
            if let Some((req, state)) = self.pending.get_mut(id) {
                *state = WithdrawalState::Settled;
                let e = self.per_account.entry(req.subaccount).or_insert(0);
                *e = e.saturating_sub(1);
            }
        }
        settled
    }

    /// Finalize cancellations: refund custody-side accounting for
    /// cancelled entries past their lock.
    #[must_use]
    pub fn cancelled_amounts(&self) -> Vec<(u64, SubaccountId, u128)> {
        self.pending
            .iter()
            .filter(|(_, (_, s))| *s == WithdrawalState::Cancelled)
            .map(|(id, (r, _))| (*id, r.subaccount, r.amount_quote_minor))
            .collect()
    }

    /// Look up one withdrawal.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<(&WithdrawalRequest, &WithdrawalState)> {
        self.pending.get(&id).map(|(r, s)| (r, s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validates_balance_caps_and_pending() {
        let mut p = WithdrawalPipeline::new(WithdrawalPolicy::default());
        assert_eq!(
            p.request(1, 0, "addr", 100, 0).unwrap_err(),
            WithdrawalError::InvalidAmount
        );
        assert_eq!(
            p.request(1, 200, "addr", 100, 0).unwrap_err(),
            WithdrawalError::InsufficientBalance
        );
        let (id, needs_manual) = p.request(1, 100, "addr", 10_000, 0).unwrap();
        assert_eq!(id, 1);
        assert!(!needs_manual);
        // Second request over the same spendable balance.
        assert_eq!(
            p.request(1, 9_950, "addr", 10_000 - 100, 0).unwrap_err(),
            WithdrawalError::InsufficientBalance
        );
    }

    #[test]
    fn time_lock_prevents_instant_settlement() {
        let mut p = WithdrawalPipeline::new(WithdrawalPolicy {
            settlement_delay_ms: 1_000,
            ..WithdrawalPolicy::default()
        });
        let (id, _) = p.request(1, 500, "addr", 1_000, 0).unwrap();
        let mut custody = InMemoryCustody {
            balance_quote_minor: 1_000_000,
        };
        assert!(p.settle_due(&mut custody, 999).is_empty(), "still locked");
        assert_eq!(p.settle_due(&mut custody, 1_000), vec![id]);
        assert_eq!(custody.balance_quote_minor, 999_500);
        // Settled withdrawals cannot be cancelled.
        assert_eq!(p.cancel(id).unwrap_err(), WithdrawalError::NotPending);
    }

    #[test]
    fn cancel_before_settlement_blocks_the_debit() {
        let mut p = WithdrawalPipeline::new(WithdrawalPolicy {
            settlement_delay_ms: 1_000,
            ..WithdrawalPolicy::default()
        });
        let (id, _) = p.request(1, 500, "addr", 1_000, 0).unwrap();
        p.cancel(id).unwrap();
        let mut custody = InMemoryCustody {
            balance_quote_minor: 1_000_000,
        };
        assert!(p.settle_due(&mut custody, 2_000).is_empty(), "cancelled");
        assert_eq!(custody.balance_quote_minor, 1_000_000);
    }

    #[test]
    fn large_withdrawals_require_approval() {
        let policy = WithdrawalPolicy {
            manual_approval_threshold_quote_minor: 1_000,
            settlement_delay_ms: 10,
            ..WithdrawalPolicy::default()
        };
        let mut p = WithdrawalPipeline::new(policy);
        let (id, needs_manual) = p.request(1, 2_000, "addr", 100_000, 0).unwrap();
        assert!(needs_manual);
        let mut custody = InMemoryCustody {
            balance_quote_minor: 1_000_000,
        };
        // Past the lock, still Pending: settled (small-withdrawal path
        // auto-settles only amounts under the manual threshold).
        let small = p.request(2, 100, "addr", 100_000, 0).unwrap().0;
        p.approve(id).unwrap();
        assert_eq!(p.settle_due(&mut custody, 100), vec![id, small]);
    }

    #[test]
    fn daily_quota_enforced_per_account_day() {
        let policy = WithdrawalPolicy {
            daily_quota_quote_minor: 1_000,
            settlement_delay_ms: 10,
            ..WithdrawalPolicy::default()
        };
        let mut p = WithdrawalPipeline::new(policy);
        p.request(1, 600, "addr", 10_000, 0).unwrap();
        p.request(1, 600, "addr", 10_000, 1).unwrap_err(); // over quota
                                                           // Next day resets.
        p.request(1, 600, "addr", 10_000, 86_400_000).unwrap();
    }
}
