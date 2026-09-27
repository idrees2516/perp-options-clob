//! State-diff capture: fold the engine's live state into settlement
//! commitments and derive the mutation list between two captures.
//!
//! This is the Lighter.xyz-shaped settlement core: rather than settling
//! every trade individually (the L1-naive design), the operator captures
//! the venue's state before and after a batch window, derives the
//! per-account *state diff*, and publishes one commitment per batch. The
//! on-chain footprint is O(changed accounts), not O(trades).

use std::collections::BTreeMap;

use poc_core::{SubaccountId, Symbol};
use poc_engine::Engine;

use crate::state::{
    AccountCommitment, AccountMutation, BUYBACK_SUBACCOUNT, HOUSE_SUBACCOUNT, INSURANCE_SUBACCOUNT,
    REWARDS_SUBACCOUNT,
};

/// A point-in-time capture of every settled balance in the venue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateCapture {
    /// User accounts.
    pub users: BTreeMap<SubaccountId, AccountCommitment>,
    /// House pool balance (fee revenue retained by the venue).
    pub house_quote_minor: i128,
    /// Insurance fund balance.
    pub insurance_quote_minor: i128,
    /// Reward pool available balance.
    pub rewards_quote_minor: u128,
    /// Buyback pool balance (routed share + coverage overflow).
    pub buyback_quote_minor: u128,
}

impl StateCapture {
    /// Capture the engine's current settled state.
    #[must_use]
    pub fn capture(engine: &Engine) -> Self {
        let mut users = BTreeMap::new();
        for (&sub, account) in engine.accounts_iter() {
            let mut positions = BTreeMap::new();
            for (symbol, pos) in &account.positions {
                if pos.signed_lots != 0 {
                    positions.insert(symbol.clone(), pos.signed_lots);
                }
            }
            users.insert(
                sub,
                AccountCommitment {
                    subaccount: sub,
                    cash_quote_minor: account.cash_quote_minor,
                    positions,
                },
            );
        }
        let (insurance, rewards, house, buyback) = engine.venue_pools();
        Self {
            users,
            // The house/buyback pools are cumulative fee-revenue sinks:
            // money that left user accounts and now belongs to the venue.
            house_quote_minor: poc_core::to_i128(house),
            insurance_quote_minor: insurance,
            rewards_quote_minor: rewards,
            buyback_quote_minor: buyback,
        }
    }

    /// Merge the venue pools into the commitment map (for root
    /// computation).
    #[must_use]
    pub fn with_pools(&self) -> BTreeMap<SubaccountId, AccountCommitment> {
        let mut all = self.users.clone();
        for (sub, cash) in [
            (HOUSE_SUBACCOUNT, self.house_quote_minor),
            (INSURANCE_SUBACCOUNT, self.insurance_quote_minor),
            (
                REWARDS_SUBACCOUNT,
                poc_core::to_i128(self.rewards_quote_minor),
            ),
            (
                BUYBACK_SUBACCOUNT,
                poc_core::to_i128(self.buyback_quote_minor),
            ),
        ] {
            all.insert(
                sub,
                AccountCommitment {
                    subaccount: sub,
                    cash_quote_minor: cash,
                    positions: BTreeMap::new(),
                },
            );
        }
        all
    }

    /// The signed venue-wide liability move between two captures:
    /// `Σ user cash deltas + Δinsurance + Δrewards + Δhouse + Δbuyback`.
    /// A non-zero value means value entered or left the tracked universe
    /// — deposits and withdrawals account for it; anything else is a leak
    /// (invariant violation the validator will reject).
    fn tracked_total(&self) -> i128 {
        self.users
            .values()
            .map(|a| a.cash_quote_minor)
            .fold(0_i128, |acc, x| acc.saturating_add(x))
            .saturating_add(self.insurance_quote_minor)
            .saturating_add(poc_core::to_i128(self.rewards_quote_minor))
            .saturating_add(self.house_quote_minor)
            .saturating_add(poc_core::to_i128(self.buyback_quote_minor))
    }

    /// Derive the mutation list from `self` (before) to `after`.
    ///
    /// Returns the per-account mutations plus the conservation residual:
    /// `residual = total(before) − total(after)` — positive when value
    /// *entered* the tracked universe (deposits exceeding withdrawals),
    /// negative when it left. Fees are internally consistent (debited
    /// from users, credited to pools), so a residual can only come from
    /// custodial flows.
    #[must_use]
    pub fn diff_to(&self, after: &StateCapture) -> (Vec<AccountMutation>, i128) {
        let mut mutations: BTreeMap<SubaccountId, AccountMutation> = BTreeMap::new();

        let push_cash =
            |map: &mut BTreeMap<SubaccountId, AccountMutation>, sub: SubaccountId, delta: i128| {
                if delta != 0 {
                    map.entry(sub)
                        .or_insert_with(|| AccountMutation {
                            subaccount: sub,
                            cash_delta_quote_minor: 0,
                            position_deltas: BTreeMap::new(),
                        })
                        .cash_delta_quote_minor = delta;
                }
            };

        // User cash + position deltas.
        let mut symbols: Vec<&Symbol> = Vec::new();
        for before_acct in self.users.values() {
            symbols.extend(before_acct.positions.keys());
        }
        for after_acct in after.users.values() {
            symbols.extend(after_acct.positions.keys());
        }
        symbols.sort_unstable();
        symbols.dedup();

        for sub in self
            .users
            .keys()
            .copied()
            .chain(after.users.keys().copied())
        {
            let (b, a) = (self.users.get(&sub), after.users.get(&sub));
            let b_cash = b.map_or(0, |x| x.cash_quote_minor);
            let a_cash = a.map_or(0, |x| x.cash_quote_minor);
            push_cash(&mut mutations, sub, a_cash.saturating_sub(b_cash));

            let mut deltas = BTreeMap::new();
            for &symbol in &symbols {
                let b_lots = b
                    .and_then(|x| x.positions.get(symbol))
                    .copied()
                    .unwrap_or(0);
                let a_lots = a
                    .and_then(|x| x.positions.get(symbol))
                    .copied()
                    .unwrap_or(0);
                if b_lots != a_lots {
                    deltas.insert(symbol.clone(), a_lots.saturating_sub(b_lots));
                }
            }
            if !deltas.is_empty() {
                mutations
                    .entry(sub)
                    .or_insert_with(|| AccountMutation {
                        subaccount: sub,
                        cash_delta_quote_minor: 0,
                        position_deltas: BTreeMap::new(),
                    })
                    .position_deltas = deltas;
            }
        }

        // Pool deltas.
        push_cash(
            &mut mutations,
            INSURANCE_SUBACCOUNT,
            after
                .insurance_quote_minor
                .saturating_sub(self.insurance_quote_minor),
        );
        push_cash(
            &mut mutations,
            REWARDS_SUBACCOUNT,
            poc_core::to_i128(after.rewards_quote_minor)
                .saturating_sub(poc_core::to_i128(self.rewards_quote_minor)),
        );
        push_cash(
            &mut mutations,
            HOUSE_SUBACCOUNT,
            after
                .house_quote_minor
                .saturating_sub(self.house_quote_minor),
        );
        push_cash(
            &mut mutations,
            BUYBACK_SUBACCOUNT,
            poc_core::to_i128(after.buyback_quote_minor)
                .saturating_sub(poc_core::to_i128(self.buyback_quote_minor)),
        );

        let residual = self.tracked_total().saturating_sub(after.tracked_total());
        (mutations.into_values().collect(), residual)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poc_core::{Instrument, PerpMarket, Side};
    use poc_engine::{Command, Engine, EngineConfig, OrderRequest};

    fn engine_with_market() -> Engine {
        let mut e = Engine::new(EngineConfig::default());
        e.register_instrument(Instrument::Perp(PerpMarket::default()));
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: 1_000,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: 1_000 });
        e
    }

    #[test]
    fn trades_fee_and_fund_flows_conserve() {
        let mut e = engine_with_market();
        e.process(Command::Deposit {
            subaccount: 1,
            amount_quote_minor: 10_000_000,
        });
        e.process(Command::Deposit {
            subaccount: 2,
            amount_quote_minor: 10_000_000,
        });

        let before = StateCapture::capture(&e);
        e.process(Command::Place {
            request: OrderRequest::limit(2, "BTC-PERP", Side::Ask, 79_950, 3),
            now: 1_001,
        });
        e.process(Command::Place {
            request: OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_960, 3),
            now: 1_002,
        });

        let after = StateCapture::capture(&e);
        let (mutations, residual) = before.diff_to(&after);
        // Fees are debited from users and routed to the venue pools by the
        // engine's revenue router: zero residual.
        assert_eq!(residual, 0, "no custodial flow in this window");
        assert_eq!(mutations.len(), 4, "two users + house + buyback pools");
        let m1 = mutations.iter().find(|m| m.subaccount == 1).unwrap();
        let m2 = mutations.iter().find(|m| m.subaccount == 2).unwrap();
        assert_eq!(m1.position_deltas.get("BTC-PERP"), Some(&3));
        assert_eq!(m2.position_deltas.get("BTC-PERP"), Some(&-3));
        // Every debited fee unit lands in a venue pool (conservation):
        // taker 10 + maker 3 = house 9 + buyback 4.
        assert_eq!(
            m1.cash_delta_quote_minor, -10,
            "taker fee only (fresh open)"
        );
        assert_eq!(m2.cash_delta_quote_minor, -3, "maker fee only (fresh open)");
        let pool_delta: i128 = mutations
            .iter()
            .filter(|m| m.subaccount >= crate::state::USER_SUBACCOUNT_CEILING)
            .map(|m| m.cash_delta_quote_minor)
            .sum();
        assert_eq!(pool_delta, 13, "house 9 + buyback 4 = the routed fees");
    }

    #[test]
    fn deposit_and_withdraw_move_the_residual() {
        let mut e = engine_with_market();
        let before = StateCapture::capture(&e);
        e.process(Command::Deposit {
            subaccount: 7,
            amount_quote_minor: 500,
        });
        let after = StateCapture::capture(&e);
        let (mutations, residual) = before.diff_to(&after);
        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].cash_delta_quote_minor, 500);
        assert_eq!(residual, -500, "value entered the tracked universe");

        let before2 = after;
        e.process(Command::Withdraw {
            subaccount: 7,
            amount_quote_minor: 200,
        });
        let after2 = StateCapture::capture(&e);
        let (_, residual2) = before2.diff_to(&after2);
        assert_eq!(residual2, 200, "value left the tracked universe");
    }
}
