//! The engine: state, command planning, and event application.
//!
//! ## Plan / apply discipline
//!
//! Every `process(command)` call runs two phases:
//!
//! 1. **plan** (`&self`) — pure decision-making: matching outcomes, margin
//!    checks, settlement amounts. Nothing mutates.
//! 2. **apply** (`&mut self`, one [`Event`] at a time) — the *only* code
//!    path that mutates books, accounts, oracles, and funds.
//!
//! The event batch produced by phase 1 is the journal; replaying it
//! reproduces the state exactly (verified by the determinism tests). This
//! is the discipline dYdX v4 uses to make off-chain matching auditable.

use std::collections::{BTreeMap, VecDeque};

use poc_core::{
    mul_div, to_i128, Instrument, Order, OrderId, OrderType, Side, SubaccountId, Symbol,
    TimeInForce, TimestampMs,
};
use poc_economics::{FeeCalculator, FeeSchedule, LiquidityIncentives, RevenueRouter, RewardPool};
use poc_margin::{
    Health, MarginAccount, MarginSummary, Mark, MarkSet, OpenOrderInfo, PortfolioMarginEngine,
};
use poc_oracle::{AssetOracle, OracleConfig};
use poc_orderbook::LimitOrderBook;
use poc_rfq::{BlockLedger, RfqBook};
use poc_risk::{InsuranceFund, LiquidationPlanner, OrderRiskContext, Rejection, RiskLimits};
use poc_volsurface::{SurfaceConfig, VolSurface};

use crate::institutions::{self, MmpState};
use crate::listing::UnderlyingListing;

use crate::command::{Command, OrderRequest};
use crate::event::{
    BookView, Event, InstrumentKindView, MarketStateView, OrderCloseReason, OrderRejected, Trade,
};

/// Auto-listing policy (G-34): which underlyings the sweep manages strike
/// grids for. Empty by default (no auto-listing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListingPolicy {
    /// Managed underlyings.
    pub underlyings: Vec<UnderlyingListing>,
}

/// Implied-volatility and margin configuration knobs.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineConfig {
    /// Pre-trade risk limits.
    pub limits: RiskLimits,
    /// Liquidation cascade parameters.
    pub liquidation: poc_risk::LiquidationParams,
    /// Insurance fund seed.
    pub insurance_seed_quote_minor: u128,
    /// Liquidity reward budget per reward interval.
    pub reward_per_interval_quote_minor: u128,
    /// Reward interval in ms (default 1h).
    pub reward_interval_ms: TimestampMs,
    /// Maker-incentive scoring parameters.
    pub incentive_params: poc_economics::IncentiveParams,
    /// Fee ladder (per-subaccount tiers).
    pub fee_schedule: FeeSchedule,
    /// Portfolio margin overrides per underlying.
    pub margin_overrides: BTreeMap<String, poc_margin::UnderlyingMarginParams>,
    /// Configured implied volatility per option market (the theoretical
    /// mark; books may trade either side of it).
    pub option_ivs: BTreeMap<Symbol, f64>,
    /// Oracle aggregation config.
    pub oracle_config: OracleConfig,
    /// Risk-free rate for analytics.
    pub risk_free_rate: f64,
    /// Live volatility surface governance (G-04: anchor-blend-govern).
    pub surface_config: SurfaceConfig,
    /// Option fee premium caps (F-2: Deribit/Derive rule).
    pub option_fee_caps: poc_economics::OptionFeeCaps,
    /// Insurance coverage policy (G-40: penalty ladder + buyback overflow).
    pub coverage_policy: poc_economics::CoveragePolicy,
    /// Circuit-breaker parameters (G-21: price dislocation + cascade velocity).
    pub breaker: BreakerParams,
    /// Marginable collateral currencies (G-17). Empty = quote-only.
    pub collateral: Vec<crate::collateral::CollateralCurrency>,
    /// Impact-notional for funding premium sampling (G-39): the bid/ask
    /// walk depth in quote minor. 0 disables (BBO mid is used instead).
    pub impact_notional_quote_minor: u128,
    /// Strike auto-listing policy (G-34).
    pub listing: ListingPolicy,
    /// Iterative ADL (G-19): max rounds per liquidation sweep and the
    /// fraction of each counterparty's position one round may close
    /// (bps). 0 rounds = legacy single-shot ADL.
    /// Iterative ADL (G-19): max rounds per liquidation sweep.
    pub adl_max_rounds: u32,
    /// Iterative ADL (G-19): fraction of each counterparty's position
    /// one round may close, bps.
    pub adl_round_bps: u64,
    /// Portfolio greeks limits (G-41): vega/gamma caps enforced
    /// pre-trade on option orders. Default: disabled.
    pub greeks_limits: poc_risk::GreeksLimits,
    /// Volatility-index publication (G-05): moneyness weighting band in
    /// bps of spot (options within the band weight the index).
    pub vol_index_band_bps: u64,
    /// Collateral interest (G-18): per-currency daily rate on the
    /// *utilized* portion of haircut collateral, bps per day.
    pub collateral_interest_bps_per_day: BTreeMap<String, u64>,
    /// LP vault epoch interval (G-16), ms. 0 disables epoch settlement.
    pub vault_epoch_interval_ms: u64,
    /// Insurance rebalancing (G-23): max lots dripped back into the book
    /// per sweep, and the minimum edge over the inventory's mark before a
    /// drip is worth the market impact (bps).
    /// Insurance rebalancing (G-23): max lots per drip.
    pub insurance_rebalance_max_lots: u64,
    /// Insurance rebalancing (G-23): minimum bid edge over the carrying
    /// mark before a drip is worth the market impact, bps.
    pub insurance_rebalance_edge_bps: u64,
    /// Market-maker tier program (G-15): the obligations ladder and
    /// review cadence. Disabled (`MmTierProgram::disabled`) opts the
    /// venue out.
    pub mm_program: poc_economics::MmTierProgram,
    /// Quote-balance interest (G-18 completion): daily bps charged on
    /// the *utilized* portion of positive quote cash. Default 0 — a
    /// governance decision to enable, never a default cost.
    pub quote_interest_bps_per_day: u64,
}

/// Circuit-breaker parameters (G-21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerParams {
    /// Sustained BBO-mid dislocation from the oracle mark beyond this
    /// many bps trips the per-instrument price breaker.
    pub dislocation_bps: u64,
    /// The dislocation must persist this long before tripping.
    pub dislocation_window_ms: TimestampMs,
    /// Breaker cooldown before the instrument accepts orders again.
    pub cooldown_ms: TimestampMs,
    /// Liquidation closures per velocity window that suspends the cascade.
    pub max_closures_per_window: u64,
    /// Cascade velocity window, ms.
    pub velocity_window_ms: TimestampMs,
}

impl Default for BreakerParams {
    fn default() -> Self {
        Self {
            dislocation_bps: 500,
            dislocation_window_ms: 60_000,
            cooldown_ms: 120_000,
            max_closures_per_window: 50,
            velocity_window_ms: 60_000,
        }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            limits: RiskLimits::default(),
            liquidation: poc_risk::LiquidationParams::default(),
            insurance_seed_quote_minor: 50_000_000,   // $500k
            reward_per_interval_quote_minor: 100_000, // $1k
            reward_interval_ms: 60 * 60 * 1000,
            incentive_params: poc_economics::IncentiveParams::default(),
            fee_schedule: FeeSchedule::mainnet(),
            margin_overrides: BTreeMap::new(),
            option_ivs: BTreeMap::new(),
            oracle_config: OracleConfig::default(),
            risk_free_rate: 0.0,
            surface_config: SurfaceConfig::default(),
            option_fee_caps: poc_economics::OptionFeeCaps::default(),
            coverage_policy: poc_economics::CoveragePolicy::default(),
            breaker: BreakerParams::default(),
            collateral: Vec::new(),
            impact_notional_quote_minor: 1_000_000, // $10k walk (2dp quote)
            listing: ListingPolicy::default(),
            adl_max_rounds: 4,
            adl_round_bps: 5_000, // 50% of each counterparty per round
            greeks_limits: poc_risk::GreeksLimits::default(),
            vol_index_band_bps: 1_000,
            collateral_interest_bps_per_day: BTreeMap::new(),
            vault_epoch_interval_ms: 24 * 60 * 60 * 1000,
            insurance_rebalance_max_lots: 10,
            insurance_rebalance_edge_bps: 50,
            mm_program: poc_economics::MmTierProgram::mainnet(),
            quote_interest_bps_per_day: 0,
        }
    }
}

/// One proof-of-reserves liability row (G-35): what the venue owes one
/// subaccount, at oracle prices, pre-haircut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PorLiabilityRow {
    /// The subaccount.
    pub subaccount: SubaccountId,
    /// Positive quote-currency cash, quote minor.
    pub quote_cash_minor: u128,
    /// Non-quote collateral: `(code, balance minor, full oracle value
    /// quote minor)`.
    pub collateral: Vec<(String, u128, u128)>,
    /// Vault share claims at current NAV, quote minor.
    pub vault_claims_quote_minor: u128,
}

/// Aggregate statistics of the running engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineStats {
    /// Events journaled.
    pub events: u64,
    /// Trades executed.
    pub trades: u64,
    /// Contracts traded (lots).
    pub lots_traded: u64,
    /// Notional traded, quote minor.
    pub notional_traded: u128,
    /// Fee revenue routed (house/insurance/buyback).
    pub revenue: poc_economics::Allocation,
    /// Insurance fund balance.
    pub insurance_balance: i128,
    /// Funding intervals settled.
    pub funding_intervals: u64,
    /// Options settled at expiry.
    pub options_settled: u64,
    /// American early exercises settled.
    pub exercises_settled: u64,
    /// Liquidation closures executed.
    pub liquidations: u64,
    /// ADL executions.
    pub adls: u64,
}

/// The exchange engine.
pub struct Engine {
    pub(crate) config: EngineConfig,

    pub(crate) instruments: BTreeMap<Symbol, Instrument>,
    pub(crate) books: BTreeMap<Symbol, LimitOrderBook>,
    pub(crate) oracles: BTreeMap<String, AssetOracle>,
    pub(crate) accounts: BTreeMap<SubaccountId, MarginAccount>,

    pub(crate) margin_engine: PortfolioMarginEngine,
    pub(crate) fee_schedule: FeeSchedule,
    pub(crate) revenue_router: RevenueRouter,
    pub(crate) incentives: LiquidityIncentives,
    pub(crate) reward_pool: RewardPool,
    pub(crate) insurance: InsuranceFund,
    pub(crate) liq_planner: LiquidationPlanner,

    pub(crate) halted: BTreeMap<String, bool>,

    pub(crate) next_order_id: OrderId,
    pub(crate) seq: u64,
    pub(crate) now: TimestampMs,
    pub(crate) journal: Vec<Event>,

    pub(crate) next_funding_ts: BTreeMap<Symbol, TimestampMs>,
    pub(crate) next_reward_ts: TimestampMs,
    /// BBO-mid / spot sample series per perp, for funding TWAPs.
    pub(crate) mark_samples: BTreeMap<Symbol, VecDeque<(TimestampMs, u128)>>,
    pub(crate) last_trade_price: BTreeMap<Symbol, u128>,
    /// Margin reserved per resting order id.
    pub(crate) reservations: BTreeMap<OrderId, u128>,
    /// Parked stop orders (off-book until triggered).
    pub(crate) stop_orders: BTreeMap<OrderId, Order>,
    /// RFQ state machine (G-11) and block-trade tape (G-13).
    pub(crate) rfq_book: RfqBook,
    pub(crate) blocks: BlockLedger,
    /// Live governed volatility surface (G-04).
    pub(crate) vol_surface: VolSurface,
    /// MMP state per (subaccount, underlying).
    pub(crate) mmp: BTreeMap<(SubaccountId, String), MmpState>,
    /// Cancel-on-disconnect setting per subaccount.
    pub(crate) cancel_on_disconnect: BTreeMap<SubaccountId, bool>,
    /// Buyback overflow pool (G-40): insurance share above target lands here.
    pub(crate) buyback_pool_quote_minor: u128,
    /// Price-breaker cooldowns per instrument (G-21).
    pub(crate) price_breaker_until: BTreeMap<Symbol, TimestampMs>,
    /// Cascade-velocity breaker: liquidation sweeps suspended until.
    pub(crate) cascade_suspended_until: TimestampMs,
    /// Liquidation closures inside the velocity window: (ts, lots).
    /// Read by the velocity breaker and by
    /// (published coverage telemetry, G-21/G-23).
    pub(crate) liq_window: std::collections::VecDeque<(TimestampMs, u64)>,
    /// Open auctions per symbol: uncross deadline (G-12). Presence = the
    /// book accumulates without matching.
    pub(crate) auctions: BTreeMap<Symbol, TimestampMs>,
    /// Parked American early-exercise requests by id (AMER exercise).
    pub(crate) exercises: BTreeMap<u64, crate::exercise::ExerciseRequest>,
    /// Next exercise request id (monotonic, deterministic).
    pub(crate) next_exercise_id: u64,
    /// Non-quote collateral balances per subaccount (G-17):
    /// currency code → minor units.
    pub(crate) collateral: BTreeMap<SubaccountId, BTreeMap<String, u128>>,
    /// OCO groups (G-08): group id → the two sibling order ids.
    pub(crate) oco_groups: BTreeMap<u64, (OrderId, OrderId)>,
    /// Next OCO group id.
    pub(crate) next_oco_id: u64,
    /// TWAP parents (G-10): parent id → slicer state.
    pub(crate) twap_parents: BTreeMap<u64, crate::event::TwapParent>,
    /// Next TWAP parent id.
    pub(crate) next_twap_id: u64,
    /// Last published 30-day volatility index per underlying (G-05),
    /// permille (index x 1000).
    pub(crate) vol_index_permille: BTreeMap<String, u64>,
    /// Day index of the last collateral-interest accrual (G-18).
    pub(crate) last_collateral_interest_day: u64,
    /// Insurance fund position inventory (G-23): symbol →
    /// (signed lots, last mark quote minor per base).
    pub(crate) insurance_inventory: BTreeMap<Symbol, (i64, u128)>,
    /// LP underwriter vaults (G-16).
    pub(crate) vaults: BTreeMap<u64, poc_economics::LpVault>,
    /// Next vault id.
    pub(crate) next_vault_id: u64,
    /// MM tier program ledger (G-15): enrollment + in-flight window
    /// stats, fed by the journaled liquidity samples.
    pub(crate) mm_ledger: poc_economics::MmLedger,
    /// Active MM tier fee discounts per subaccount (G-15), bps. Set
    /// only by journaled `MmTierAdjusted` events.
    pub(crate) mm_discount_bps: BTreeMap<SubaccountId, u64>,
    /// Next MM tier review boundary (G-15).
    pub(crate) next_mm_review_ts: TimestampMs,
    /// Day index of the last quote-interest accrual (G-18).
    pub(crate) last_quote_interest_day: u64,

    // statistics
    pub(crate) trades: u64,
    pub(crate) lots_traded: u64,
    pub(crate) notional_traded: u128,
    pub(crate) funding_intervals: u64,
    pub(crate) options_settled: u64,
    pub(crate) exercises_settled: u64,
    pub(crate) liquidations: u64,
    pub(crate) adls: u64,
}

impl Engine {
    /// Build an engine from a configuration.
    ///
    /// An invalid `surface_config` is replaced by its default (the surface
    /// is a governed mark producer; the engine refuses to die for it).
    #[must_use]
    pub fn new(config: EngineConfig) -> Self {
        let mut margin_engine = PortfolioMarginEngine::new();
        for (base, params) in &config.margin_overrides {
            margin_engine.set_params(base.clone(), *params);
        }
        let liq_planner = LiquidationPlanner::new(config.liquidation);
        let fee_schedule = config.fee_schedule.clone();
        let incentives = LiquidityIncentives::new(config.incentive_params);
        let reward_pool = RewardPool::new(config.reward_per_interval_quote_minor);
        let insurance = InsuranceFund::new(config.insurance_seed_quote_minor);
        let next_reward_ts = config.reward_interval_ms;
        // Built before the struct literal (the literal moves `config`).
        let vol_surface = VolSurface::new(config.surface_config)
            .or_else(|_| VolSurface::new(SurfaceConfig::default()))
            .expect("default surface config is valid");
        let next_mm_review_ts = config.mm_program.review_interval_ms;
        Self {
            config,
            instruments: BTreeMap::new(),
            books: BTreeMap::new(),
            oracles: BTreeMap::new(),
            accounts: BTreeMap::new(),
            margin_engine,
            fee_schedule,
            revenue_router: RevenueRouter::default(),
            incentives,
            reward_pool,
            insurance,
            liq_planner,
            halted: BTreeMap::new(),
            next_order_id: 1,
            seq: 0,
            now: 0,
            journal: Vec::new(),
            next_funding_ts: BTreeMap::new(),
            next_reward_ts,
            mark_samples: BTreeMap::new(),
            last_trade_price: BTreeMap::new(),
            reservations: BTreeMap::new(),
            stop_orders: BTreeMap::new(),
            rfq_book: RfqBook::new(),
            blocks: BlockLedger::new(poc_rfq::BlockLedger::DEFAULT_DELAY_MS),
            vol_surface,
            mmp: BTreeMap::new(),
            cancel_on_disconnect: BTreeMap::new(),
            buyback_pool_quote_minor: 0,
            price_breaker_until: BTreeMap::new(),
            cascade_suspended_until: 0,
            liq_window: std::collections::VecDeque::new(),
            auctions: BTreeMap::new(),
            exercises: BTreeMap::new(),
            next_exercise_id: 1,
            collateral: BTreeMap::new(),
            oco_groups: BTreeMap::new(),
            next_oco_id: 1,
            twap_parents: BTreeMap::new(),
            next_twap_id: 1,
            vol_index_permille: BTreeMap::new(),
            last_collateral_interest_day: 0,
            insurance_inventory: BTreeMap::new(),
            vaults: BTreeMap::new(),
            next_vault_id: 1,
            mm_ledger: poc_economics::MmLedger::new(),
            mm_discount_bps: BTreeMap::new(),
            next_mm_review_ts,
            last_quote_interest_day: 0,
            trades: 0,
            lots_traded: 0,
            notional_traded: 0,
            funding_intervals: 0,
            options_settled: 0,
            exercises_settled: 0,
            liquidations: 0,
            adls: 0,
        }
    }

    /// The engine's configuration.
    #[must_use]
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Register an instrument. Idempotent per symbol; creates its book and
    /// underlying oracle. The listing is journaled, so replays reconstruct
    /// the registry exactly.
    pub fn register_instrument(&mut self, instrument: Instrument) {
        let event = Event::MarketListed {
            instrument,
            anchor_iv_bps: None,
        };
        self.apply_event(&event);
        self.journal.push(event);
    }

    /// The instrument registry (read-only view).
    #[must_use]
    pub fn instruments(&self) -> &BTreeMap<Symbol, Instrument> {
        &self.instruments
    }

    /// One book (read-only).
    #[must_use]
    pub fn book(&self, symbol: &str) -> Option<&LimitOrderBook> {
        self.books.get(symbol)
    }

    /// One account's margin account (read-only).
    #[must_use]
    pub fn account(&self, subaccount: SubaccountId) -> Option<&MarginAccount> {
        self.accounts.get(&subaccount)
    }

    /// Every margin account, ascending subaccount id (deterministic).
    #[must_use = "the iterator borrows the engine; use it or drop it"]
    pub fn accounts_iter(&self) -> impl Iterator<Item = (&SubaccountId, &MarginAccount)> {
        self.accounts.iter()
    }

    /// The insurance fund's carried position inventory (G-23):
    /// symbol -> (signed lots, carrying mark quote minor per base).
    #[must_use]
    pub fn insurance_inventory(&self) -> &BTreeMap<Symbol, (i64, u128)> {
        &self.insurance_inventory
    }

    /// The LP underwriter vaults (G-16), by id.
    #[must_use]
    pub fn vaults(&self) -> &BTreeMap<u64, poc_economics::LpVault> {
        &self.vaults
    }

    /// Liquidation closures counted inside the cascade-velocity window
    /// (G-21) — coverage telemetry for the stress dashboards.
    #[must_use]
    pub fn cascade_closures_in_window(&self, now: TimestampMs) -> u64 {
        let window = self.config.breaker.velocity_window_ms;
        self.liq_window
            .iter()
            .filter(|(ts, _)| now.saturating_sub(*ts) <= window)
            .map(|(_, lots)| *lots)
            .sum::<u64>()
            .saturating_add(0)
    }

    /// The most recently published volatility index per underlying
    /// (G-05), permille (index x 1000).
    #[must_use]
    pub fn vol_index(&self, base: &str) -> Option<u64> {
        self.vol_index_permille.get(base).copied()
    }

    /// The venue's internal pools (settlement-layer conservation inputs):
    /// `(insurance balance, reward pool available, cumulative fee revenue
    /// routed to the house, buyback pool balance)`, quote minor.
    #[must_use = "pool state is for settlement-conservation checks"]
    pub fn venue_pools(&self) -> (i128, u128, u128, u128) {
        let allocation = self.revenue_router.cumulative();
        (
            self.insurance.balance(),
            self.reward_pool.available(),
            allocation.house,
            self.buyback_pool_quote_minor,
        )
    }

    /// The full journal since genesis.
    #[must_use]
    pub fn journal(&self) -> &[Event] {
        &self.journal
    }

    /// The active MM tier fee discount of a subaccount (G-15), bps.
    /// Zero when the account holds no tier.
    #[must_use]
    pub fn mm_discount_of(&self, subaccount: SubaccountId) -> u64 {
        self.mm_discount_bps.get(&subaccount).copied().unwrap_or(0)
    }

    /// The MM tier program ledger view (G-15): enrolled subaccounts and
    /// their in-flight window statistics.
    #[must_use]
    pub fn mm_ledger(&self) -> &poc_economics::MmLedger {
        &self.mm_ledger
    }

    /// Build the proof-of-reserves liability rows (G-35): one row per
    /// subaccount — positive quote cash, non-quote collateral at full
    /// (un-haircut) oracle value, and vault share claims at current
    /// NAV. Liabilities are what customers can *claim*; the venue's
    /// haircut is its own risk buffer, not the customer's.
    ///
    /// Publication itself is a settlement-layer act
    /// (`poc_settlement::por`): the engine exposes the projection, the
    /// settlement crate commits it.
    #[must_use]
    pub fn por_liabilities(&self, now: TimestampMs) -> Vec<PorLiabilityRow> {
        let mut vault_claims: BTreeMap<SubaccountId, u128> = BTreeMap::new();
        for vault in self.vaults.values() {
            for holder in vault.shareholders.keys() {
                let claim = vault.claim_quote_minor(*holder);
                let acc = vault_claims.entry(*holder).or_insert(0);
                *acc = acc.saturating_add(claim);
            }
        }
        let mut rows: Vec<PorLiabilityRow> = Vec::with_capacity(self.accounts.len());
        for (sub, account) in &self.accounts {
            // A negative balance is a debt the customer owes the venue,
            // not a liability the venue owes them.
            let cash = account.cash_quote_minor.max(0).unsigned_abs();
            let mut collateral = Vec::new();
            if let Some(balances) = self.collateral.get(sub) {
                for (code, minor) in balances {
                    let value = self
                        .collateral_price(code, now)
                        .and_then(|price| {
                            let unit = self
                                .collateral_config(code)
                                .map(|cfg| 10_u128.saturating_pow(cfg.decimals))
                                .unwrap_or(1);
                            mul_div(*minor, price, unit, poc_core::Rounding::Floor)
                        })
                        .unwrap_or(0);
                    collateral.push((code.clone(), *minor, value));
                }
            }
            let claims = vault_claims.get(sub).copied().unwrap_or(0);
            rows.push(PorLiabilityRow {
                subaccount: *sub,
                quote_cash_minor: cash,
                collateral,
                vault_claims_quote_minor: claims,
            });
        }
        rows
    }

    /// Engine wall clock.
    #[must_use]
    pub fn now(&self) -> TimestampMs {
        self.now
    }

    // ------------------------------------------------------------------
    // Process: plan then apply
    // ------------------------------------------------------------------

    /// Process one command; returns the events it produced (also appended
    /// to the journal).
    ///
    /// Batches, OCO pairs, and ticks run in *sequential-commit* mode:
    /// each member is planned and applied before the next is planned, so
    /// order ids advance and later members see earlier fills (G-08/G-09
    /// fix for the shared-id collision and phantom double-fills). The
    /// pre-pass gates in the batch/OCO planners keep the command atomic
    /// on validity and margin.
    pub fn process(&mut self, cmd: Command) -> Vec<Event> {
        match cmd {
            Command::PlaceBatch { requests, now } => {
                if let Some(rejection) = self.batch_gate(&requests, now) {
                    return self.commit(rejection);
                }
                let mut events = Vec::new();
                for request in requests {
                    events.extend(self.process(Command::Place { request, now }));
                }
                events
            }
            Command::PlaceOco { first, second, now } => self.process_place_oco(first, second, now),
            Command::PlaceTwap {
                subaccount,
                symbol,
                side,
                total_lots,
                slices,
                slice_interval_ms,
                limit_ticks,
                now,
            } => self.process_place_twap(
                subaccount,
                symbol,
                side,
                total_lots,
                slices,
                slice_interval_ms,
                limit_ticks,
                now,
            ),
            Command::CancelTwap {
                subaccount,
                parent_id,
                now,
            } => self.process_cancel_twap(subaccount, parent_id, now),
            Command::Tick { now } => self.process_tick(now),
            cmd => self.process_one(cmd),
        }
    }

    /// Plan, cascade OCO siblings, apply, journal — the default path.
    fn process_one(&mut self, cmd: Command) -> Vec<Event> {
        let mut events = self.plan(&cmd);
        let siblings = self.plan_oco_siblings(&events);
        events.extend(siblings);
        self.commit(events)
    }

    /// Apply a planned event batch and journal it.
    pub(crate) fn commit(&mut self, events: Vec<Event>) -> Vec<Event> {
        for event in &events {
            self.apply_event(event);
        }
        self.journal.extend(events.iter().cloned());
        events
    }

    pub(crate) fn plan(&self, cmd: &Command) -> Vec<Event> {
        match cmd {
            Command::Deposit {
                subaccount,
                amount_quote_minor,
            } => {
                vec![Event::Deposit {
                    subaccount: *subaccount,
                    amount_quote_minor: *amount_quote_minor,
                    ts: self.now,
                }]
            }
            Command::Withdraw {
                subaccount,
                amount_quote_minor,
            } => self.plan_withdraw(*subaccount, *amount_quote_minor),
            Command::Place { request, now } => self.plan_place(request, *now),
            Command::Exercise {
                subaccount,
                symbol,
                lots,
                now,
            } => crate::exercise::plan_exercise(self, *subaccount, symbol, *lots, *now),
            Command::Cancel {
                subaccount,
                order_id,
                ..
            } => self.plan_cancel(*subaccount, *order_id),
            Command::CancelAll {
                subaccount, symbol, ..
            } => self.plan_cancel_all(*subaccount, symbol.as_deref()),
            Command::OracleUpdate {
                base_symbol,
                provider,
                ts,
                price_quote_minor,
            } => {
                vec![Event::ProviderObserved {
                    base_symbol: base_symbol.clone(),
                    provider: provider.clone(),
                    ts: *ts,
                    price_quote_minor: *price_quote_minor,
                }]
            }
            Command::Tick { now } => crate::sweep::plan_tick(self, *now),
            // Intercepted in `process` (sequential commit); unreachable here.
            Command::PlaceBatch { .. } => Vec::new(),
            Command::PlaceOco { .. } | Command::PlaceTwap { .. } | Command::CancelTwap { .. } => {
                Vec::new()
            }
            Command::RfqCreate {
                taker,
                legs,
                counterparties,
                min_total_cost_quote_minor,
                max_total_cost_quote_minor,
                ttl_ms,
                now,
            } => self.plan_rfq_create(
                *taker,
                legs,
                counterparties,
                *min_total_cost_quote_minor,
                *max_total_cost_quote_minor,
                *ttl_ms,
                *now,
            ),
            Command::RfqQuote {
                maker,
                rfq_id,
                leg_prices_ticks,
                ttl_ms,
                now,
            } => self.plan_rfq_quote(*maker, *rfq_id, leg_prices_ticks, *ttl_ms, *now),
            Command::RfqExecute {
                taker,
                rfq_id,
                quote_id,
                now,
            } => institutions::plan_rfq_execute(self, *taker, *rfq_id, *quote_id, *now),
            Command::RfqCancel {
                subaccount,
                rfq_id,
                quote_id,
                now,
            } => self.plan_rfq_cancel(*subaccount, *rfq_id, *quote_id, *now),
            Command::BlockTrade {
                taker,
                maker,
                legs,
                now,
            } => self.plan_block(*taker, *maker, legs, *now),
            Command::Transfer {
                from,
                to,
                amount_quote_minor,
                now,
            } => self.plan_transfer(*from, *to, *amount_quote_minor, *now),
            Command::SetMmp {
                subaccount,
                base_symbol,
                interval_ms,
                frozen_time_ms,
                amount_limit_lots,
                delta_limit_lots,
                ..
            } => vec![Event::MmpConfigured {
                subaccount: *subaccount,
                base_symbol: base_symbol.clone(),
                interval_ms: *interval_ms,
                frozen_time_ms: *frozen_time_ms,
                amount_limit_lots: *amount_limit_lots,
                delta_limit_lots: *delta_limit_lots,
            }],
            Command::SetCod {
                subaccount,
                enabled,
                now,
            } => vec![Event::CodChanged {
                subaccount: *subaccount,
                enabled: *enabled,
                ts: *now,
            }],
            Command::SessionDropped { subaccount, now } => {
                let mut events = Vec::new();
                if self
                    .cancel_on_disconnect
                    .get(subaccount)
                    .copied()
                    .unwrap_or(false)
                {
                    events.extend(self.plan_cancel_all(*subaccount, None));
                }
                events.push(Event::SessionDisconnected {
                    subaccount: *subaccount,
                    canceled_orders: Vec::new(),
                    ts: *now,
                });
                events
            }
            Command::CancelBatch {
                subaccount,
                order_ids,
                ..
            } => self.plan_cancel_batch(*subaccount, order_ids),
            Command::Amend {
                subaccount,
                order_id,
                new_price_ticks,
                new_open_lots,
                now,
            } => self.plan_amend(
                *subaccount,
                *order_id,
                *new_price_ticks,
                *new_open_lots,
                *now,
            ),
            Command::BeginAuction {
                symbol,
                uncross_at,
                now,
            } => {
                if self.instruments.contains_key(symbol) && *uncross_at > *now {
                    vec![Event::AuctionOpened {
                        symbol: symbol.clone(),
                        uncross_at: *uncross_at,
                        ts: *now,
                    }]
                } else {
                    Vec::new()
                }
            }
            Command::DepositCollateral {
                subaccount,
                currency,
                amount_minor,
                now,
            } => self.plan_collateral_deposit(*subaccount, currency, *amount_minor, *now),
            Command::WithdrawCollateral {
                subaccount,
                currency,
                amount_minor,
                now,
            } => self.plan_collateral_withdraw(*subaccount, currency, *amount_minor, *now),
            Command::ConvertCollateral {
                subaccount,
                from,
                to,
                from_amount_minor,
                now,
            } => self.plan_collateral_convert(*subaccount, from, to, *from_amount_minor, *now),
            Command::VaultCreate { .. }
            | Command::VaultSubscribe { .. }
            | Command::VaultRedeem { .. } => self.plan_vault_command(cmd, self.now),
            Command::MmTierEnroll { subaccount, now } => {
                if self.accounts.contains_key(subaccount) {
                    vec![Event::MmEnrolled {
                        subaccount: *subaccount,
                        ts: *now,
                    }]
                } else {
                    Vec::new()
                }
            }
        }
    }

    fn plan_rfq_create(
        &self,
        taker: SubaccountId,
        legs: &[crate::command::RfqLegCommand],
        counterparties: &[SubaccountId],
        min_total: Option<u128>,
        max_total: Option<u128>,
        ttl_ms: TimestampMs,
        now: TimestampMs,
    ) -> Vec<Event> {
        if ttl_ms == 0 || legs.is_empty() {
            return vec![Event::RfqRejected {
                subaccount: taker,
                reason: "invalid rfq parameters",
                ts: now,
            }];
        }
        if legs.iter().any(|l| l.qty_lots == 0) {
            return vec![Event::RfqRejected {
                subaccount: taker,
                reason: "leg quantity must be positive",
                ts: now,
            }];
        }
        if counterparties.contains(&taker) {
            return vec![Event::RfqRejected {
                subaccount: taker,
                reason: "cannot direct rfq to self",
                ts: now,
            }];
        }
        let Some(built) = institutions::build_rfq_legs(self, legs) else {
            return vec![Event::RfqRejected {
                subaccount: taker,
                reason: "unknown instrument in package",
                ts: now,
            }];
        };
        vec![Event::RfqCreated {
            taker,
            legs: built,
            counterparties: counterparties.to_vec(),
            min_total_cost_quote_minor: min_total,
            max_total_cost_quote_minor: max_total,
            ttl_ms,
            ts: now,
        }]
    }

    fn plan_rfq_quote(
        &self,
        maker: SubaccountId,
        rfq_id: u64,
        leg_prices: &[u64],
        ttl_ms: TimestampMs,
        now: TimestampMs,
    ) -> Vec<Event> {
        let Some(rfq) = self.rfq_book.rfq(rfq_id) else {
            return vec![Event::RfqRejected {
                subaccount: maker,
                reason: "unknown rfq",
                ts: now,
            }];
        };
        if rfq.status != poc_rfq::RfqStatus::Open || now > rfq.valid_until {
            return vec![Event::RfqRejected {
                subaccount: maker,
                reason: "rfq not open",
                ts: now,
            }];
        }
        if maker == rfq.taker {
            return vec![Event::RfqRejected {
                subaccount: maker,
                reason: "cannot quote own rfq",
                ts: now,
            }];
        }
        if !rfq.counterparties.is_empty() && !rfq.counterparties.contains(&maker) {
            return vec![Event::RfqRejected {
                subaccount: maker,
                reason: "not a directed counterparty",
                ts: now,
            }];
        }
        if leg_prices.len() != rfq.legs.len() || leg_prices.contains(&0) || ttl_ms == 0 {
            return vec![Event::RfqRejected {
                subaccount: maker,
                reason: "invalid quote parameters",
                ts: now,
            }];
        }
        vec![Event::RfqQuoted {
            rfq_id,
            maker,
            leg_prices_ticks: leg_prices.to_vec(),
            ttl_ms,
            ts: now,
        }]
    }

    fn plan_rfq_cancel(
        &self,
        subaccount: SubaccountId,
        rfq_id: Option<u64>,
        quote_id: Option<u64>,
        now: TimestampMs,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some(rfq_id) = rfq_id {
            let owned = self
                .rfq_book
                .rfq(rfq_id)
                .is_some_and(|r| r.taker == subaccount);
            if owned {
                events.push(Event::RfqClosed {
                    rfq_id,
                    quote_id: None,
                    reason: "cancelled",
                });
            }
        }
        if let Some(quote_id) = quote_id {
            let owned = self
                .rfq_book
                .quote(quote_id)
                .is_some_and(|q| q.maker == subaccount);
            if owned {
                events.push(Event::RfqClosed {
                    rfq_id: 0,
                    quote_id: Some(quote_id),
                    reason: "cancelled",
                });
            }
        }
        if events.is_empty() {
            events.push(Event::RfqRejected {
                subaccount,
                reason: "nothing to cancel",
                ts: now,
            });
        }
        events
    }

    fn plan_block(
        &self,
        taker: SubaccountId,
        maker: SubaccountId,
        legs: &[(Symbol, Side, u64, u64)],
        now: TimestampMs,
    ) -> Vec<Event> {
        if taker == maker || legs.is_empty() {
            return vec![Event::RfqRejected {
                subaccount: taker,
                reason: "invalid block parameters",
                ts: now,
            }];
        }
        for (symbol, _side, qty, px) in legs {
            if *qty == 0 || *px == 0 {
                return vec![Event::RfqRejected {
                    subaccount: taker,
                    reason: "invalid block leg",
                    ts: now,
                }];
            }
            if !self.instruments.contains_key(symbol) {
                return vec![Event::RfqRejected {
                    subaccount: taker,
                    reason: "unknown instrument in block",
                    ts: now,
                }];
            }
        }
        match institutions::rfq_margin_ok(self, taker, maker, legs, now) {
            Some(true) => {}
            _ => {
                return vec![Event::RfqRejected {
                    subaccount: taker,
                    reason: "insufficient margin for block",
                    ts: now,
                }]
            }
        }
        let total: u128 = legs
            .iter()
            .filter_map(|(s, _, q, p)| {
                self.instruments
                    .get(s)
                    .and_then(|i| i.notional_quote_minor(*p, *q))
            })
            .sum();
        vec![Event::BlockRegistered {
            taker,
            maker,
            legs: legs.to_vec(),
            total_notional_quote_minor: total,
            taker_fees_quote_minor: institutions::rfq_taker_fees(self, taker, legs),
            broadcast_ts: now.saturating_add(poc_rfq::BlockLedger::DEFAULT_DELAY_MS),
        }]
    }

    fn plan_transfer(
        &self,
        from: SubaccountId,
        to: SubaccountId,
        amount: u128,
        now: TimestampMs,
    ) -> Vec<Event> {
        let reject = |reason: &'static str| {
            vec![Event::TransferRejected {
                from,
                to,
                requested: amount,
                reason,
                ts: now,
            }]
        };
        if from == to || amount == 0 {
            return reject("invalid transfer");
        }
        let Some(account) = self.accounts.get(&from) else {
            return reject("unknown source account");
        };
        if (to == from || !self.accounts.contains_key(&to)) && !self.accounts.contains_key(&to) {
            return reject("unknown destination account");
        }
        let _ = account;
        if let Some(summary) = self.margin_summary_of(from) {
            if summary.available_quote_minor() < to_i128(amount) {
                return reject("amount exceeds free equity");
            }
        } else {
            return reject("source account cannot be margined");
        }
        vec![Event::TransferExecuted {
            from,
            to,
            amount_quote_minor: amount,
            ts: now,
        }]
    }

    fn plan_withdraw(&self, subaccount: SubaccountId, amount: u128) -> Vec<Event> {
        let reject = |reason: &'static str| {
            vec![Event::WithdrawRejected {
                subaccount,
                requested: amount,
                reason,
                ts: self.now,
            }]
        };
        let Some(account) = self.accounts.get(&subaccount) else {
            return reject("unknown account");
        };
        if amount == 0 {
            return reject("amount must be positive");
        }
        if account.cash_quote_minor < to_i128(amount) {
            return reject("insufficient cash");
        }
        // Withdrawals may only take *free* equity: capital leaves only
        // when it is not backing margin or resting orders.
        if let Some(summary) = self.margin_summary_of(subaccount) {
            if summary.available_quote_minor() < to_i128(amount) {
                return reject("amount exceeds free equity");
            }
        }
        vec![Event::Withdrawal {
            subaccount,
            amount_quote_minor: amount,
            ts: self.now,
        }]
    }

    pub(crate) fn plan_place(&self, request: &OrderRequest, now: TimestampMs) -> Vec<Event> {
        let order_id = self.next_order_id;
        let rejection = |reason: Rejection| {
            vec![Event::OrderRejection(Box::new(OrderRejected {
                request: request.clone(),
                reason,
                order_id,
            }))]
        };

        let Some(instrument) = self.instruments.get(&request.symbol) else {
            return rejection(Rejection::UnknownInstrument);
        };
        let Some(account) = self.accounts.get(&request.subaccount) else {
            return rejection(Rejection::UnknownAccount);
        };
        if let Err(why) = instrument.validate_qty(request.qty_lots) {
            return rejection(Rejection::InvalidOrder(why.to_string()));
        }
        // G-41: portfolio greeks caps on option orders (worst-case
        // increment at the current mark; disabled caps skip the math).
        if let Instrument::Option(m) = instrument {
            if !self.config.greeks_limits.disabled() {
                if let Some(rej) = self.check_greeks_limit(request, m) {
                    return rejection(rej);
                }
            }
        }
        if let Some(ticks) = request.price_ticks {
            if instrument.price_quote_minor(ticks).is_none() {
                return rejection(Rejection::InvalidOrder("price off tick grid".into()));
            }
        }

        let mut order = Order {
            id: order_id,
            subaccount: request.subaccount,
            symbol: request.symbol.clone(),
            side: request.side,
            order_type: request.order_type,
            price_ticks: request.price_ticks,
            qty_lots: request.qty_lots,
            filled_lots: 0,
            tif: request.tif,
            post_only: request.post_only,
            reduce_only: request.reduce_only,
            stp: request.stp,
            display_lots: request.display_lots,
            trailing_extreme_quote_minor: None,
            oco_group: request.oco_group,
            client_ts: request.client_ts,
            engine_ts: now,
        };

        // Reduce-only: cap at the current position size.
        if order.reduce_only {
            let current = account.lots_of(&request.symbol);
            let cap = poc_risk::reduce_only_cap(&order, current);
            if cap == 0 {
                return rejection(Rejection::ReduceOnlyWouldIncrease);
            }
            order.qty_lots = cap;
        }

        // Marks and the pre-trade risk gate (stops are gated too: they
        // may trigger into positions later).
        let marks = self.build_marks(now);
        let mark_price = marks
            .as_ref()
            .and_then(|m| self.instrument_mark(instrument, m));
        let (marks, mark_price) = match (marks, mark_price) {
            (Some(m), Some(p)) => (m, p),
            _ => return rejection(Rejection::MissingMark),
        };

        // Trailing stops anchor their running extreme at the current mark
        // (G-07); every later tick can only tighten the trigger from here.
        if order.order_type.is_parked()
            && matches!(
                order.order_type,
                OrderType::TrailingStopMarket { .. } | OrderType::TrailingStopLimit { .. }
            )
        {
            order.trailing_extreme_quote_minor = Some(mark_price);
        }

        let Some(book) = self.books.get(&request.symbol) else {
            return rejection(Rejection::UnknownInstrument);
        };

        // MMP freeze: a tripped protection blocks new orders for that
        // (subaccount, currency) until the freeze lapses (Derive MMP).
        if let Some(mmp) = self
            .mmp
            .get(&(request.subaccount, instrument.base_symbol().to_owned()))
        {
            if mmp.is_frozen(now) {
                return rejection(Rejection::InvalidOrder("mmp frozen".into()));
            }
        }

        let estimated_fee = self.estimate_fee(instrument, &order, mark_price);
        let risk_ctx = OrderRiskContext {
            order: &order,
            instrument,
            instruments: &self.instruments,
            account,
            marks: &marks,
            margin_engine: &self.margin_engine,
            mark_quote_minor: mark_price,
            best_bid_ticks: book.best_bid(),
            best_ask_ticks: book.best_ask(),
            limits: self.config.limits,
            halted: self
                .halted
                .get(instrument.base_symbol())
                .copied()
                .unwrap_or(false),
            estimated_fee_quote_minor: estimated_fee,
            collateral_equity_quote_minor: to_i128(
                self.collateral_value_of(request.subaccount, now),
            ),
            collateral_spot_exposures: self.collateral_spot_exposures_of(request.subaccount),
        };
        if let Err(reason) = poc_risk::check_order(&risk_ctx) {
            return rejection(reason);
        }

        // Stop orders (and trailing stops) park off-book until the mark
        // crosses their trigger.
        if order.order_type.is_parked() {
            return vec![Event::OrderResting {
                order,
                margin_reserved_quote_minor: 0,
            }];
        }

        // Auctions (G-12): limit orders rest without matching until the
        // uncross; anything that would sweep is refused.
        if self
            .books
            .get(&order.symbol)
            .is_some_and(|b| b.auction_mode())
        {
            if !matches!(order.order_type, OrderType::Limit) {
                return rejection(Rejection::InvalidOrder(
                    "only limit orders rest in an auction".into(),
                ));
            }
            if order.price_ticks.is_none() {
                return rejection(Rejection::InvalidOrder(
                    "auction orders carry a price".into(),
                ));
            }
            let auction_reservation = poc_risk::order_margin_increment(&risk_ctx);
            return vec![Event::OrderResting {
                order,
                margin_reserved_quote_minor: auction_reservation,
            }];
        }

        // Margin the resting remainder will reserve.
        let reservation = poc_risk::order_margin_increment(&risk_ctx);
        self.plan_match(&order, now, reservation)
    }

    /// Match an incoming order against its book, emitting trade, STP, and
    /// lifecycle events (including the `OrderResting` for remainders).
    /// Shared by placements and stop activations.
    pub(crate) fn plan_match(
        &self,
        order: &Order,
        now: TimestampMs,
        reservation: u128,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let Some(instrument) = self.instruments.get(&order.symbol) else {
            return events;
        };
        let Some(book) = self.books.get(&order.symbol) else {
            return events;
        };

        let price_limit = match order.order_type {
            OrderType::Market => None,
            _ => order.price_ticks,
        };
        let fok = matches!(order.tif, TimeInForce::Fok);
        let outcome = book.match_taker(order, price_limit, fok, order.stp);

        let mut trades_in_batch = 0_u64;
        for fill in &outcome.fills {
            trades_in_batch += 1;
            let notional = instrument
                .notional_quote_minor(fill.price_ticks, fill.qty_lots)
                .unwrap_or(0);
            let taker_tier = self.fee_schedule.tier_for(fill.taker_subaccount);
            let maker_tier = self.fee_schedule.tier_for(fill.maker_subaccount);
            // The insurance fund's inventory drips pay no taker fee: the
            // synthetic subaccount has no balance to charge, and routing a
            // fee nobody paid into the pools would mint quote. The venue
            // does not tax its own backstop for unwinding what it
            // absorbed (G-23).
            let fee_exempt = fill.taker_subaccount == crate::sweep::INSURANCE_SUBACCOUNT;
            let (taker_fee, maker_fee) = if fee_exempt {
                (0, 0)
            } else {
                match instrument {
                    Instrument::Option(_) => {
                        // Options: the notional above IS the premium value of
                        // the fill; the fee is min(rate x underlying notional,
                        // cap% x premium) — F-2, the Deribit/Derive rule.
                        let caps = &self.config.option_fee_caps;
                        let underlying_notional = self
                            .build_marks(now)
                            .as_ref()
                            .and_then(|ms| ms.get(instrument.base_symbol()))
                            .and_then(|set| {
                                instrument.position_notional_minor(
                                    set.spot_quote_minor_per_base,
                                    i64::try_from(fill.qty_lots).unwrap_or(i64::MAX),
                                )
                            })
                            .unwrap_or(0);
                        (
                            FeeCalculator::option_taker_fee(
                                taker_tier,
                                caps,
                                underlying_notional,
                                notional,
                            )
                            .unwrap_or(0),
                            FeeCalculator::option_maker_fee(
                                maker_tier,
                                caps,
                                underlying_notional,
                                notional,
                            )
                            .unwrap_or(0),
                        )
                    }
                    _ => (
                        FeeCalculator::taker_fee(taker_tier, notional).unwrap_or(0),
                        FeeCalculator::maker_fee(maker_tier, notional).unwrap_or(0),
                    ),
                }
            };
            // G-15: the active MM tier discount composes after the
            // volume ladder (fees round in the payer's favour).
            let taker_fee = poc_economics::apply_tier_discount(
                taker_fee,
                self.mm_discount_of(fill.taker_subaccount),
            );
            let maker_fee = poc_economics::apply_tier_discount(
                maker_fee,
                self.mm_discount_of(fill.maker_subaccount),
            );
            events.push(Event::TradeExecuted(Box::new(Trade {
                seq: self.seq + trades_in_batch - 1,
                symbol: order.symbol.clone(),
                taker_order_id: fill.taker_order_id,
                maker_order_id: fill.maker_order_id,
                taker_subaccount: fill.taker_subaccount,
                maker_subaccount: fill.maker_subaccount,
                maker_side: fill.maker_side,
                price_ticks: fill.price_ticks,
                qty_lots: fill.qty_lots,
                notional_quote_minor: notional,
                taker_fee_quote_minor: taker_fee,
                maker_fee_quote_minor: maker_fee,
                ts: now,
            })));

            // Fully-filled makers leave the book (their reservation is
            // released by the trade application).
            if let Some(resting) = book.get(fill.maker_order_id) {
                if resting.order.open_qty() == fill.qty_lots {
                    let mut final_order = resting.order.clone();
                    final_order.filled_lots = final_order.qty_lots;
                    events.push(Event::OrderClosed {
                        order_id: fill.maker_order_id,
                        subaccount: fill.maker_subaccount,
                        symbol: order.symbol.clone(),
                        order: final_order,
                        reason: OrderCloseReason::Filled,
                    });
                }
            }
        }

        if !outcome.stp.canceled_makers.is_empty() {
            events.push(Event::StpCancels {
                maker_ids: outcome.stp.canceled_makers.clone(),
                ts: now,
            });
        }

        // MMP evaluation (G-37): fills accumulate against each side's
        // protection windows; a trip cancels that side's resting orders in
        // the affected currency and freezes further trading for the window.
        {
            let mut trips: Vec<(SubaccountId, String)> = Vec::new();
            if let Some(base) = self
                .instruments
                .get(&order.symbol)
                .map(|i| i.base_symbol().to_owned())
            {
                for fill in &outcome.fills {
                    let taker_side = fill.maker_side.opposite();
                    for (sub, delta) in [
                        (
                            fill.taker_subaccount,
                            taker_side.sign() * fill.qty_lots as i64,
                        ),
                        (
                            fill.maker_subaccount,
                            fill.maker_side.sign() * fill.qty_lots as i64,
                        ),
                    ] {
                        if let Some(mmp) = self.mmp.get(&(sub, base.clone())) {
                            let mut sim = mmp.clone();
                            if sim.record_fill(now, fill.qty_lots, delta, now)
                                && !trips.contains(&(sub, base.clone()))
                            {
                                trips.push((sub, base.clone()));
                            }
                        }
                    }
                }
                for (sub, base) in trips {
                    events.push(Event::MmpTripped {
                        subaccount: sub,
                        base_symbol: base.clone(),
                        ts: now,
                    });
                    for inst in self.instruments.values() {
                        if inst.base_symbol() != base {
                            continue;
                        }
                        if let Some(book) = self.books.get(inst.symbol()) {
                            for resting in book.resting_orders() {
                                if resting.order.subaccount == sub {
                                    events.push(Event::OrderClosed {
                                        order_id: resting.order.id,
                                        subaccount: sub,
                                        symbol: inst.symbol().to_owned(),
                                        order: resting.order.clone(),
                                        reason: OrderCloseReason::Canceled,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // Taker lifecycle: what remains after fills and STP consumption.
        let remaining = outcome.taker_remaining_lots;
        let consumed = order.open_qty().saturating_sub(remaining);
        let mut final_order = order.clone();
        final_order.filled_lots = consumed; // STP decrements count as consumed

        if remaining == 0 {
            events.push(Event::OrderClosed {
                order_id: order.id,
                subaccount: order.subaccount,
                symbol: order.symbol.clone(),
                order: final_order,
                reason: OrderCloseReason::Filled,
            });
        } else if order.can_rest() && matches!(order.tif, TimeInForce::Gtc | TimeInForce::Gtd(_)) {
            events.push(Event::OrderResting {
                order: final_order,
                margin_reserved_quote_minor: reservation,
            });
        } else {
            events.push(Event::OrderClosed {
                order_id: order.id,
                subaccount: order.subaccount,
                symbol: order.symbol.clone(),
                order: final_order,
                reason: OrderCloseReason::IocRemainder,
            });
        }
        events
    }

    pub(crate) fn plan_cancel(&self, subaccount: SubaccountId, order_id: OrderId) -> Vec<Event> {
        if let Some(order) = self.stop_orders.get(&order_id) {
            if order.subaccount != subaccount {
                return Vec::new();
            }
            return vec![Event::OrderClosed {
                order_id,
                subaccount,
                symbol: order.symbol.clone(),
                order: order.clone(),
                reason: OrderCloseReason::Canceled,
            }];
        }
        let Some(account) = self.accounts.get(&subaccount) else {
            return Vec::new();
        };
        let Some(info) = account.open_orders.get(&order_id) else {
            return Vec::new();
        };
        let Some(order) = self
            .books
            .get(&info.symbol)
            .and_then(|b| b.get(order_id))
            .map(|r| r.order.clone())
        else {
            return Vec::new();
        };
        vec![Event::OrderClosed {
            order_id,
            subaccount,
            symbol: info.symbol.clone(),
            order,
            reason: OrderCloseReason::Canceled,
        }]
    }

    fn plan_cancel_all(&self, subaccount: SubaccountId, symbol: Option<&str>) -> Vec<Event> {
        let Some(account) = self.accounts.get(&subaccount) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        for info in account.open_orders.values() {
            if let Some(sym) = symbol {
                if info.symbol != sym {
                    continue;
                }
            }
            if let Some(order) = self
                .books
                .get(&info.symbol)
                .and_then(|b| b.get(info.order_id))
                .map(|r| r.order.clone())
            {
                events.push(Event::OrderClosed {
                    order_id: info.order_id,
                    subaccount,
                    symbol: info.symbol.clone(),
                    order,
                    reason: OrderCloseReason::Canceled,
                });
            }
        }
        for (id, order) in &self.stop_orders {
            if order.subaccount != subaccount {
                continue;
            }
            if let Some(sym) = symbol {
                if order.symbol != sym {
                    continue;
                }
            }
            events.push(Event::OrderClosed {
                order_id: *id,
                subaccount,
                symbol: order.symbol.clone(),
                order: order.clone(),
                reason: OrderCloseReason::Canceled,
            });
        }
        events
    }

    // ------------------------------------------------------------------
    // Apply: the single mutation path
    // ------------------------------------------------------------------

    /// Apply one journal event. This is the only `&mut self` state
    /// transition in the engine; replay calls it directly.
    pub fn apply_event(&mut self, event: &Event) {
        self.now = self.now.max(event_ts(event));
        match event {
            Event::Deposit {
                subaccount,
                amount_quote_minor,
                ..
            } => {
                let account = self
                    .accounts
                    .entry(*subaccount)
                    .or_insert_with(|| MarginAccount::new(*subaccount, 0));
                account.apply_reward(*amount_quote_minor);
            }
            Event::Withdrawal {
                subaccount,
                amount_quote_minor,
                ..
            } => {
                if let Some(account) = self.accounts.get_mut(subaccount) {
                    account.cash_quote_minor = account
                        .cash_quote_minor
                        .saturating_sub(to_i128(*amount_quote_minor));
                }
            }
            Event::WithdrawRejected { .. } => {}
            Event::MarketListed {
                instrument,
                anchor_iv_bps,
            } => {
                let symbol = instrument.symbol().to_owned();
                let base = instrument.base_symbol().to_owned();
                if let Instrument::Option(m) = &instrument {
                    // Every option market registers on the surface at its
                    // configured anchor IV (G-04 stage 1); the book then
                    // blends and the sweep governs.
                    let anchor_bps = anchor_iv_bps
                        .or_else(|| {
                            self.config
                                .option_ivs
                                .get(&m.symbol)
                                .copied()
                                .map(|iv| (iv * 10_000.0) as u64)
                        })
                        .unwrap_or(5_500);
                    self.vol_surface.register(&m.symbol, anchor_bps);
                    if m.variant == poc_core::OptionVariant::Everlasting {
                        // The roll's first interval starts at listing.
                        self.next_funding_ts
                            .insert(m.symbol.clone(), m.everlasting.interval_ms);
                    }
                }
                if let Instrument::Perp(m) = instrument {
                    let next = self
                        .next_funding_ts
                        .get(&symbol)
                        .copied()
                        .unwrap_or_else(|| self.now.saturating_add(m.funding.interval_ms));
                    self.next_funding_ts.insert(symbol.clone(), next);
                }
                self.oracles.entry(base.clone()).or_insert_with(|| {
                    AssetOracle::new(base.clone(), self.config.oracle_config.clone())
                });
                self.halted.entry(base).or_insert(false);
                self.books
                    .entry(symbol.clone())
                    .or_insert_with(|| LimitOrderBook::new(symbol.clone()));
                self.instruments.insert(symbol, instrument.clone());
            }
            Event::ClockAdvanced { now: new_now } => {
                self.now = self.now.max(*new_now);
            }
            Event::ProviderObserved {
                base_symbol,
                provider,
                ts,
                price_quote_minor,
            } => {
                if let Some(oracle) = self.oracles.get_mut(base_symbol) {
                    oracle.update(provider, *ts, *price_quote_minor);
                }
                // Sample perp marks on this underlying for funding TWAPs.
                if let Some(spot) = self.oracles.get(base_symbol).and_then(|o| o.mark(*ts)) {
                    for symbol in self.perps_on_base(base_symbol) {
                        let mid = self.perp_mark_for_funding(&symbol).unwrap_or(spot);
                        self.push_mark_sample(&symbol, *ts, mid);
                    }
                }
            }
            Event::OrderResting {
                order,
                margin_reserved_quote_minor,
            } => {
                self.next_order_id = self.next_order_id.max(order.id + 1);
                // Activated stop orders leave the parking lot when they
                // rest as real limit orders; fresh stops re-insert below.
                self.stop_orders.remove(&order.id);
                if order.order_type.is_parked() {
                    self.stop_orders.insert(order.id, order.clone());
                    return;
                }
                if let Some(price) = order.price_ticks {
                    if let Some(book) = self.books.get_mut(&order.symbol) {
                        book.insert_resting(order.clone(), price);
                    }
                }
                if let Some(account) = self.accounts.get_mut(&order.subaccount) {
                    account.track_order(OpenOrderInfo {
                        order_id: order.id,
                        symbol: order.symbol.clone(),
                        side: order.side,
                        price_ticks: order.price_ticks.unwrap_or(0),
                        open_lots: order.open_qty(),
                    });
                    account.order_margin_quote_minor = account
                        .order_margin_quote_minor
                        .saturating_add(*margin_reserved_quote_minor);
                }
                self.reservations
                    .insert(order.id, *margin_reserved_quote_minor);
            }
            Event::OrderClosed {
                order_id,
                subaccount,
                symbol,
                reason,
                order,
            } => {
                self.stop_orders.remove(order_id);
                // OCO bookkeeping: a closed member releases its group.
                if order.oco_group.is_some() {
                    self.oco_groups
                        .retain(|_, (a, b)| *a != *order_id && *b != *order_id);
                }
                if *reason != OrderCloseReason::Filled
                    && self
                        .books
                        .get(symbol)
                        .is_some_and(|b| b.get(*order_id).is_some())
                {
                    if let Some(book) = self.books.get_mut(symbol) {
                        book.cancel(*order_id);
                    }
                }
                if let Some(account) = self.accounts.get_mut(subaccount) {
                    account.untrack_order(*order_id);
                    let released = self.reservations.remove(order_id).unwrap_or(0);
                    account.order_margin_quote_minor =
                        account.order_margin_quote_minor.saturating_sub(released);
                }
            }
            Event::OrderRejection(rejection) => {
                self.next_order_id = self.next_order_id.max(rejection.order_id + 1);
                // An OCO leg rejected at the gate releases its group.
                if let Some(group) = rejection.request.oco_group {
                    self.oco_groups.remove(&group);
                }
            }
            Event::TradeExecuted(trade) => self.apply_trade(trade),
            Event::StpCancels { maker_ids, .. } => {
                for symbol in self.books.keys().cloned().collect::<Vec<_>>() {
                    let mut remove = false;
                    if let Some(book) = self.books.get_mut(&symbol) {
                        if maker_ids.iter().any(|id| book.get(*id).is_some()) {
                            book.apply_stp(&poc_orderbook::StpEffects {
                                canceled_makers: maker_ids.clone(),
                                decrements: Vec::new(),
                                taker_consumed_lots: 0,
                                taker_canceled: false,
                            });
                            remove = true;
                        }
                    }
                    if remove {
                        break;
                    }
                }
                for id in maker_ids {
                    for account in self.accounts.values_mut() {
                        if account.open_orders.contains_key(id) {
                            account.untrack_order(*id);
                            let released = self.reservations.remove(id).unwrap_or(0);
                            account.order_margin_quote_minor =
                                account.order_margin_quote_minor.saturating_sub(released);
                        }
                    }
                }
            }
            Event::Funding(settled) => {
                if let Some(next) = self.next_funding_ts.get_mut(&settled.symbol) {
                    let interval = funding_interval_of(&self.instruments, &settled.symbol);
                    *next = (*next).max(settled.ts).saturating_add(interval);
                }
                self.funding_intervals += 1;
            }
            Event::FundingFlow(paid) => {
                if let Some(account) = self.accounts.get_mut(&paid.subaccount) {
                    account.apply_funding(paid.credit_quote_minor);
                }
            }
            Event::OptionExpiry(settled) => {
                if let Some(Instrument::Option(market)) = self.instruments.get(&settled.symbol) {
                    if let Some(account) = self.accounts.get_mut(&settled.subaccount) {
                        // Settlement is a forced close at the option's
                        // terminal value (per-base intrinsic): realized
                        // PnL lands in cash, the position disappears.
                        let terminal = match market.kind {
                            poc_core::OptionKind::Call => settled
                                .settlement_quote_minor
                                .saturating_sub(market.strike_quote_minor),
                            poc_core::OptionKind::Put => market
                                .strike_quote_minor
                                .saturating_sub(settled.settlement_quote_minor),
                        };
                        let closing_side = if settled.signed_lots > 0 {
                            Side::Ask
                        } else {
                            Side::Bid
                        };
                        account.apply_fill(
                            &Instrument::Option(market.clone()),
                            &settled.symbol,
                            closing_side,
                            settled.signed_lots.unsigned_abs(),
                            terminal,
                        );
                        self.options_settled += 1;
                    }
                }
            }
            Event::OptionDelisted { symbol } => {
                self.instruments.remove(symbol);
                self.books.remove(symbol);
                self.mark_samples.remove(symbol);
                // Any exercise requests parked on the delisted market are
                // consumed (the expiry settlement already closed the
                // positions they referenced).
                self.exercises.retain(|_, ex| ex.symbol != *symbol);
            }
            Event::ExerciseQueued {
                request_id,
                subaccount,
                symbol,
                lots,
                requested_at,
                settle_at,
            } => {
                self.exercises.insert(
                    *request_id,
                    crate::exercise::ExerciseRequest {
                        id: *request_id,
                        subaccount: *subaccount,
                        symbol: symbol.clone(),
                        lots: *lots,
                        requested_at: *requested_at,
                        settle_at: *settle_at,
                    },
                );
                self.next_exercise_id = self.next_exercise_id.max(*request_id + 1);
            }
            Event::OptionExercised(ex) => {
                self.apply_exercise(ex);
            }
            Event::ExerciseRejected { .. } => {}
            Event::ExerciseDeferred {
                request_id,
                new_settle_at,
            } => {
                if let Some(req) = self.exercises.get_mut(request_id) {
                    req.settle_at = *new_settle_at;
                }
            }
            Event::LiquidityScored { observations } => {
                for obs in observations {
                    self.incentives.on_observation(
                        obs.subaccount,
                        obs.size_lots,
                        obs.spread_bps,
                        obs.two_sided,
                    );
                }
                // G-15: the same journaled samples feed the MM tier
                // ledger — one measurement, two incentive systems.
                crate::mm::apply_liquidity_scored(self, observations);
            }
            Event::MmEnrolled { subaccount, .. } => {
                self.mm_ledger.enroll(*subaccount);
            }
            Event::MmTierAdjusted {
                subaccount,
                fee_discount_bps,
                ts,
                ..
            } => {
                // Re-derive the ledger transition exactly as planned:
                // the planner drained the window on a clone; replay the
                // drain so live state matches.
                let _ = self.mm_ledger.drain_window();
                if *fee_discount_bps == 0 {
                    self.mm_discount_bps.remove(subaccount);
                } else {
                    self.mm_discount_bps.insert(*subaccount, *fee_discount_bps);
                }
                crate::mm::advance_review_schedule(self);
                let _ = ts;
            }
            Event::QuoteInterestAccrued {
                subaccount,
                amount_quote_minor,
                ts,
            } => {
                // Charge the account, route the income — the same
                // fee-income path every other charge uses.
                if let Some(account) = self.accounts.get_mut(subaccount) {
                    let cash = account.cash_quote_minor;
                    let charge = (*amount_quote_minor as i128).min(cash.max(0));
                    account.cash_quote_minor = cash - charge;
                    if let Ok(amount) = u128::try_from(charge.max(0)) {
                        self.apply_fee_income(amount, *ts);
                    }
                }
                crate::mm::mark_quote_interest_day(self, *ts);
            }
            Event::Reward(paid) => {
                if let Some(account) = self.accounts.get_mut(&paid.subaccount) {
                    account.apply_reward(paid.amount_quote_minor);
                }
            }
            Event::RewardsSettled => {
                // Same state the plan-stage clone had: identical outcome,
                // mutations kept (payments were already applied via
                // `Reward` events).
                let _ = self.incentives.settle(&mut self.reward_pool);
                self.next_reward_ts = self
                    .next_reward_ts
                    .saturating_add(self.config.reward_interval_ms);
            }
            Event::Liquidation(exec) => self.apply_liquidation(exec),
            Event::Adl(exec) => self.apply_adl(exec),
            Event::MarketHalted { base_symbol, .. } => {
                self.halted.insert(base_symbol.clone(), true);
            }
            Event::MarketResumed { base_symbol, .. } => {
                self.halted.insert(base_symbol.clone(), false);
            }
            Event::RfqCreated {
                taker,
                legs,
                counterparties,
                min_total_cost_quote_minor,
                max_total_cost_quote_minor,
                ttl_ms,
                ts,
            } => {
                if self
                    .rfq_book
                    .create_rfq(
                        *taker,
                        legs.clone(),
                        counterparties.clone(),
                        *min_total_cost_quote_minor,
                        *max_total_cost_quote_minor,
                        *ttl_ms,
                        *ts,
                    )
                    .is_err()
                {
                    // Defensive: the plan stage validated everything; a
                    // failure here means the event was hand-crafted.
                }
            }
            Event::RfqQuoted {
                rfq_id,
                maker,
                leg_prices_ticks,
                ttl_ms,
                ts,
            } => {
                if self
                    .rfq_book
                    .send_quote(*maker, *rfq_id, leg_prices_ticks.clone(), *ttl_ms, *ts)
                    .is_err()
                {}
            }
            Event::RfqSettled {
                rfq_id,
                quote_id,
                trades,
                taker,
                ..
            } => {
                // Mutate the RFQ state machine exactly as planned: mark
                // filled, cancel every other live quote.
                let settled_at = trades.first().map_or(self.now, |t| t.ts);
                let _ = self
                    .rfq_book
                    .execute(*taker, *rfq_id, *quote_id, settled_at);
                for trade in trades {
                    self.apply_rfq_trade(trade, *taker);
                }
            }
            Event::RfqRejected { .. } => {}
            Event::RfqClosed {
                rfq_id,
                quote_id,
                reason,
            } => {
                let expired = *reason == "expired";
                if *rfq_id != 0 {
                    if expired {
                        self.rfq_book.expire_rfq_by_id(*rfq_id);
                    } else {
                        self.rfq_book.cancel_rfq_by_id(*rfq_id);
                    }
                }
                if let Some(quote_id) = quote_id {
                    if expired {
                        self.rfq_book.expire_quote_by_id(*quote_id);
                    } else {
                        self.rfq_book.cancel_quote_by_id(*quote_id);
                    }
                }
            }
            Event::BlockRegistered {
                taker,
                maker,
                legs,
                total_notional_quote_minor,
                taker_fees_quote_minor,
                broadcast_ts,
            } => {
                // Settle the negotiated package through the venue's
                // account path (fills, fees, volume, routing) — a block
                // that skips margin/fees is not a feature, it is a hole.
                for (i, (symbol, taker_side, qty, price_ticks)) in legs.iter().enumerate() {
                    let Some(instrument) = self.instruments.get(symbol).cloned() else {
                        continue;
                    };
                    let price = instrument.price_quote_minor(*price_ticks).unwrap_or(0);
                    if let Some(maker_acct) = self.accounts.get_mut(maker) {
                        maker_acct.apply_fill(
                            &instrument,
                            symbol,
                            taker_side.opposite(),
                            *qty,
                            price,
                        );
                    }
                    if let Some(taker_acct) = self.accounts.get_mut(taker) {
                        taker_acct.apply_fill(&instrument, symbol, *taker_side, *qty, price);
                        taker_acct.apply_fee(taker_fees_quote_minor.get(i).copied().unwrap_or(0));
                    }
                    let notional = instrument
                        .notional_quote_minor(*price_ticks, *qty)
                        .unwrap_or(0);
                    self.fee_schedule
                        .record_volume_at(*taker, notional, *broadcast_ts);
                    let gross = taker_fees_quote_minor.get(i).copied().unwrap_or(0).max(0);
                    if gross > 0 {
                        // One fee-income path: vault split + coverage
                        // policy + buyback (G-16/G-40).
                        self.apply_fee_income(gross.unsigned_abs(), *broadcast_ts);
                    }
                    self.trades += 1;
                    self.lots_traded += *qty;
                    self.notional_traded = self.notional_traded.saturating_add(notional);
                }
                let _ = self.blocks.register(
                    *taker,
                    *maker,
                    legs.clone(),
                    *total_notional_quote_minor,
                    broadcast_ts
                        .saturating_sub(poc_rfq::BlockLedger::DEFAULT_DELAY_MS)
                        .max(1),
                );
            }
            Event::BlockPrinted { block_id } => {
                self.blocks.mark_broadcast(*block_id);
            }
            Event::TransferExecuted {
                from,
                to,
                amount_quote_minor,
                ..
            } => {
                if let Some(source) = self.accounts.get_mut(from) {
                    source.cash_quote_minor = source
                        .cash_quote_minor
                        .saturating_sub(to_i128(*amount_quote_minor));
                }
                if let Some(dest) = self.accounts.get_mut(to) {
                    dest.apply_reward(*amount_quote_minor);
                }
            }
            Event::TransferRejected { .. } => {}
            Event::MmpConfigured {
                subaccount,
                base_symbol,
                interval_ms,
                frozen_time_ms,
                amount_limit_lots,
                delta_limit_lots,
            } => {
                self.mmp.insert(
                    (*subaccount, base_symbol.clone()),
                    MmpState {
                        interval_ms: *interval_ms,
                        frozen_time_ms: *frozen_time_ms,
                        amount_limit_lots: *amount_limit_lots,
                        delta_limit_lots: *delta_limit_lots,
                        fills: Vec::new(),
                        frozen_until: 0,
                    },
                );
            }
            Event::MmpTripped {
                subaccount,
                base_symbol,
                ts,
            } => {
                if let Some(mmp) = self.mmp.get_mut(&(*subaccount, base_symbol.clone())) {
                    mmp.frozen_until = if mmp.frozen_time_ms == 0 {
                        u64::MAX
                    } else {
                        ts.saturating_add(mmp.frozen_time_ms)
                    };
                    mmp.fills.clear();
                }
            }
            Event::SessionDisconnected { .. } => {}
            Event::CodChanged {
                subaccount,
                enabled,
                ..
            } => {
                self.cancel_on_disconnect.insert(*subaccount, *enabled);
            }
            Event::BreakerTripped { symbol, .. } => {
                // Velocity breakers suspend the cascade; price breakers
                // block the specific instrument until cooldown.
                if symbol.is_empty() {
                    self.cascade_suspended_until =
                        self.now.saturating_add(self.config.breaker.cooldown_ms);
                } else {
                    self.price_breaker_until.insert(
                        symbol.clone(),
                        self.now.saturating_add(self.config.breaker.cooldown_ms),
                    );
                }
            }
            Event::BreakerReleased { .. } => {}
            Event::SurfaceObserved(obs) => {
                if obs.american {
                    self.vol_surface.observe_american(
                        &obs.symbol,
                        obs.spot_quote_minor,
                        obs.strike_quote_minor,
                        obs.is_call,
                        obs.tte_ms,
                        obs.bid_quote_minor,
                        obs.ask_quote_minor,
                        obs.bid_lots,
                        obs.ask_lots,
                        obs.ts,
                    );
                } else {
                    self.vol_surface.observe(
                        &obs.symbol,
                        obs.spot_quote_minor,
                        obs.strike_quote_minor,
                        obs.is_call,
                        obs.tte_ms,
                        obs.bid_quote_minor,
                        obs.ask_quote_minor,
                        obs.bid_lots,
                        obs.ask_lots,
                        obs.ts,
                    );
                }
            }
            Event::SurfaceSwept { now } => {
                self.vol_surface.sweep(*now);
            }
            Event::OrderAmended(amended) => {
                // In-place size reduction (queue priority kept, G-09).
                let old_open = self
                    .books
                    .get(&amended.symbol)
                    .and_then(|b| b.get(amended.order_id))
                    .map(|r| r.order.open_qty());
                if let Some(old_open) = old_open {
                    let cut = old_open.saturating_sub(amended.new_open_lots);
                    if cut > 0 {
                        if let Some(book) = self.books.get_mut(&amended.symbol) {
                            book.reduce(amended.order_id, cut);
                        }
                    }
                    if let Some(account) = self.accounts.get_mut(&amended.subaccount) {
                        if let Some(info) = account.open_orders.get_mut(&amended.order_id) {
                            info.open_lots = amended.new_open_lots;
                        }
                        // Rescale the order-margin reservation to the new size.
                        if let Some(&reserved) = self.reservations.get(&amended.order_id) {
                            let scaled = mul_div(
                                reserved,
                                u128::from(amended.new_open_lots),
                                u128::from(old_open.max(1)),
                                poc_core::Rounding::Floor,
                            )
                            .unwrap_or(0);
                            let delta = reserved.saturating_sub(scaled);
                            account.order_margin_quote_minor =
                                account.order_margin_quote_minor.saturating_sub(delta);
                            self.reservations.insert(amended.order_id, scaled);
                        }
                    }
                }
            }
            Event::TrailingUpdated {
                order_id,
                extreme_quote_minor,
                ..
            } => {
                if let Some(order) = self.stop_orders.get_mut(order_id) {
                    order.trailing_extreme_quote_minor = Some(*extreme_quote_minor);
                }
            }
            Event::AuctionOpened {
                symbol, uncross_at, ..
            } => {
                self.auctions.insert(symbol.clone(), *uncross_at);
                if let Some(book) = self.books.get_mut(symbol) {
                    book.set_auction_mode(true);
                }
            }
            Event::AuctionUncrossed {
                symbol,
                taker_reductions,
                ..
            } => {
                // Reduce every resting "taker" the trades did not.
                if let Some(book) = self.books.get_mut(symbol) {
                    for (id, lots) in taker_reductions {
                        book.reduce(*id, *lots);
                    }
                    book.set_auction_mode(false);
                }
                self.auctions.remove(symbol);
            }
            Event::CollateralMoved(moved) => {
                self.apply_collateral_move(moved.subaccount, &moved.currency, moved.amount_minor);
            }
            Event::CollateralRejected { .. } => {}
            Event::CollateralConversion(converted) => {
                self.apply_collateral_move(
                    converted.subaccount,
                    &converted.from,
                    -i128::try_from(converted.from_amount_minor).unwrap_or(i128::MAX),
                );
                self.apply_collateral_move(
                    converted.subaccount,
                    &converted.to,
                    i128::try_from(converted.to_amount_minor).unwrap_or(i128::MAX),
                );
            }
            Event::PositionMigrated {
                subaccount,
                from_symbol,
                to_symbol,
                signed_lots,
                close_price_quote_minor,
                open_price_quote_minor,
                ts,
            } => {
                let _ = ts;
                let (Some(from_inst), Some(to_inst)) = (
                    self.instruments.get(from_symbol),
                    self.instruments.get(to_symbol),
                ) else {
                    return;
                };
                let (from_inst, to_inst) = (from_inst.clone(), to_inst.clone());
                let closing_side = if *signed_lots > 0 {
                    Side::Ask
                } else {
                    Side::Bid
                };
                let lots = signed_lots.unsigned_abs();
                if let Some(account) = self.accounts.get_mut(subaccount) {
                    account.apply_fill(
                        &from_inst,
                        from_symbol,
                        closing_side,
                        lots,
                        *close_price_quote_minor,
                    );
                    account.apply_fill(
                        &to_inst,
                        to_symbol,
                        closing_side.opposite(),
                        lots,
                        *open_price_quote_minor,
                    );
                }
            }
            Event::OcoLinked {
                group,
                first,
                second,
                ..
            } => {
                self.oco_groups.insert(*group, (*first, *second));
                self.next_oco_id = self.next_oco_id.max(group + 1);
            }
            Event::TwapOpened(parent) => {
                self.next_twap_id = self.next_twap_id.max(parent.parent_id + 1);
                self.twap_parents.insert(parent.parent_id, *parent.clone());
            }
            Event::TwapSliced {
                parent_id,
                request,
                slice_index,
                ..
            } => {
                if let Some(parent) = self.twap_parents.get_mut(parent_id) {
                    parent.slices_placed = parent.slices_placed.max(slice_index + 1);
                    parent.lots_placed = parent.lots_placed.saturating_add(request.qty_lots);
                    parent.next_slice_ts = parent
                        .next_slice_ts
                        .saturating_add(parent.slice_interval_ms);
                }
            }
            Event::TwapClosed { parent_id, .. } => {
                self.twap_parents.remove(parent_id);
            }
            Event::VolIndexPublished {
                base_symbol,
                index_permille,
                ..
            } => {
                self.vol_index_permille
                    .insert(base_symbol.clone(), *index_permille);
            }
            Event::CollateralInterestAccrued {
                subaccount,
                currency,
                amount_minor,
                quote_value_minor,
                ..
            } => {
                if *amount_minor > 0 {
                    if let Some(balances) = self.collateral.get_mut(subaccount) {
                        if let Some(balance) = balances.get_mut(currency) {
                            *balance = balance.saturating_sub(*amount_minor);
                        }
                    }
                    // Interest income routes like fee income: house,
                    // insurance, buyback.
                    if let Some(allocation) = self.revenue_router.route(*quote_value_minor) {
                        self.apply_revenue(allocation);
                    }
                }
            }
            Event::InsuranceMarked {
                symbol,
                signed_lots,
                mark_quote_minor,
                pnl_quote_minor,
                ..
            } => {
                if let Some((lots, last_mark)) = self.insurance_inventory.get_mut(symbol) {
                    let _ = lots;
                    *last_mark = *mark_quote_minor;
                }
                if *pnl_quote_minor != 0 {
                    self.insurance.apply_mark_pnl(*pnl_quote_minor, self.now);
                }
                let _ = signed_lots;
            }
            Event::InsuranceRebalanced {
                symbol,
                lots,
                proceeds_quote_minor,
                ..
            } => {
                if let Some((signed, _)) = self.insurance_inventory.get_mut(symbol) {
                    // Rebalancing only ever sells long inventory back:
                    // reduce the carried lots, floor at zero.
                    *signed = (*signed - i64::try_from(*lots).unwrap_or(0)).max(0);
                }
                if *proceeds_quote_minor > 0 {
                    self.insurance
                        .credit_penalty(*proceeds_quote_minor, self.now);
                }
            }
            Event::VaultEpochSettled(epoch) => {
                self.apply_vault_epoch(epoch);
            }
            Event::VaultOpened {
                vault_id,
                revenue_share_bps,
                ..
            } => {
                self.next_vault_id = self.next_vault_id.max(vault_id + 1);
                self.vaults.insert(
                    *vault_id,
                    poc_economics::LpVault::new(*vault_id, *revenue_share_bps),
                );
            }
            Event::VaultQueued {
                vault_id,
                subaccount,
                is_subscribe,
                amount,
                ..
            } => {
                if let Some(vault) = self.vaults.get_mut(vault_id) {
                    if *is_subscribe {
                        vault.subscribe(*subaccount, *amount);
                    } else {
                        vault.redeem(*subaccount, *amount);
                    }
                }
            }
        }
    }

    /// Route an interest charge's quote value through the revenue router
    /// and apply the allocations.
    fn apply_revenue(&mut self, allocation: poc_economics::Allocation) {
        if allocation.insurance > 0 {
            self.credit_insurance_allocation(allocation.insurance, self.now);
        }
        self.buyback_pool_quote_minor = self
            .buyback_pool_quote_minor
            .saturating_add(allocation.buyback);
        // The house share stays outside engine balances (the venue's
        // own account ledger is out of scope for the reference engine).
        let _ = allocation.house;
    }

    /// Route a gross fee income through the revenue router and apply
    /// the allocations: insurance share first through the vault
    /// revenue split (G-16), then the coverage policy, then the buyback
    /// pool — one path for trade fees, RFQ fees, block fees, and
    /// interest charges alike.
    pub(crate) fn apply_fee_income(&mut self, gross: u128, ts: TimestampMs) {
        if gross == 0 {
            return;
        }
        if let Some(allocation) = self.revenue_router.route(gross) {
            self.credit_insurance_allocation(allocation.insurance, ts);
            if allocation.buyback > 0 {
                self.buyback_pool_quote_minor = self
                    .buyback_pool_quote_minor
                    .saturating_add(allocation.buyback);
            }
            // The house share stays outside engine balances.
            let _ = allocation.house;
        }
    }

    /// Credit the insurance allocation: LP vaults take their
    /// configured share of the routed insurance revenue first (G-16,
    /// deterministic ascending vault-id order, each vault's share
    /// floored from the *full* allocation and capped by what remains,
    /// so the split can never exceed the allocation); the remainder
    /// lands in the fund — or, when the fund is above its coverage
    /// target, overflows to the buyback pool (G-40).
    ///
    /// Vault shares are an LP yield, not a fund top-up: they apply
    /// even when the fund is above target, because depositors earn
    /// their backstop premium in every regime.
    pub(crate) fn credit_insurance_allocation(&mut self, amount: u128, ts: TimestampMs) {
        if amount == 0 {
            return;
        }
        // Snapshot the weights first (immutable), then mutate.
        let weights: Vec<(u64, u64)> = self
            .vaults
            .values()
            .filter(|v| v.revenue_share_bps > 0 && v.collateral_quote_minor > 0)
            .map(|v| (v.vault_id, v.revenue_share_bps))
            .collect();
        let mut remaining = amount;
        for (vault_id, share_bps) in weights {
            if remaining == 0 {
                break;
            }
            let credit = mul_div(
                amount,
                u128::from(share_bps),
                10_000,
                poc_core::Rounding::Floor,
            )
            .unwrap_or(0)
            .min(remaining);
            if credit > 0 {
                if let Some(vault) = self.vaults.get_mut(&vault_id) {
                    vault.credit_revenue(credit);
                }
                remaining -= credit;
            }
        }
        if remaining > 0 {
            // G-40: an insurance fund above its coverage target stops
            // hoarding — the share overflows to the buyback pool (the
            // Hyperliquid-validated pattern).
            let coverage = self.insurance_coverage_permille();
            if self.config.coverage_policy.above_target(coverage) {
                self.buyback_pool_quote_minor =
                    self.buyback_pool_quote_minor.saturating_add(remaining);
            } else {
                self.insurance.credit_penalty(remaining, ts);
            }
        }
    }

    /// Apply one leg of an RFQ settlement (no book interaction).
    fn apply_rfq_trade(&mut self, trade: &Trade, taker: SubaccountId) {
        let Some(instrument) = self.instruments.get(&trade.symbol).cloned() else {
            return;
        };
        let price = instrument.price_quote_minor(trade.price_ticks).unwrap_or(0);
        let taker_side = trade.maker_side.opposite();
        if let Some(maker) = self.accounts.get_mut(&trade.maker_subaccount) {
            maker.apply_fill(
                &instrument,
                &trade.symbol,
                trade.maker_side,
                trade.qty_lots,
                price,
            );
        }
        if let Some(taker_acct) = self.accounts.get_mut(&taker) {
            taker_acct.apply_fill(
                &instrument,
                &trade.symbol,
                taker_side,
                trade.qty_lots,
                price,
            );
            taker_acct.apply_fee(trade.taker_fee_quote_minor);
        }
        self.fee_schedule
            .record_volume_at(taker, trade.notional_quote_minor, trade.ts);
        let gross = trade.taker_fee_quote_minor.max(0);
        if gross > 0 {
            self.apply_fee_income(gross.unsigned_abs(), trade.ts);
        }
        self.seq = self.seq.max(trade.seq + 1);
        self.trades += 1;
        self.lots_traded += trade.qty_lots;
        self.notional_traded = self
            .notional_traded
            .saturating_add(trade.notional_quote_minor);
    }

    /// Insurance coverage (permille of the aggregate maintenance
    /// requirement across margined accounts with live marks).
    #[must_use]
    pub fn insurance_coverage_permille(&self) -> u64 {
        let Some(marks) = self.build_marks(self.now) else {
            return u64::MAX;
        };
        let mut aggregate: u128 = 0;
        for account in self.accounts.values() {
            if let Some(summary) =
                self.margin_engine
                    .margin_summary(account, &self.instruments, &marks)
            {
                aggregate = aggregate.saturating_add(summary.maintenance_quote_minor);
            }
        }
        if aggregate == 0 {
            return u64::MAX;
        }
        let balance = self.insurance.balance().max(0).unsigned_abs();
        u64::try_from(
            poc_core::mul_div(balance, 1_000, aggregate, poc_core::Rounding::Floor).unwrap_or(0),
        )
        .unwrap_or(u64::MAX)
    }

    fn apply_trade(&mut self, trade: &Trade) {
        let Some(instrument) = self.instruments.get(&trade.symbol).cloned() else {
            return;
        };
        let price = instrument.price_quote_minor(trade.price_ticks).unwrap_or(0);

        // Book: reduce the maker (open-before for reservation scaling).
        let maker_open_before = self
            .books
            .get(&trade.symbol)
            .and_then(|b| b.get(trade.maker_order_id))
            .map(|r| r.order.open_qty());
        if let Some(book) = self.books.get_mut(&trade.symbol) {
            let fill = poc_orderbook::Fill {
                taker_order_id: trade.taker_order_id,
                maker_order_id: trade.maker_order_id,
                taker_subaccount: trade.taker_subaccount,
                maker_subaccount: trade.maker_subaccount,
                maker_side: trade.maker_side,
                price_ticks: trade.price_ticks,
                qty_lots: trade.qty_lots,
            };
            book.apply_fill(&fill);
        }

        // Accounts: positions, cash, fees.
        if let Some(maker) = self.accounts.get_mut(&trade.maker_subaccount) {
            maker.apply_fill(
                &instrument,
                &trade.symbol,
                trade.maker_side,
                trade.qty_lots,
                price,
            );
            maker.apply_fee(trade.maker_fee_quote_minor);
        }
        if let Some(taker) = self.accounts.get_mut(&trade.taker_subaccount) {
            let taker_side = trade.maker_side.opposite();
            taker.apply_fill(
                &instrument,
                &trade.symbol,
                taker_side,
                trade.qty_lots,
                price,
            );
            taker.apply_fee(trade.taker_fee_quote_minor);
        }

        // Volume accounting for fee tiers (exact day-bucketed window).
        self.fee_schedule.record_volume_at(
            trade.maker_subaccount,
            trade.notional_quote_minor,
            trade.ts,
        );
        self.fee_schedule.record_volume_at(
            trade.taker_subaccount,
            trade.notional_quote_minor,
            trade.ts,
        );

        // MMP: fills accumulate against each side's protection windows.
        if let Some(instrument) = self.instruments.get(&trade.symbol) {
            let base = instrument.base_symbol().to_owned();
            let taker_side = trade.maker_side.opposite();
            for (sub, delta) in [
                (
                    trade.taker_subaccount,
                    taker_side.sign() * trade.qty_lots as i64,
                ),
                (
                    trade.maker_subaccount,
                    trade.maker_side.sign() * trade.qty_lots as i64,
                ),
            ] {
                if let Some(mmp) = self.mmp.get_mut(&(sub, base.clone())) {
                    mmp.record_fill(trade.ts, trade.qty_lots, delta, trade.ts);
                }
            }
        }

        // Revenue routing: positive fee income only; rebates are a house
        // cost that nets out of the routed gross.
        let gross = trade
            .taker_fee_quote_minor
            .max(0)
            .saturating_add(trade.maker_fee_quote_minor.max(0));
        if gross > 0 {
            // One fee-income path: vault split + coverage policy +
            // buyback (G-16/G-40).
            self.apply_fee_income(gross.unsigned_abs(), trade.ts);
        }

        // Scale the maker's order-margin reservation to its remaining size.
        if let (Some(before), Some(&reserved)) = (
            maker_open_before,
            self.reservations.get(&trade.maker_order_id),
        ) {
            if before > 0 && reserved > 0 {
                let remaining = before.saturating_sub(trade.qty_lots);
                let scaled = mul_div(
                    reserved,
                    u128::from(remaining),
                    u128::from(before),
                    poc_core::Rounding::Floor,
                )
                .unwrap_or(0);
                let delta = reserved.saturating_sub(scaled);
                if let Some(account) = self.accounts.get_mut(&trade.maker_subaccount) {
                    account.order_margin_quote_minor =
                        account.order_margin_quote_minor.saturating_sub(delta);
                }
                if remaining == 0 {
                    self.reservations.remove(&trade.maker_order_id);
                } else {
                    self.reservations.insert(trade.maker_order_id, scaled);
                }
            }
        }

        // Mark tracking for the funding premium.
        self.last_trade_price.insert(trade.symbol.clone(), price);
        let mid = self.perp_mark_for_funding(&trade.symbol).unwrap_or(price);
        self.push_mark_sample(&trade.symbol, trade.ts, mid);

        self.seq = self.seq.max(trade.seq + 1);
        self.trades += 1;
        self.lots_traded += trade.qty_lots;
        self.notional_traded = self
            .notional_traded
            .saturating_add(trade.notional_quote_minor);
    }

    fn apply_liquidation(&mut self, exec: &crate::event::LiquidationExecuted) {
        let Some(instrument) = self.instruments.get(&exec.symbol).cloned() else {
            return;
        };
        let closing_side = if exec.closing_side_is_ask {
            Side::Ask
        } else {
            Side::Bid
        };
        if let Some(account) = self.accounts.get_mut(&exec.subaccount) {
            account.apply_fill(
                &instrument,
                &exec.symbol,
                closing_side,
                exec.lots,
                exec.price_quote_minor,
            );
            // Liquidation takes over risk management on this instrument:
            // pull the account's resting orders there.
            let ids: Vec<OrderId> = account
                .open_orders
                .values()
                .filter(|o| o.symbol == exec.symbol)
                .map(|o| o.order_id)
                .collect();
            for id in ids {
                account.untrack_order(id);
                let released = self.reservations.remove(&id).unwrap_or(0);
                account.order_margin_quote_minor =
                    account.order_margin_quote_minor.saturating_sub(released);
                if let Some(book) = self.books.get_mut(&exec.symbol) {
                    book.cancel(id);
                }
            }
        }
        if exec.to_insurance && exec.penalty_quote_minor > 0 {
            self.insurance
                .credit_penalty(exec.penalty_quote_minor, self.now);
        }
        // G-23: as buyer of last resort the fund *carries* the position.
        // Book it into inventory at the execution price; the sweep marks
        // it and drips it back when the book pays an edge.
        if exec.to_insurance {
            let signed = if exec.closing_side_is_ask {
                i64::try_from(exec.lots).unwrap_or(i64::MAX)
            } else {
                -i64::try_from(exec.lots).unwrap_or(i64::MAX)
            };
            let entry = self
                .insurance_inventory
                .entry(exec.symbol.clone())
                .or_insert((0, exec.price_quote_minor));
            entry.0 = entry.0.saturating_add(signed);
            entry.1 = exec.price_quote_minor;
        }
        if exec.absorbed_quote_minor > 0 {
            if let Some(account) = self.accounts.get_mut(&exec.subaccount) {
                account.cash_quote_minor = account
                    .cash_quote_minor
                    .saturating_add(to_i128(exec.absorbed_quote_minor));
            }
            self.insurance.absorb(exec.absorbed_quote_minor, self.now);
        }
        self.liquidations += 1;
    }

    fn apply_adl(&mut self, exec: &crate::event::AdlExecuted) {
        let Some(instrument) = self.instruments.get(&exec.symbol).cloned() else {
            return;
        };
        let closing_side = if exec.closing_side_is_ask {
            Side::Ask
        } else {
            Side::Bid
        };
        if let Some(account) = self.accounts.get_mut(&exec.liquidated_subaccount) {
            account.apply_fill(
                &instrument,
                &exec.symbol,
                closing_side,
                exec.lots,
                exec.price_quote_minor,
            );
        }
        if let Some(counterparty) = self.accounts.get_mut(&exec.counterparty_subaccount) {
            counterparty.apply_fill(
                &instrument,
                &exec.symbol,
                closing_side.opposite(),
                exec.lots,
                exec.price_quote_minor,
            );
        }
        self.adls += 1;
    }

    // ------------------------------------------------------------------
    // Marks
    // ------------------------------------------------------------------

    /// Apply an American early-exercise settlement: the long closes at
    /// TWAP intrinsic (realized against entry, exactly like expiry),
    /// pays the fee through the revenue router, and each assigned short
    /// closes its assigned lots at the same per-base intrinsic.
    fn apply_exercise(&mut self, ex: &crate::event::OptionExercised) {
        let Some(Instrument::Option(market)) = self.instruments.get(&ex.symbol).cloned() else {
            // Market gone (delisted between plan and apply): consume the
            // request without touching accounts.
            self.exercises.remove(&ex.request_id);
            return;
        };
        let instrument = Instrument::Option(market.clone());

        // Long side: close settled lots at per-base intrinsic.
        if ex.settled_lots > 0 {
            let per_base_intrinsic = ex
                .settlement_quote_minor
                .saturating_sub(market.strike_quote_minor);
            let per_base_intrinsic = match market.kind {
                poc_core::OptionKind::Call => per_base_intrinsic,
                poc_core::OptionKind::Put => market
                    .strike_quote_minor
                    .saturating_sub(ex.settlement_quote_minor),
            };
            if let Some(account) = self.accounts.get_mut(&ex.subaccount) {
                account.apply_fill(
                    &instrument,
                    &ex.symbol,
                    Side::Ask, // closing a long
                    ex.settled_lots,
                    per_base_intrinsic,
                );
            }
            self.options_settled += 1;
        }
        // Exercise fee: charged to the long, routed to venue revenue.
        if ex.exercise_fee_quote_minor > 0 {
            if let Some(account) = self.accounts.get_mut(&ex.subaccount) {
                account.apply_fee(i128::try_from(ex.exercise_fee_quote_minor).unwrap_or(i128::MAX));
            }
            if let Some(allocation) = self.revenue_router.route(ex.exercise_fee_quote_minor) {
                if allocation.insurance > 0 {
                    let coverage = self.insurance_coverage_permille();
                    if self.config.coverage_policy.above_target(coverage) {
                        self.buyback_pool_quote_minor = self
                            .buyback_pool_quote_minor
                            .saturating_add(allocation.insurance);
                    } else {
                        self.insurance
                            .credit_penalty(allocation.insurance, self.now);
                    }
                }
                self.buyback_pool_quote_minor = self
                    .buyback_pool_quote_minor
                    .saturating_add(allocation.buyback);
            }
        }
        // Short side: close assigned lots at per-base intrinsic.
        for a in &ex.assignments {
            if a.lots == 0 {
                continue;
            }
            let per_base_intrinsic = ex
                .settlement_quote_minor
                .saturating_sub(market.strike_quote_minor);
            let per_base_intrinsic = match market.kind {
                poc_core::OptionKind::Call => per_base_intrinsic,
                poc_core::OptionKind::Put => market
                    .strike_quote_minor
                    .saturating_sub(ex.settlement_quote_minor),
            };
            if let Some(account) = self.accounts.get_mut(&a.subaccount) {
                account.apply_fill(
                    &instrument,
                    &ex.symbol,
                    Side::Bid, // closing a short
                    a.lots,
                    per_base_intrinsic,
                );
            }
        }
        self.exercises.remove(&ex.request_id);
        self.exercises_settled += 1;
    }

    /// Build the mark set for every underlying with a live oracle.
    ///
    /// Marks are oracle-anchored (see crate docs): perps mark at spot,
    /// options at Black-Scholes with configured IV. Books never move marks.
    #[must_use]
    pub fn build_marks(&self, now: TimestampMs) -> Option<BTreeMap<String, MarkSet>> {
        let mut out = BTreeMap::new();
        for (base, oracle) in &self.oracles {
            let Some(spot) = oracle.mark(now) else {
                continue;
            };
            let mut set = MarkSet::new(base.clone(), spot);
            for (symbol, instrument) in &self.instruments {
                if instrument.base_symbol() != base {
                    continue;
                }
                let mark = match instrument {
                    Instrument::Perp(_) => Mark::Perp {
                        mark_quote_minor_per_base: spot,
                    },
                    Instrument::Option(m) => {
                        // Everlasting options mark at the roll's effective
                        // maturity (tte_ms handles the variant); IV comes
                        // from the governed surface with the configured
                        // anchor as fail-safe.
                        let tau = tau_years(u64::try_from(m.tte_ms(now)).unwrap_or(0));
                        let anchor = self.config.option_ivs.get(symbol).copied().unwrap_or(0.55);
                        let iv = self
                            .vol_surface
                            .mark_iv_bps(symbol)
                            .map_or(anchor, |bps| bps as f64 / 10_000.0);
                        let flavour = match m.kind {
                            poc_core::OptionKind::Call => poc_margin::Flavour::Call,
                            poc_core::OptionKind::Put => poc_margin::Flavour::Put,
                        };
                        let rate = self.config.risk_free_rate;
                        let premium = if m.is_american() {
                            // American: the early-exercise premium is
                            // priced in (BAW); at the venue's zero-rate
                            // convention this equals the European mark
                            // exactly (migration-safe default).
                            poc_margin::AmericanAnalytics::baw_price(
                                flavour,
                                spot as f64,
                                m.strike_quote_minor as f64,
                                tau,
                                iv,
                                rate,
                                rate, // b = r: non-dividend carry
                            )
                        } else {
                            poc_margin::OptionAnalytics::price(
                                flavour,
                                spot as f64,
                                m.strike_quote_minor as f64,
                                tau,
                                iv,
                                rate,
                            )
                        };
                        Mark::Option {
                            premium_quote_minor_per_base: half_up_u128(premium),
                            iv,
                            tau_years: tau,
                        }
                    }
                };
                set.marks.insert(symbol.clone(), mark);
            }
            out.insert(base.clone(), set);
        }
        Some(out)
    }

    /// Mark price of one instrument (quote minor per base).
    pub(crate) fn instrument_mark(
        &self,
        instrument: &Instrument,
        marks: &BTreeMap<String, MarkSet>,
    ) -> Option<u128> {
        let set = marks.get(instrument.base_symbol())?;
        match instrument {
            Instrument::Perp(_) => Some(set.spot_quote_minor_per_base),
            Instrument::Option(m) => match set.marks.get(&m.symbol)? {
                Mark::Option {
                    premium_quote_minor_per_base,
                    ..
                } => Some(*premium_quote_minor_per_base),
                Mark::Perp { .. } => None,
            },
        }
    }

    /// The funding mark of a perp: the impact-mid when the book has
    /// depth, else BBO mid, else last trade, else the oracle spot.
    ///
    /// G-39: the impact mid comes from walking a fixed impact notional
    /// into both sides (the Derive INA / BitMEX impact-price pattern) — a
    /// thin top-of-book cannot move the funding mark, and depth beyond
    /// the touch participates in premium discovery.
    fn perp_mark_for_funding(&self, symbol: &str) -> Option<u128> {
        let book = self.books.get(symbol)?;
        let instrument = self.instruments.get(symbol)?;
        if let Some((ibp, iap)) = self.impact_prices(symbol) {
            return Some((ibp + iap) / 2);
        }
        if let (Some(b), Some(a)) = book.bbo() {
            let mid = (b + a) / 2;
            return instrument.price_quote_minor(mid);
        }
        self.last_trade_price.get(symbol).copied()
    }

    /// Impact bid/ask prices (G-39): volume-weighted prices paid to
    /// absorb `impact_notional_quote_minor` of flow into each side.
    /// Falls back to the best touch when a side's visible depth cannot
    /// cover the notional (the Derive convention of dropping thin sides,
    /// resolved as "use the touch" — never a zero price).
    fn impact_prices(&self, symbol: &str) -> Option<(u128, u128)> {
        if self.config.impact_notional_quote_minor == 0 {
            return None;
        }
        let book = self.books.get(symbol)?;
        let instrument = self.instruments.get(symbol)?;
        let (bid_levels, ask_levels) = book.depth(64);
        let price_of = |ticks: u64| instrument.price_quote_minor(ticks);
        let value_per_lot = |price: u128| -> Option<u128> {
            let base_unit = 10_u128.checked_pow(instrument.base_decimals())?;
            mul_div(
                price,
                instrument.lot_size(),
                base_unit,
                poc_core::Rounding::Floor,
            )
        };
        let walk = |levels: &[poc_orderbook::LevelSnapshot]| -> Option<u128> {
            let mut spent = 0_u128;
            let mut cost = 0_u128;
            let mut lots = 0_u128;
            for level in levels {
                let Some(price) = price_of(level.price_ticks) else {
                    continue;
                };
                let Some(vpl) = value_per_lot(price) else {
                    continue;
                };
                for _ in 0..level.total_qty_lots {
                    if spent >= self.config.impact_notional_quote_minor {
                        break;
                    }
                    spent = spent.saturating_add(vpl);
                    cost = cost.saturating_add(price);
                    lots += 1;
                }
                if spent >= self.config.impact_notional_quote_minor {
                    break;
                }
            }
            if lots > 0 && spent >= self.config.impact_notional_quote_minor {
                Some(cost / lots)
            } else {
                // Insufficient depth: the best touch stands in (documented
                // fail-safe — never a zero price, never a stale mid).
                levels.first().and_then(|l| price_of(l.price_ticks))
            }
        };
        let ibp = walk(&bid_levels);
        let iap = walk(&ask_levels);
        match (ibp, iap) {
            (Some(b), Some(a)) if b > 0 && a > 0 => Some((b, a)),
            _ => None,
        }
    }

    fn push_mark_sample(&mut self, symbol: &str, ts: TimestampMs, price: u128) {
        let samples = self.mark_samples.entry(symbol.to_owned()).or_default();
        if let Some(back) = samples.back_mut() {
            if back.0 == ts {
                back.1 = price;
                return;
            }
            if back.0 > ts {
                return; // monotonic only
            }
        }
        samples.push_back((ts, price));
        while samples.len() > 100_000 {
            samples.pop_front();
        }
    }

    /// Step-function TWAP of a mark sample series over `[now - window, now]`
    /// (same semantics as the oracle TWAP: first sample extends backward).
    #[must_use]
    pub fn mark_twap(
        &self,
        symbol: &str,
        now: TimestampMs,
        window_ms: TimestampMs,
    ) -> Option<u128> {
        let samples = self.mark_samples.get(symbol)?;
        if samples.is_empty() || window_ms == 0 {
            return None;
        }
        let start = now.saturating_sub(window_ms);
        let mut weighted: u128 = 0;
        let mut total: u128 = 0;
        let mut iter = samples.iter().peekable();
        while let Some(&(t, p)) = iter.next() {
            let next_t = iter.peek().map_or(now, |&&(nt, _)| nt.min(now));
            let seg_end = next_t.max(t);
            let (a, b) = (t.max(start), seg_end.max(start));
            if b > a {
                weighted = weighted.saturating_add(p.saturating_mul(u128::from(b - a)));
                total = total.saturating_add(u128::from(b - a));
            }
            if t > now {
                break;
            }
        }
        if let Some(&(first_ts, first_p)) = samples.front() {
            if first_ts > start {
                let extend = u128::from(first_ts - start);
                weighted = weighted.saturating_add(first_p.saturating_mul(extend));
                total = total.saturating_add(extend);
            }
        }
        if total == 0 {
            return None;
        }
        Some(weighted / total)
    }

    fn perps_on_base(&self, base: &str) -> Vec<Symbol> {
        self.instruments
            .iter()
            .filter(|(_, inst)| inst.base_symbol() == base && matches!(inst, Instrument::Perp(_)))
            .map(|(sym, _)| sym.clone())
            .collect()
    }

    // ------------------------------------------------------------------
    // Views and stats
    // ------------------------------------------------------------------

    /// Margin summary of one account (requires live marks), with
    /// haircut-adjusted collateral equity included (G-17).
    #[must_use]
    pub fn margin_summary_of(&self, subaccount: SubaccountId) -> Option<MarginSummary> {
        let account = self.accounts.get(&subaccount)?;
        self.effective_margin_summary(account)
    }

    /// Health classification of one account.
    #[must_use]
    pub fn health_of(&self, subaccount: SubaccountId) -> Option<Health> {
        Some(Health::classify(&self.margin_summary_of(subaccount)?))
    }

    /// Full account view for API surfaces.
    #[must_use]
    pub fn account_view(&self, subaccount: SubaccountId) -> Option<crate::event::AccountView> {
        let account = self.accounts.get(&subaccount)?;
        let summary = self.margin_summary_of(subaccount)?;
        Some(crate::event::AccountView {
            id: subaccount,
            summary,
            cash_quote_minor: account.cash_quote_minor,
            positions: account
                .positions
                .iter()
                .map(|(s, p)| (s.clone(), p.signed_lots, p.avg_entry_quote_minor))
                .collect(),
            fees_paid_quote_minor: account.fees_paid_quote_minor,
            funding_received_quote_minor: -account.funding_pnl_quote_minor,
            open_order_ids: account.open_orders.keys().copied().collect(),
        })
    }

    /// One instrument's book view.
    #[must_use]
    pub fn book_view(&self, symbol: &str) -> Option<BookView> {
        let instrument = self.instruments.get(symbol)?;
        let book = self.books.get(symbol)?;
        let (bid, ask) = book.bbo();
        Some(BookView {
            symbol: symbol.to_owned(),
            best_bid_ticks: bid,
            best_ask_ticks: ask,
            open_orders: book.open_order_count(),
            halted: self
                .halted
                .get(instrument.base_symbol())
                .copied()
                .unwrap_or(false),
            kind: match instrument {
                Instrument::Perp(_) => InstrumentKindView::Perp,
                Instrument::Option(_) => InstrumentKindView::Option,
            },
        })
    }

    /// Aggregate market state snapshot.
    #[must_use]
    pub fn market_state(&self) -> MarketStateView {
        let books = self
            .instruments
            .keys()
            .filter_map(|symbol| self.book_view(symbol))
            .collect();
        let spots = self
            .oracles
            .iter()
            .map(|(base, oracle)| (base.clone(), oracle.mark(self.now)))
            .collect();
        MarketStateView {
            books,
            spots,
            insurance_balance: self.insurance.balance(),
        }
    }

    /// Engine statistics.
    #[must_use]
    pub fn stats(&self) -> EngineStats {
        EngineStats {
            events: self.journal.len() as u64,
            trades: self.trades,
            lots_traded: self.lots_traded,
            notional_traded: self.notional_traded,
            revenue: self.revenue_router.cumulative(),
            insurance_balance: self.insurance.balance(),
            funding_intervals: self.funding_intervals,
            options_settled: self.options_settled,
            exercises_settled: self.exercises_settled,
            liquidations: self.liquidations,
            adls: self.adls,
        }
    }

    /// The insurance fund (read-only).
    #[must_use]
    pub fn insurance(&self) -> &InsuranceFund {
        &self.insurance
    }

    /// The RFQ state machine (read-only view for makers/takers).
    #[must_use]
    pub fn rfq_view(&self) -> &RfqBook {
        &self.rfq_book
    }

    /// The block-trade tape (read-only).
    #[must_use]
    pub fn blocks_view(&self) -> &BlockLedger {
        &self.blocks
    }

    /// The governed volatility surface (read-only).
    #[must_use]
    pub fn vol_surface_view(&self) -> &VolSurface {
        &self.vol_surface
    }

    /// The buyback overflow pool balance (G-40).
    #[must_use]
    pub fn buyback_pool(&self) -> u128 {
        self.buyback_pool_quote_minor
    }

    /// MMP state (read-only).
    #[must_use]
    pub fn mmp_state(&self, subaccount: SubaccountId, base: &str) -> Option<&MmpState> {
        self.mmp.get(&(subaccount, base.to_owned()))
    }

    /// Whether an instrument is breaker-blocked at `now` (G-21).
    #[must_use]
    pub fn is_breaker_blocked(&self, symbol: &str, now: TimestampMs) -> bool {
        self.price_breaker_until
            .get(symbol)
            .is_some_and(|until| *until > now)
    }

    /// Pre-trade portfolio greeks check (G-41): the account's current
    /// vega/gamma plus this order's worst-case increment (a full fill
    /// at the current mark), against the configured caps.
    fn check_greeks_limit(
        &self,
        request: &OrderRequest,
        market: &poc_core::OptionMarket,
    ) -> Option<Rejection> {
        let marks = self.build_marks(self.now)?;
        let set = marks.get(&market.base_symbol)?;
        let spot = set.spot_quote_minor_per_base as f64;
        let strike = market.strike_quote_minor as f64;
        let rate = self.config.risk_free_rate;
        let lot_base = market.lot_size_base_minor as f64 / 10_f64.powi(market.base_decimals as i32);
        let signed_qty = request.side.sign() as f64 * request.qty_lots as f64;

        // Portfolio totals over the account's option positions.
        let mut vega_now = 0_i128;
        let mut gamma_now = 0_i128;
        let account = self.accounts.get(&request.subaccount)?;
        for (symbol, position) in &account.positions {
            let Some(inst) = self.instruments.get(symbol) else {
                continue;
            };
            let Instrument::Option(m) = inst else {
                continue;
            };
            let Some(set) = marks.get(&m.base_symbol) else {
                continue;
            };
            let Some(poc_margin::Mark::Option {
                iv, tau_years: tau, ..
            }) = set.marks.get(symbol)
            else {
                continue;
            };
            let spot = set.spot_quote_minor_per_base as f64;
            let strike = m.strike_quote_minor as f64;
            let lot_base = m.lot_size_base_minor as f64 / 10_f64.powi(m.base_decimals as i32);
            let qty = position.signed_lots as f64;
            let vega =
                (poc_margin::OptionAnalytics::vega(spot, strike, *tau, *iv, rate) * qty * lot_base)
                    .round()
                    .clamp(-1e15, 1e15) as i128;
            let gamma = (poc_margin::OptionAnalytics::gamma(spot, strike, *tau, *iv, rate)
                * qty
                * lot_base)
                .round()
                .clamp(-1e15, 1e15) as i128;
            vega_now = vega_now.saturating_add(vega);
            gamma_now = gamma_now.saturating_add(gamma);
        }

        // Gamma increment of the new order (same mark inputs).
        let Some(poc_margin::Mark::Option { iv, tau_years, .. }) = set.marks.get(&market.symbol)
        else {
            return None;
        };
        let (iv, tau) = (*iv, *tau_years);
        let vega_inc = (poc_margin::OptionAnalytics::vega(spot, strike, tau, iv, rate)
            * signed_qty
            * lot_base)
            .round()
            .clamp(-1e15, 1e15) as i128;
        let gamma_inc = (poc_margin::OptionAnalytics::gamma(spot, strike, tau, iv, rate)
            * signed_qty
            * lot_base)
            .round()
            .clamp(-1e15, 1e15) as i128;

        match self
            .config
            .greeks_limits
            .check(vega_now, gamma_now, vega_inc, gamma_inc)
        {
            Ok(()) => None,
            Err(poc_risk::GreeksRejection::VegaCap { would_be, cap }) => {
                Some(Rejection::GreeksLimitExceeded {
                    what: "vega",
                    would_be,
                    cap,
                })
            }
            Err(poc_risk::GreeksRejection::GammaCap { would_be, cap }) => {
                Some(Rejection::GreeksLimitExceeded {
                    what: "gamma",
                    would_be,
                    cap,
                })
            }
        }
    }

    /// Position Greeks at the current marks (G-28: the desk's eye on risk).
    ///
    /// Delta and vega per position, account-aggregated per instrument, from
    /// the same Black-76 engine the margin scenarios use. Everlasting
    /// theta is the roll: reported as the interval's mark premium per lot.
    #[must_use]
    pub fn greeks_view(&self, subaccount: SubaccountId) -> Option<Vec<(Symbol, i128, i128)>> {
        let account = self.accounts.get(&subaccount)?;
        let marks = self.build_marks(self.now)?;
        let mut out = Vec::new();
        for (symbol, position) in &account.positions {
            if position.signed_lots == 0 {
                continue;
            }
            let Some(instrument) = self.instruments.get(symbol) else {
                continue;
            };
            let Instrument::Option(m) = instrument else {
                continue;
            };
            let Some(set) = marks.get(instrument.base_symbol()) else {
                continue;
            };
            let Some(poc_margin::Mark::Option {
                iv, tau_years: tau, ..
            }) = set.marks.get(symbol)
            else {
                continue;
            };
            let flavour = match m.kind {
                poc_core::OptionKind::Call => poc_margin::Flavour::Call,
                poc_core::OptionKind::Put => poc_margin::Flavour::Put,
            };
            let spot = set.spot_quote_minor_per_base as f64;
            let strike = m.strike_quote_minor as f64;
            let rate = self.config.risk_free_rate;
            let delta = poc_margin::OptionAnalytics::delta(flavour, spot, strike, *tau, *iv, rate);
            let vega = poc_margin::OptionAnalytics::vega(spot, strike, *tau, *iv, rate);
            // Per-base greeks scaled into lots of the contract.
            let lot_base = m.lot_size_base_minor as f64 / 10_f64.powi(m.base_decimals as i32);
            let delta_lots = delta * position.signed_lots as f64 * lot_base;
            let vega_lots = vega * position.signed_lots as f64 * lot_base;
            out.push((
                symbol.clone(),
                delta_lots.round().clamp(-1e15, 1e15) as i128,
                vega_lots.round().clamp(-1e15, 1e15) as i128,
            ));
        }
        Some(out)
    }

    /// The incentive scoreboard (read-only).
    #[must_use]
    pub fn incentive_scoreboard(&self) -> Vec<(SubaccountId, u128)> {
        self.incentives.scoreboard()
    }

    /// Replay a journal into a fresh engine built from `config`.
    #[must_use]
    pub fn replay(config: EngineConfig, journal: &[Event]) -> Self {
        let mut engine = Self::new(config);
        for event in journal {
            engine.apply_event(event);
        }
        engine.journal = journal.to_vec();
        engine
    }

    /// Read-only handles for the institutional planning module.
    #[must_use]
    pub(crate) fn rfq_book_ref(&self) -> &RfqBook {
        &self.rfq_book
    }
    #[must_use]
    pub(crate) fn fee_schedule_ref(&self) -> &FeeSchedule {
        &self.fee_schedule
    }
    #[must_use]
    pub(crate) fn now_ref(&self) -> TimestampMs {
        self.now
    }
    #[must_use]
    pub(crate) fn seq_ref(&self) -> u64 {
        self.seq
    }
    #[must_use]
    pub(crate) fn margin_engine_ref(&self) -> &poc_margin::PortfolioMarginEngine {
        &self.margin_engine
    }

    /// Fee estimator for the risk gate (worst-case taker fee).
    pub(crate) fn estimate_fee(&self, instrument: &Instrument, order: &Order, mark: u128) -> i128 {
        let price_ticks = order
            .price_ticks
            .unwrap_or_else(|| instrument.ticks_from_quote_minor(mark).unwrap_or(0));
        let notional = instrument
            .notional_quote_minor(price_ticks, order.open_qty())
            .unwrap_or(0);
        let tier = self.fee_schedule.tier_for(order.subaccount);
        FeeCalculator::worst_case_fee(tier, notional).unwrap_or(0)
    }
}

// ----------------------------------------------------------------------
// Private helpers
// ----------------------------------------------------------------------

fn event_ts(event: &Event) -> TimestampMs {
    match event {
        Event::Deposit { ts, .. }
        | Event::Withdrawal { ts, .. }
        | Event::WithdrawRejected { ts, .. }
        | Event::ProviderObserved { ts, .. }
        | Event::StpCancels { ts, .. }
        | Event::MarketHalted { ts, .. }
        | Event::MarketResumed { ts, .. } => *ts,
        Event::MarketListed { .. } => 0,
        Event::OrderResting { order, .. } => order.engine_ts,
        Event::OrderClosed { order, .. } => order.engine_ts,
        Event::OrderRejection(r) => r.request.client_ts,
        Event::TradeExecuted(t) => t.ts,
        Event::Funding(f) => f.ts,
        Event::ClockAdvanced { now } => *now,
        Event::FundingFlow(_)
        | Event::OptionExpiry(_)
        | Event::OptionExercised(_)
        | Event::ExerciseDeferred { .. }
        | Event::OptionDelisted { .. }
        | Event::LiquidityScored { .. }
        | Event::Reward(_)
        | Event::RewardsSettled
        | Event::Liquidation(_)
        | Event::Adl(_) => 0,
        Event::ExerciseQueued { requested_at, .. } => *requested_at,
        Event::ExerciseRejected { ts, .. } => *ts,
        Event::RfqCreated { ts, .. }
        | Event::RfqQuoted { ts, .. }
        | Event::RfqRejected { ts, .. }
        | Event::TransferRejected { ts, .. }
        | Event::TransferExecuted { ts, .. }
        | Event::MmpTripped { ts, .. }
        | Event::SessionDisconnected { ts, .. }
        | Event::CodChanged { ts, .. }
        | Event::BreakerTripped { ts, .. }
        | Event::BreakerReleased { ts, .. } => *ts,
        Event::SurfaceSwept { now } => *now,
        Event::SurfaceObserved(obs) => obs.ts,
        Event::RfqSettled { trades, .. } => trades.first().map_or(0, |t| t.ts),
        Event::OrderAmended(a) => a.ts,
        Event::TrailingUpdated { ts, .. } => *ts,
        Event::AuctionOpened { ts, .. } => *ts,
        Event::AuctionUncrossed { ts, .. } => *ts,
        Event::CollateralMoved(m) => m.ts,
        Event::CollateralRejected { ts, .. } => *ts,
        Event::CollateralConversion(c) => c.ts,
        Event::PositionMigrated { ts, .. } => *ts,
        Event::OcoLinked { ts, .. } => *ts,
        Event::TwapOpened(p) => p.opened_ts,
        Event::TwapSliced { ts, .. } => *ts,
        Event::TwapClosed { ts, .. } => *ts,
        Event::VolIndexPublished { ts, .. } => *ts,
        Event::CollateralInterestAccrued { ts, .. } => *ts,
        Event::InsuranceMarked { ts, .. } => *ts,
        Event::InsuranceRebalanced { ts, .. } => *ts,
        Event::VaultEpochSettled(v) => v.ts,
        Event::VaultOpened { ts, .. } | Event::VaultQueued { ts, .. } => *ts,
        Event::MmEnrolled { ts, .. }
        | Event::MmTierAdjusted { ts, .. }
        | Event::QuoteInterestAccrued { ts, .. } => *ts,
        Event::RfqClosed { .. }
        | Event::BlockRegistered { .. }
        | Event::BlockPrinted { .. }
        | Event::MmpConfigured { .. } => 0,
    }
}

fn funding_interval_of(instruments: &BTreeMap<Symbol, Instrument>, symbol: &str) -> TimestampMs {
    instruments.get(symbol).map_or(8 * 3_600_000, |i| match i {
        Instrument::Perp(m) => m.funding.interval_ms,
        Instrument::Option(m) => m.roll_interval_ms().max(1),
    })
}

/// Milliseconds → years for analytics.
fn tau_years(ms: TimestampMs) -> f64 {
    (ms as f64) / (365.0 * 24.0 * 3_600_000.0)
}

/// f64 → u128 half-up (premium marks).
fn half_up_u128(x: f64) -> u128 {
    if x <= 0.0 {
        return 0;
    }
    let r = (x + 0.5).floor();
    if r >= u128::MAX as f64 {
        u128::MAX
    } else {
        r as u128
    }
}
