//! Institutional rails: RFQ execution, blocks, MMP, cancel-on-disconnect,
//! internal transfers — planned against the pre-command snapshot and
//! settled through the same journal every other state change flows through.
//!
//! Design lineage (see `docs/DESIGN_SOURCES.md`): the RFQ lifecycle is the
//! Derive V3 / Paradigm ONE shape — an unsigned intent, maker quotes that
//! are firm while they live, an atomic taker execution that cancels every
//! other live quote on the RFQ. MMP and cancel-on-disconnect are Derive's
//! market-maker protections: rolling-window fill limits that freeze a
//! currency, and a persisted per-account setting that pulls resting orders
//! when the connection drops.

use std::collections::BTreeMap;

use poc_core::{to_i128, Instrument, Side, SubaccountId, Symbol, TimestampMs};
use poc_margin::{MarginAccount, MarkSet};
use poc_rfq::LegFeeClass;

use crate::command::RfqLegCommand;
use crate::engine::Engine;
use crate::event::{Event, Trade};

/// MMP state for one (subaccount, underlying).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmpState {
    /// Rolling window length, ms.
    pub interval_ms: TimestampMs,
    /// Freeze duration, ms (0 = until reconfigured).
    pub frozen_time_ms: TimestampMs,
    /// Cumulative absolute fill size that trips the freeze, lots (0 = off).
    pub amount_limit_lots: u64,
    /// Cumulative absolute net delta that trips the freeze, lots (0 = off).
    pub delta_limit_lots: u64,
    /// (ts, |size| lots, signed delta lots) fill records inside the window.
    pub fills: Vec<(TimestampMs, u64, i64)>,
    /// Frozen-until timestamp (0 = not frozen; u64::MAX = until reset).
    pub frozen_until: TimestampMs,
}

impl MmpState {
    /// Whether new orders from this account in this currency are blocked.
    #[must_use]
    pub fn is_frozen(&self, now: TimestampMs) -> bool {
        self.frozen_until > now
    }

    /// Record a fill and evaluate the trip.
    ///
    /// Returns `true` when this fill trips the protection (the caller then
    /// cancels resting orders and freezes the account for the currency).
    /// The trip clears the window so the account starts a fresh count when
    /// the freeze lapses.
    pub fn record_fill(
        &mut self,
        ts: TimestampMs,
        size_lots: u64,
        delta_lots: i64,
        now: TimestampMs,
    ) -> bool {
        self.fills.push((ts, size_lots, delta_lots));
        self.prune(now);
        let amount: u64 = self
            .fills
            .iter()
            .fold(0_u64, |acc, (_, s, _)| acc.saturating_add(*s));
        let delta: i64 = self
            .fills
            .iter()
            .fold(0_i64, |acc, (_, _, d)| acc.saturating_add(*d));
        let amount_trips = self.amount_limit_lots > 0 && amount >= self.amount_limit_lots;
        let delta_trips =
            self.delta_limit_lots > 0 && delta.unsigned_abs() >= self.delta_limit_lots;
        if amount_trips || delta_trips {
            self.frozen_until = if self.frozen_time_ms == 0 {
                u64::MAX
            } else {
                now.saturating_add(self.frozen_time_ms)
            };
            self.fills.clear();
            return true;
        }
        false
    }

    fn prune(&mut self, now: TimestampMs) {
        if self.interval_ms == 0 {
            return;
        }
        let cutoff = now.saturating_sub(self.interval_ms);
        self.fills.retain(|(ts, _, _)| *ts > cutoff);
    }
}

/// Build `poc-rfq` legs from client commands, pulling the numeric spec from
/// the engine's instrument registry. Returns `None` when any leg references
/// an unknown instrument.
#[must_use]
pub fn build_rfq_legs(engine: &Engine, legs: &[RfqLegCommand]) -> Option<Vec<poc_rfq::RfqLeg>> {
    let mut out = Vec::with_capacity(legs.len());
    for leg in legs {
        let instrument = engine.instruments().get(&leg.symbol)?;
        let spec = poc_rfq::LegSpec {
            tick_size_quote_minor: instrument.tick_size(),
            lot_size_base_minor: instrument.lot_size(),
            base_decimals: instrument.base_decimals(),
        };
        out.push(poc_rfq::RfqLeg {
            symbol: leg.symbol.clone(),
            side: leg.side,
            qty_lots: leg.qty_lots,
            spec,
        });
    }
    Some(out)
}

/// A resolved, executable RFQ package (plan-stage view of an execution).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedExecution {
    /// The maker whose quote executes.
    pub maker: SubaccountId,
    /// (symbol, taker side, qty lots, price ticks) per leg.
    pub legs: Vec<(Symbol, Side, u64, u64)>,
}

/// Read-only resolution of (rfq, quote) into an executable package.
/// Mirrors `RfqBook::execute`'s validation without mutating anything —
/// the plan/apply discipline for the RFQ state machine too.
#[must_use = "the quote total is needed for the caller's accounting"]
pub fn resolve_execution(
    engine: &Engine,
    taker: SubaccountId,
    rfq_id: u64,
    quote_id: u64,
    now: TimestampMs,
) -> Result<PlannedExecution, &'static str> {
    let rfq = engine.rfq_book_ref().rfq(rfq_id).ok_or("unknown rfq")?;
    if rfq.taker != taker {
        return Err("not rfq owner");
    }
    if rfq.status != poc_rfq::RfqStatus::Open || now > rfq.valid_until {
        return Err("rfq not open");
    }
    let quote = engine
        .rfq_book_ref()
        .quote(quote_id)
        .ok_or("unknown quote")?;
    if quote.rfq_id != rfq_id {
        return Err("quote not on rfq");
    }
    if quote.status != poc_rfq::QuoteStatus::Live || now > quote.valid_until {
        return Err("quote not live");
    }
    if quote.leg_prices_ticks.len() != rfq.legs.len() {
        return Err("leg count mismatch");
    }
    let legs = rfq
        .legs
        .iter()
        .zip(&quote.leg_prices_ticks)
        .map(|(leg, px)| (leg.symbol.clone(), leg.side, leg.qty_lots, *px))
        .collect();
    Ok(PlannedExecution {
        maker: quote.maker,
        legs,
    })
}

/// Hypothetical post-trade margin check for an RFQ package: simulate both
/// counterparties' accounts with every leg applied and require each to
/// stay above maintenance. An RFQ that does not consume margin checks is
/// not a feature, it is a hole.
#[must_use]
pub(crate) fn rfq_margin_ok(
    engine: &Engine,
    taker: SubaccountId,
    maker: SubaccountId,
    legs: &[(Symbol, Side, u64, u64)],
    now: TimestampMs,
) -> Option<bool> {
    let marks = engine.build_marks(now)?;
    let mut sim_taker = engine.account(taker)?.clone();
    let mut sim_maker = engine.account(maker)?.clone();
    for (symbol, taker_side, qty, price_ticks) in legs {
        let instrument = engine.instruments().get(symbol)?.clone();
        let price = instrument.price_quote_minor(*price_ticks)?;
        sim_taker.apply_fill(&instrument, symbol, *taker_side, *qty, price);
        sim_maker.apply_fill(&instrument, symbol, taker_side.opposite(), *qty, price);
    }
    let ok_taker = margin_ok(engine, &sim_taker, &marks);
    let ok_maker = margin_ok(engine, &sim_maker, &marks);
    Some(ok_taker && ok_maker)
}

fn margin_ok(engine: &Engine, sim: &MarginAccount, marks: &BTreeMap<String, MarkSet>) -> bool {
    let Some(summary) = engine
        .margin_engine_ref()
        .margin_summary(sim, engine.instruments(), marks)
    else {
        return false;
    };
    summary.equity_quote_minor >= to_i128(summary.maintenance_quote_minor)
}

/// RFQ fee rails (G-38): the taker pays the tier's taker rate per leg,
/// capped at `cap_bps x premium` for option legs (F-2), then Derive's
/// grouped multi-leg discount ladder applies (cheapest group free, next
/// two at 50%); makers pay zero — the Paradigm dealer economics.
#[must_use]
pub fn rfq_taker_fees(
    engine: &Engine,
    taker: SubaccountId,
    legs: &[(Symbol, Side, u64, u64)],
) -> Vec<i128> {
    let tier = engine.fee_schedule_ref().tier_for(taker);
    let caps = engine.config().option_fee_caps;
    let marks = engine.build_marks(engine.now_ref());
    let mut classified: Vec<(LegFeeClass, u128)> = Vec::with_capacity(legs.len());
    for (symbol, taker_side, qty, price_ticks) in legs {
        let Some(instrument) = engine.instruments().get(symbol) else {
            classified.push((LegFeeClass::Perp, 0));
            continue;
        };
        match instrument {
            Instrument::Perp(_) => {
                let notional = instrument
                    .notional_quote_minor(*price_ticks, *qty)
                    .unwrap_or(0);
                let fee = poc_economics::FeeCalculator::taker_fee(tier, notional).unwrap_or(0);
                classified.push((LegFeeClass::Perp, fee.unsigned_abs()));
            }
            Instrument::Option(m) => {
                let premium = instrument
                    .notional_quote_minor(*price_ticks, *qty)
                    .unwrap_or(0);
                let underlying = marks
                    .as_ref()
                    .and_then(|ms| ms.get(instrument.base_symbol()))
                    .map(|s| s.spot_quote_minor_per_base)
                    .unwrap_or(m.strike_quote_minor);
                let notional = instrument
                    .position_notional_minor(underlying, i64::try_from(*qty).unwrap_or(i64::MAX))
                    .unwrap_or(0);
                let is_call = matches!(m.kind, poc_core::OptionKind::Call);
                let class = poc_rfq::classify_leg(true, is_call, *taker_side);
                let fee =
                    poc_economics::FeeCalculator::option_taker_fee(tier, &caps, notional, premium)
                        .unwrap_or(0);
                classified.push((class, fee.unsigned_abs()));
            }
        }
    }
    poc_rfq::apply_group_discounts(&classified)
        .into_iter()
        .map(to_i128)
        .collect()
}

/// Plan the taker's execution of one quote into journal events.
/// Pure over the pre-command snapshot.
pub(crate) fn plan_rfq_execute(
    engine: &Engine,
    taker: SubaccountId,
    rfq_id: u64,
    quote_id: u64,
    now: TimestampMs,
) -> Vec<Event> {
    let planned = resolve_execution(engine, taker, rfq_id, quote_id, now);
    let planned = match planned {
        Ok(p) => p,
        Err(reason) => {
            return vec![Event::RfqRejected {
                subaccount: taker,
                reason,
                ts: now,
            }]
        }
    };
    match rfq_margin_ok(engine, taker, planned.maker, &planned.legs, now) {
        Some(true) => {}
        _ => {
            return vec![Event::RfqRejected {
                subaccount: taker,
                reason: "insufficient margin for package",
                ts: now,
            }]
        }
    }
    let fees = rfq_taker_fees(engine, taker, &planned.legs);
    let mut trades = Vec::with_capacity(planned.legs.len());
    let mut seq = engine.seq_ref();
    for (i, (symbol, taker_side, qty, price_ticks)) in planned.legs.iter().enumerate() {
        let notional = engine
            .instruments()
            .get(symbol)
            .and_then(|i| i.notional_quote_minor(*price_ticks, *qty))
            .unwrap_or(0);
        seq = seq.saturating_add(1);
        trades.push(Trade {
            seq,
            symbol: symbol.clone(),
            taker_order_id: 0,
            maker_order_id: 0,
            taker_subaccount: taker,
            maker_subaccount: planned.maker,
            maker_side: taker_side.opposite(),
            price_ticks: *price_ticks,
            qty_lots: *qty,
            notional_quote_minor: notional,
            taker_fee_quote_minor: fees.get(i).copied().unwrap_or(0),
            maker_fee_quote_minor: 0,
            ts: now,
        });
    }
    vec![Event::RfqSettled {
        rfq_id,
        quote_id,
        taker,
        maker: planned.maker,
        trades,
        taker_fees_quote_minor: fees,
    }]
}
