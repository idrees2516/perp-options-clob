//! LP underwriter vaults (G-16): shared-risk backstop capital.
//!
//! ## The design (Lyra v2 / Thales SAFE-shaped, resolved for this engine)
//!
//! The insurance fund is a single balance, and a single balance has a
//! scaling problem: the coverage the venue can promise is capped by the
//! capital one treasury is willing to lock. An underwriter vault lets any
//! account *deposit* quote cash into a shared backstop pool, receive
//! shares, and earn the insurance share of fee revenue pro-rata —
//! converting the fund from a treasury line-item into a market anyone
//! can supply.
//!
//! The economics are deliberately conservative:
//!
//! * **Epoch processing.** Subscriptions and redemptions queue during an
//!   epoch and settle at its boundary — no mid-epoch dilution, the exact
//!   mechanic every audit of an open-ended vault demands.
//! * **NAV accounting is exact integer math.** Shares are issued at
//!   `nav_per_share = collateral / shares` (initially 1:1); subscription
//!   shares are floored (the vault keeps the dust), redemptions are
//!   ceiled against available collateral — no unit of account is created.
//! * **Backstop draws are the risk.** When the engine arms a vault as a
//!   loss absorber (the insurance fund is exhausted), a draw debits the
//!   vault's collateral and reduces NAV per share — LPs lose exactly what
//!   the cascade would otherwise socialize through ADL. Draws are the
//!   product; ADL frequency is what they buy down.
//! * **Revenue share.** A configurable fraction of the insurance revenue
//!   allocation is routed to vaults (pro-rata by collateral), the yield
//!   LPs earn for standing behind the book.
//!
//! Determinism: the vault is pure state plus integer arithmetic; every
//! mutation is planned by the engine's sweep and applied through
//! journaled [`VaultEpochSettled`](poc_engine::VaultEpochSettled) events.

use std::collections::BTreeMap;

use poc_core::mul_div;

/// A shared backstop vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LpVault {
    /// Vault id (engine-assigned).
    pub vault_id: u64,
    /// Outstanding shares.
    pub shares: u128,
    /// Collateral backing the shares, quote minor.
    pub collateral_quote_minor: u128,
    /// Subscription queue: (subscriber, quote minor).
    pub pending_subscriptions: Vec<(u64, u128)>,
    /// Redemption queue: (redeemer, shares).
    pub pending_redemptions: Vec<(u64, u128)>,
    /// Per-subscriber share holdings (proof-of-claim ledger: the
    /// liability the vault owes each depositor, required by
    /// proof-of-reserves (G-35) and enforced by redemption
    /// validation — a redeemer can only burn shares they hold).
    pub shareholders: BTreeMap<u64, u128>,
    /// Share of the insurance revenue allocation routed here, bps.
    pub revenue_share_bps: u64,
    /// Epoch index of the last settlement.
    pub last_epoch: u64,
    /// Wall-clock of the last epoch settlement (0 = never).
    pub last_settle_ts: u64,
}

/// The result of settling one vault epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochOutcome {
    /// Shares issued this epoch.
    pub subscribed_shares: u128,
    /// Quote minor received.
    pub subscribed_quote_minor: u128,
    /// Shares burned this epoch.
    pub redeemed_shares: u128,
    /// Quote minor paid out.
    pub redeemed_quote_minor: u128,
    /// NAV per share after the epoch.
    pub nav_after_quote_minor: u128,
    /// Per-subscriber signed quote flows (subscriptions negative,
    /// redemptions positive) — the exact cash movements to apply.
    pub flows: Vec<(u64, i128)>,
}

impl LpVault {
    /// A fresh vault with 1:1 initial NAV.
    #[must_use]
    pub fn new(vault_id: u64, revenue_share_bps: u64) -> Self {
        Self {
            vault_id,
            shares: 0,
            collateral_quote_minor: 0,
            pending_subscriptions: Vec::new(),
            pending_redemptions: Vec::new(),
            shareholders: BTreeMap::new(),
            revenue_share_bps,
            last_epoch: 0,
            last_settle_ts: 0,
        }
    }

    /// Current NAV per share (quote minor); 1:1 while empty.
    #[must_use]
    pub fn nav_per_share_quote_minor(&self) -> u128 {
        if self.shares == 0 {
            return 1_000_000;
        }
        (self.collateral_quote_minor * 1_000_000) / self.shares
    }

    /// Queue a subscription for the next epoch boundary.
    pub fn subscribe(&mut self, subscriber: u64, amount_quote_minor: u128) {
        if amount_quote_minor > 0 {
            self.pending_subscriptions
                .push((subscriber, amount_quote_minor));
        }
    }

    /// Queue a redemption for the next epoch boundary. Shares the
    /// redeemer does not hold are clipped at queue time — a redemption
    /// is a burn of *your own* claim, never of the pool's.
    pub fn redeem(&mut self, redeemer: u64, shares: u128) {
        if shares == 0 {
            return;
        }
        let held = self.shareholders.get(&redeemer).copied().unwrap_or(0);
        let burnable = shares.min(held);
        if burnable > 0 {
            self.pending_redemptions.push((redeemer, burnable));
        }
    }

    /// Shares a subaccount currently holds.
    #[must_use]
    pub fn holdings(&self, subscriber: u64) -> u128 {
        self.shareholders.get(&subscriber).copied().unwrap_or(0)
    }

    /// The claim a shareholder can prove against the vault at the
    /// current NAV, quote minor — the liability entry proof-of-reserves
    /// publishes (share x NAV, floored in the holder's disfavour so the
    /// published liability never exceeds the provable one).
    #[must_use]
    pub fn claim_quote_minor(&self, subscriber: u64) -> u128 {
        let shares = self.holdings(subscriber);
        if shares == 0 {
            return 0;
        }
        mul_div(
            shares,
            self.nav_per_share_quote_minor(),
            1_000_000,
            poc_core::Rounding::Floor,
        )
        .unwrap_or(0)
    }

    /// Credit insurance revenue (raises NAV, no new shares).
    pub fn credit_revenue(&mut self, amount_quote_minor: u128) {
        self.collateral_quote_minor = self
            .collateral_quote_minor
            .saturating_add(amount_quote_minor);
    }

    /// A backstop draw: collateral leaves, NAV per share falls. LPs bear
    /// the loss the cascade would otherwise socialize.
    pub fn draw(&mut self, amount_quote_minor: u128) {
        self.collateral_quote_minor = self
            .collateral_quote_minor
            .saturating_sub(amount_quote_minor);
    }

    /// Settle the epoch: process queues at the pre-epoch NAV, then
    /// recompute. Pure integer accounting — subscription shares are
    /// floored (dust stays), redemption collateral is floored — and the
    /// returned flows carry the exact per-subscriber cash movements.
    pub fn settle_epoch(&mut self) -> EpochOutcome {
        let nav = self.nav_per_share_quote_minor();
        let scale = 1_000_000u128;
        let mut flows: Vec<(u64, i128)> = Vec::new();

        // Subscriptions: shares = amount * scale / nav, floored.
        let mut subscribed_shares = 0u128;
        let mut subscribed_quote = 0u128;
        for (sub, amount) in self.pending_subscriptions.drain(..) {
            let shares = mul_div(amount, scale, nav, poc_core::Rounding::Floor).unwrap_or(0);
            if shares == 0 {
                // Dust subscription: nothing debited, nothing issued.
                continue;
            }
            let debited = mul_div(shares, nav, scale, poc_core::Rounding::Floor)
                .unwrap_or(0)
                .min(amount);
            subscribed_shares = subscribed_shares.saturating_add(shares);
            subscribed_quote = subscribed_quote.saturating_add(debited);
            self.shares = self.shares.saturating_add(shares);
            self.collateral_quote_minor = self.collateral_quote_minor.saturating_add(debited);
            let holdings = self.shareholders.get(&sub).copied().unwrap_or(0);
            self.shareholders
                .insert(sub, holdings.saturating_add(shares));
            flows.push((sub, -poc_core::to_i128(debited)));
        }

        // Redemptions: collateral = shares * nav / scale, floored — the
        // vault never pays out more than it computed. Each redemption
        // burns the redeemer's own holdings (queue-time validation
        // clipped anything beyond them).
        let mut redeemed_shares = 0u128;
        let mut redeemed_quote = 0u128;
        for (red, shares) in self.pending_redemptions.drain(..) {
            let held = self.shareholders.get(&red).copied().unwrap_or(0);
            let by_pool = self.shares.saturating_sub(redeemed_shares);
            let burn = shares.min(held).min(by_pool);
            if burn == 0 {
                continue;
            }
            let payout = mul_div(burn, nav, scale, poc_core::Rounding::Floor).unwrap_or(0);
            redeemed_shares = redeemed_shares.saturating_add(burn);
            redeemed_quote = redeemed_quote.saturating_add(payout);
            self.shares = self.shares.saturating_sub(burn);
            self.collateral_quote_minor = self.collateral_quote_minor.saturating_sub(payout);
            let remaining = held.saturating_sub(burn);
            if remaining == 0 {
                self.shareholders.remove(&red);
            } else {
                self.shareholders.insert(red, remaining);
            }
            flows.push((red, poc_core::to_i128(payout)));
        }

        self.last_epoch = self.last_epoch.saturating_add(1);
        EpochOutcome {
            subscribed_shares,
            subscribed_quote_minor: subscribed_quote,
            redeemed_shares,
            redeemed_quote_minor: redeemed_quote,
            nav_after_quote_minor: self.nav_per_share_quote_minor(),
            flows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_starts_at_par() {
        let v = LpVault::new(1, 0);
        assert_eq!(v.nav_per_share_quote_minor(), 1_000_000);
    }

    #[test]
    fn epoch_round_trip_conserves() {
        let mut v = LpVault::new(1, 0);
        v.subscribe(10, 500_000);
        let out = v.settle_epoch();
        assert_eq!(out.subscribed_shares, 500_000);
        assert_eq!(out.subscribed_quote_minor, 500_000);
        assert_eq!(v.nav_per_share_quote_minor(), 1_000_000);

        // Revenue raises NAV.
        v.credit_revenue(250_000);
        assert_eq!(v.nav_per_share_quote_minor(), 1_500_000);

        // Redemption at the raised NAV.
        v.redeem(10, 250_000);
        let out = v.settle_epoch();
        assert_eq!(out.redeemed_shares, 250_000);
        assert_eq!(out.redeemed_quote_minor, 375_000);
        assert_eq!(v.shares, 250_000);
        assert_eq!(v.collateral_quote_minor, 375_000);
    }

    #[test]
    fn draw_reduces_nav() {
        let mut v = LpVault::new(1, 0);
        v.subscribe(10, 1_000_000);
        v.settle_epoch();
        v.draw(400_000);
        assert_eq!(v.nav_per_share_quote_minor(), 600_000);
    }

    #[test]
    fn oversubscribed_redemption_bounded() {
        let mut v = LpVault::new(1, 0);
        v.subscribe(10, 100);
        v.settle_epoch();
        v.redeem(10, 1_000_000);
        let out = v.settle_epoch();
        assert_eq!(out.redeemed_shares, 100);
        assert_eq!(v.shares, 0);
    }

    #[test]
    fn redemption_requires_owned_shares() {
        let mut v = LpVault::new(1, 0);
        v.subscribe(10, 1_000);
        v.settle_epoch();
        // Account 11 holds nothing: the redemption queues zero burns.
        v.redeem(11, 500);
        let out = v.settle_epoch();
        assert_eq!(out.redeemed_shares, 0);
        assert_eq!(v.shares, 1_000);
        assert_eq!(v.holdings(11), 0);
        // Account 10 redeeming more than held clips at holdings.
        v.redeem(10, 5_000);
        let out = v.settle_epoch();
        assert_eq!(out.redeemed_shares, 1_000);
        assert_eq!(v.holdings(10), 0);
        assert!(!v.shareholders.contains_key(&10));
    }

    #[test]
    fn shareholder_holdings_track_multiple_subscribers() {
        let mut v = LpVault::new(1, 0);
        v.subscribe(10, 600);
        v.subscribe(11, 400);
        let out = v.settle_epoch();
        assert_eq!(out.subscribed_shares, 1_000);
        assert_eq!(v.holdings(10), 600);
        assert_eq!(v.holdings(11), 400);
        assert_eq!(v.shares, 600 + 400);
        // Claims at par NAV sum to collateral.
        assert_eq!(v.claim_quote_minor(10) + v.claim_quote_minor(11), 1_000);
        // Revenue raises claims pro-rata.
        v.credit_revenue(500);
        let c10 = v.claim_quote_minor(10);
        let c11 = v.claim_quote_minor(11);
        assert_eq!(c10, 900); // 600 shares x 1.5 NAV
        assert_eq!(c11, 600);
        assert!(c10 + c11 <= v.collateral_quote_minor);
        // Partial redemption reduces holdings only.
        v.redeem(10, 300);
        v.settle_epoch();
        assert_eq!(v.holdings(10), 300);
        assert_eq!(v.holdings(11), 400);
    }

    #[test]
    fn draw_spreads_across_shareholders_proportionally() {
        let mut v = LpVault::new(1, 0);
        v.subscribe(10, 700);
        v.subscribe(11, 300);
        v.settle_epoch();
        v.draw(400);
        // NAV per share = 600_000 (0.6); claims floored.
        assert_eq!(v.claim_quote_minor(10), 420); // 700 x 0.6
        assert_eq!(v.claim_quote_minor(11), 180); // 300 x 0.6
    }
}
