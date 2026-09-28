//! LP vault command processing (G-16): queue management and epoch
//! settlement.
//!
//! Commands only *queue* flows ([`Event::VaultQueued`]); cash moves
//! exclusively at epoch boundaries through [`Event::VaultEpochSettled`],
//! whose embedded per-subscriber flows are the exact movements applied.
//! The sweep settles every vault whose boundary has passed; the planner
//! clones the vault, simulates `settle_epoch`, and journals the outcome —
//! apply replays the identical simulation against live state.

use crate::command::Command;
use crate::engine::Engine;
use crate::event::{Event, VaultEpoch};
use poc_core::TimestampMs;

impl Engine {
    /// Plan vault commands.
    pub(crate) fn plan_vault_command(&self, cmd: &Command, now: TimestampMs) -> Vec<Event> {
        match cmd {
            Command::VaultCreate {
                revenue_share_bps, ..
            } => {
                if *revenue_share_bps > 10_000 {
                    return Vec::new();
                }
                vec![Event::VaultOpened {
                    vault_id: self.next_vault_id,
                    revenue_share_bps: *revenue_share_bps,
                    ts: now,
                }]
            }
            Command::VaultSubscribe {
                vault_id,
                subaccount,
                amount_quote_minor,
                ..
            } => {
                // Margin gate: the subscription must be spendable cash
                // (free equity above every existing commitment).
                if *amount_quote_minor == 0 || !self.vaults.contains_key(vault_id) {
                    return Vec::new();
                }
                let Some(account) = self.accounts.get(subaccount) else {
                    return Vec::new();
                };
                let summary = self.effective_margin_summary_at(
                    account,
                    &self.build_marks(now).unwrap_or_default(),
                    now,
                );
                let Some(summary) = summary else {
                    return Vec::new();
                };
                // Spendable = equity - maintenance - reserved order margin.
                let committed = summary
                    .equity_quote_minor
                    .saturating_sub(i128::try_from(summary.maintenance_quote_minor).unwrap_or(0));
                let committed = committed
                    .saturating_sub(i128::try_from(account.order_margin_quote_minor).unwrap_or(0));
                if poc_core::to_i128(*amount_quote_minor) > committed {
                    return Vec::new();
                }
                vec![Event::VaultQueued {
                    vault_id: *vault_id,
                    subaccount: *subaccount,
                    is_subscribe: true,
                    amount: *amount_quote_minor,
                    ts: now,
                }]
            }
            Command::VaultRedeem {
                vault_id,
                subaccount,
                shares,
                ..
            } => {
                if *shares == 0 || !self.vaults.contains_key(vault_id) {
                    return Vec::new();
                }
                vec![Event::VaultQueued {
                    vault_id: *vault_id,
                    subaccount: *subaccount,
                    is_subscribe: false,
                    amount: *shares,
                    ts: now,
                }]
            }
            _ => Vec::new(),
        }
    }

    /// Apply a vault epoch settlement: move the cash, keep the vault
    /// state in lockstep with the planner's simulation.
    pub(crate) fn apply_vault_epoch(&mut self, epoch: &VaultEpoch) {
        if let Some(vault) = self.vaults.get_mut(&epoch.vault_id) {
            // The planner already simulated the settlement on a clone;
            // replay it here so live state matches exactly.
            let _ = vault.settle_epoch();
            vault.collateral_quote_minor = vault
                .collateral_quote_minor
                .saturating_add(epoch.insurance_credit_quote_minor);
            vault.last_settle_ts = epoch.ts;
        }
        for (sub, flow) in &epoch.flows {
            if let Some(account) = self.accounts.get_mut(sub) {
                account.cash_quote_minor = account.cash_quote_minor.saturating_add(*flow);
            }
        }
    }
}

/// The sweep stage that settles due vault epochs (G-16).
pub(crate) fn plan_vault_epochs(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    let interval = engine.config.vault_epoch_interval_ms;
    if interval == 0 {
        return Vec::new();
    }
    let mut events = Vec::new();
    for (id, vault) in &engine.vaults {
        if vault.pending_subscriptions.is_empty() && vault.pending_redemptions.is_empty() {
            continue;
        }
        if now < vault.last_settle_ts.saturating_add(interval) {
            continue;
        }
        // Simulate the settlement on a clone.
        let mut sim = vault.clone();
        let outcome = sim.settle_epoch();
        events.push(Event::VaultEpochSettled(Box::new(VaultEpoch {
            vault_id: *id,
            epoch: sim.last_epoch,
            nav_per_share_quote_minor: sim.nav_per_share_quote_minor(),
            subscribed_shares: outcome.subscribed_shares,
            subscribed_quote_minor: outcome.subscribed_quote_minor,
            redeemed_shares: outcome.redeemed_shares,
            redeemed_quote_minor: outcome.redeemed_quote_minor,
            insurance_credit_quote_minor: 0,
            nav_after_quote_minor: outcome.nav_after_quote_minor,
            flows: outcome.flows,
            ts: now,
        })));
    }
    events
}
