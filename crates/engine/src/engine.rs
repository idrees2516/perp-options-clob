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
use poc_risk::{InsuranceFund, LiquidationPlanner, OrderRiskContext, Rejection, RiskLimits};

use crate::command::{Command, OrderRequest};
use crate::event::{
    BookView, Event, InstrumentKindView, MarketStateView, OrderCloseReason, OrderRejected, Trade,
};

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
        }
    }
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

    // statistics
    pub(crate) trades: u64,
    pub(crate) lots_traded: u64,
    pub(crate) notional_traded: u128,
    pub(crate) funding_intervals: u64,
    pub(crate) options_settled: u64,
    pub(crate) liquidations: u64,
    pub(crate) adls: u64,
}

impl Engine {
    /// Build an engine from a configuration.
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
            trades: 0,
            lots_traded: 0,
            notional_traded: 0,
            funding_intervals: 0,
            options_settled: 0,
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
        let event = Event::MarketListed { instrument };
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

    /// The full journal since genesis.
    #[must_use]
    pub fn journal(&self) -> &[Event] {
        &self.journal
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
    pub fn process(&mut self, cmd: Command) -> Vec<Event> {
        let events = self.plan(&cmd);
        for event in &events {
            self.apply_event(event);
        }
        self.journal.extend(events.iter().cloned());
        events
    }

    fn plan(&self, cmd: &Command) -> Vec<Event> {
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
        }
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

    fn plan_place(&self, request: &OrderRequest, now: TimestampMs) -> Vec<Event> {
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

        let Some(book) = self.books.get(&request.symbol) else {
            return rejection(Rejection::UnknownInstrument);
        };

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
        };
        if let Err(reason) = poc_risk::check_order(&risk_ctx) {
            return rejection(reason);
        }

        // Stop orders park off-book until the mark crosses their trigger.
        if matches!(
            order.order_type,
            OrderType::StopMarket { .. } | OrderType::StopLimit { .. }
        ) {
            return vec![Event::OrderResting {
                order,
                margin_reserved_quote_minor: 0,
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
            let taker_fee = FeeCalculator::taker_fee(taker_tier, notional).unwrap_or(0);
            let maker_fee = FeeCalculator::maker_fee(maker_tier, notional).unwrap_or(0);
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

    fn plan_cancel(&self, subaccount: SubaccountId, order_id: OrderId) -> Vec<Event> {
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
            Event::MarketListed { instrument } => {
                let symbol = instrument.symbol().to_owned();
                let base = instrument.base_symbol().to_owned();
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
                if matches!(
                    order.order_type,
                    OrderType::StopMarket { .. } | OrderType::StopLimit { .. }
                ) {
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
                ..
            } => {
                self.stop_orders.remove(order_id);
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
                // The 30-day volume window slides one interval forward.
                self.fee_schedule.decay_volume(111);
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
        }
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

        // Volume accounting for fee tiers.
        self.fee_schedule
            .record_volume(trade.maker_subaccount, trade.notional_quote_minor);
        self.fee_schedule
            .record_volume(trade.taker_subaccount, trade.notional_quote_minor);

        // Revenue routing: positive fee income only; rebates are a house
        // cost that nets out of the routed gross.
        let gross = trade
            .taker_fee_quote_minor
            .max(0)
            .saturating_add(trade.maker_fee_quote_minor.max(0));
        if gross > 0 {
            if let Some(allocation) = self.revenue_router.route(gross.unsigned_abs()) {
                if allocation.insurance > 0 {
                    self.insurance
                        .credit_penalty(allocation.insurance, trade.ts);
                }
            }
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
                        let tau = tau_years(m.expiry_ts_ms.saturating_sub(now));
                        let iv = self.config.option_ivs.get(symbol).copied().unwrap_or(0.55);
                        let flavour = match m.kind {
                            poc_core::OptionKind::Call => poc_margin::Flavour::Call,
                            poc_core::OptionKind::Put => poc_margin::Flavour::Put,
                        };
                        let premium = poc_margin::OptionAnalytics::price(
                            flavour,
                            spot as f64,
                            m.strike_quote_minor as f64,
                            tau,
                            iv,
                            self.config.risk_free_rate,
                        );
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
    fn instrument_mark(
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

    /// The funding mark of a perp: BBO mid if two-sided, else last trade,
    /// else the oracle spot (fallback by callers).
    fn perp_mark_for_funding(&self, symbol: &str) -> Option<u128> {
        let book = self.books.get(symbol)?;
        let instrument = self.instruments.get(symbol)?;
        if let (Some(b), Some(a)) = book.bbo() {
            let mid = (b + a) / 2;
            return instrument.price_quote_minor(mid);
        }
        self.last_trade_price.get(symbol).copied()
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

    /// Margin summary of one account (requires live marks).
    #[must_use]
    pub fn margin_summary_of(&self, subaccount: SubaccountId) -> Option<MarginSummary> {
        let account = self.accounts.get(&subaccount)?;
        let marks = self.build_marks(self.now)?;
        self.margin_engine
            .margin_summary(account, &self.instruments, &marks)
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
            liquidations: self.liquidations,
            adls: self.adls,
        }
    }

    /// The insurance fund (read-only).
    #[must_use]
    pub fn insurance(&self) -> &InsuranceFund {
        &self.insurance
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

    /// Fee estimator for the risk gate (worst-case taker fee).
    fn estimate_fee(&self, instrument: &Instrument, order: &Order, mark: u128) -> i128 {
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
        | Event::OptionDelisted { .. }
        | Event::LiquidityScored { .. }
        | Event::Reward(_)
        | Event::RewardsSettled
        | Event::Liquidation(_)
        | Event::Adl(_) => 0,
    }
}

fn funding_interval_of(instruments: &BTreeMap<Symbol, Instrument>, symbol: &str) -> TimestampMs {
    instruments.get(symbol).map_or(8 * 3_600_000, |i| match i {
        Instrument::Perp(m) => m.funding.interval_ms,
        Instrument::Option(_) => 8 * 3_600_000,
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
