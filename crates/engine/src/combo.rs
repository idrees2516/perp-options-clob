//! Combined (multi-leg) order planning — the three matching regimes
//! and the thin-book handoff (G-11 depth).
//!
//! The pure algebra lives in [`poc_combo`]; this module plans against
//! live engine state and journals ordinary `TradeExecuted` events for
//! every leg (so positions, cash, fees, revenue routing, MMP and the
//! volume ledger all flow through the ONE settlement path) plus
//! combo-side events that mutate only the combo book and its
//! reservations.
//!
//! Regimes:
//!
//! 1. **combo-vs-combo** — at the resting limit; the resting owner is
//!    the maker (maker fee 0, the negotiated rails); leg prices come
//!    from [`poc_combo::distribute_net`].
//! 2. **implied-in** — the new combo takes from the per-leg books via
//!    synthetic IOC takers bounded at the walk's worst tick; STP is
//!    preserved per leg and a two-pass correction keeps lot ratios
//!    exact; real makers earn maker fees.
//! 3. **implied-out** — a plain order's remainder fills into a resting
//!    combo at the combo's implied leg price while the combo hedges its
//!    other legs at the touch; the aggressor pays taker fees, the combo
//!    owner is the maker on the aggressed leg and the taker on hedges.

use std::collections::BTreeMap;

use poc_combo::{
    canonicalize_legs, distribute_net, implied_out_bound, plan_implied_in, ComboLeg, ComboOrder,
    ComboTif, FacingLevel,
};
use poc_core::{
    mul_div, Instrument, Order, OrderType, SelfTradePrevention, Side, SubaccountId, Symbol,
    TimeInForce, TimestampMs,
};

use crate::command::ComboLegCommand;
use crate::engine::Engine;
use crate::event::{ComboTrade, Event, Trade};
use crate::institutions;

/// Combined-order configuration (G-11 depth).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComboConfig {
    /// Maximum legs in one package.
    pub max_legs: usize,
    /// Whether implied matching (in/out) is enabled.
    pub implied_matching: bool,
    /// Depth levels the implied-in walk surveys per leg.
    pub implied_depth_levels: usize,
    /// Whether IOC/FOK shortfalls auto-publish an RFQ.
    pub auto_rfq_on_shortfall: bool,
    /// Quoting window of auto-published RFQs, ms.
    pub auto_rfq_ttl_ms: TimestampMs,
}

impl Default for ComboConfig {
    fn default() -> Self {
        Self {
            max_legs: 8,
            implied_matching: true,
            implied_depth_levels: 16,
            auto_rfq_on_shortfall: true,
            auto_rfq_ttl_ms: 3_000,
        }
    }
}

/// Composite structure BBO: what the leg books imply for one package
/// unit (per-unit, quote minor, buyer-cost convention). Each side is
/// independently available — a one-sided wing prices only the direction
/// it can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositeBbo {
    /// Best available per-unit proceeds for a package SELLER sweeping
    /// into the leg books (`None` when a needed touch is missing).
    pub bid_proceeds_quote_minor_per_unit: Option<i128>,
    /// Best available per-unit cost for a package BUYER sweeping the
    /// leg books (`None` when a needed touch is missing).
    pub ask_cost_quote_minor_per_unit: Option<i128>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn to_i128(v: u128) -> i128 {
    i128::try_from(v).unwrap_or(i128::MAX)
}

/// The owner's side on a leg, given their package side.
fn owner_side_on_leg(package_side: Side, leg_side: Side) -> Side {
    if package_side == Side::Bid {
        leg_side
    } else {
        leg_side.opposite()
    }
}

/// Fees for one synthetic leg fill against a REAL resting maker —
/// mirrors `plan_match`'s fee rail (option caps F-2, MM discount G-15,
/// insurance exemption G-23) but takes the marks once.
#[allow(clippy::too_many_arguments)]
fn leg_fill_fees(
    engine: &Engine,
    instrument: &Instrument,
    taker: SubaccountId,
    maker: SubaccountId,
    price_ticks: u64,
    qty_lots: u64,
    marks: Option<&BTreeMap<String, poc_margin::MarkSet>>,
) -> (i128, i128, u128) {
    let notional = instrument
        .notional_quote_minor(price_ticks, qty_lots)
        .unwrap_or(0);
    let fee_exempt = taker == crate::sweep::INSURANCE_SUBACCOUNT;
    if fee_exempt {
        return (0, 0, notional);
    }
    let taker_tier = engine.fee_schedule_ref().tier_for(taker);
    let maker_tier = engine.fee_schedule_ref().tier_for(maker);
    let (taker_fee, maker_fee) = match instrument {
        Instrument::Option(_) => {
            let caps = engine.config().option_fee_caps;
            let underlying_notional = marks
                .and_then(|ms| ms.get(instrument.base_symbol()))
                .and_then(|set| {
                    instrument.position_notional_minor(
                        set.spot_quote_minor_per_base,
                        i64::try_from(qty_lots).unwrap_or(i64::MAX),
                    )
                })
                .unwrap_or(0);
            (
                poc_economics::FeeCalculator::option_taker_fee(
                    taker_tier,
                    &caps,
                    underlying_notional,
                    notional,
                )
                .unwrap_or(0),
                poc_economics::FeeCalculator::option_maker_fee(
                    maker_tier,
                    &caps,
                    underlying_notional,
                    notional,
                )
                .unwrap_or(0),
            )
        }
        _ => (
            poc_economics::FeeCalculator::taker_fee(taker_tier, notional).unwrap_or(0),
            poc_economics::FeeCalculator::maker_fee(maker_tier, notional).unwrap_or(0),
        ),
    };
    let taker_fee = poc_economics::apply_tier_discount(taker_fee, engine.mm_discount_of(taker));
    let maker_fee = poc_economics::apply_tier_discount(maker_fee, engine.mm_discount_of(maker));
    (taker_fee, maker_fee, notional)
}

/// Maintenance-margin affordability of a set of leg fills for one
/// account (the RFQ execution gate's rule: equity >= maintenance).
fn account_can_afford(
    engine: &Engine,
    sub: SubaccountId,
    legs: &[(Symbol, Side, u64, u64)],
    marks: &BTreeMap<String, poc_margin::MarkSet>,
) -> bool {
    let Some(sim) = engine.account(sub).cloned() else {
        return false;
    };
    let mut sim = sim;
    for (symbol, side, qty, price_ticks) in legs {
        let Some(instrument) = engine.instruments().get(symbol).cloned() else {
            return false;
        };
        let Some(price) = instrument.price_quote_minor(*price_ticks) else {
            return false;
        };
        sim.apply_fill(&instrument, symbol, *side, *qty, price);
    }
    let Some(summary) =
        engine
            .margin_engine_ref()
            .margin_summary(&sim, engine.instruments(), marks)
    else {
        return false;
    };
    summary.equity_quote_minor >= to_i128(summary.maintenance_quote_minor)
}

/// A synthetic IOC taker order for one leg of a package.
pub(crate) fn synthetic_leg_order(
    owner: SubaccountId,
    leg_index_symbol: &Symbol,
    side: Side,
    qty_lots: u64,
    limit_ticks: u64,
    now: TimestampMs,
) -> Order {
    Order {
        id: 0,
        subaccount: owner,
        symbol: leg_index_symbol.clone(),
        side,
        order_type: OrderType::Limit,
        price_ticks: Some(limit_ticks),
        qty_lots,
        filled_lots: 0,
        tif: TimeInForce::Ioc,
        post_only: false,
        reduce_only: false,
        stp: SelfTradePrevention::CancelNewest,
        display_lots: None,
        trailing_extreme_quote_minor: None,
        oco_group: None,
        client_ts: 0,
        engine_ts: now,
    }
}

/// Emit the exhaustion closes for makers fully consumed by a fill list.
fn maker_close_events(
    engine: &Engine,
    symbol: &Symbol,
    fills: &[poc_orderbook::Fill],
) -> Vec<Event> {
    let mut events = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for fill in fills {
        if !seen.insert(fill.maker_order_id) {
            continue;
        }
        if let Some(resting) = engine
            .books
            .get(symbol)
            .and_then(|b| b.get(fill.maker_order_id))
        {
            if resting.order.open_qty() == fill.qty_lots {
                let mut final_order = resting.order.clone();
                final_order.filled_lots = final_order.qty_lots;
                events.push(Event::OrderClosed {
                    order_id: fill.maker_order_id,
                    subaccount: fill.maker_subaccount,
                    symbol: symbol.clone(),
                    order: final_order,
                    reason: crate::event::OrderCloseReason::Filled,
                });
            }
        }
    }
    events
}

/// Opponents of a signature in best-first priority order for a taker,
/// excluding ids in `exclude`.
fn opponents_in_priority_order(
    engine: &Engine,
    signature: &poc_combo::ComboSignature,
    taker: SubaccountId,
    taker_side: Side,
    taker_limit: i128,
    exclude: &std::collections::BTreeSet<u64>,
) -> Vec<ComboOrder> {
    let mut out: Vec<ComboOrder> = Vec::new();
    for combo in engine.combo_book.all() {
        if combo.side == taker_side
            || !combo.is_open()
            || combo.owner == taker // STP: never trade against yourself
            || exclude.contains(&combo.id)
        {
            continue;
        }
        if &combo.signature() != signature {
            continue;
        }
        let (sell_limit, buy_limit) = if taker_side == Side::Bid {
            (combo.limit_quote_minor_per_unit, taker_limit)
        } else {
            (taker_limit, combo.limit_quote_minor_per_unit)
        };
        if sell_limit > buy_limit {
            continue;
        }
        out.push(combo.clone());
    }
    out.sort_by(|a, b| {
        let key = |o: &ComboOrder| match taker_side {
            // Incoming buy: lowest seller limit first.
            Side::Bid => (o.limit_quote_minor_per_unit, o.seq),
            // Incoming sell: highest buyer limit first.
            Side::Ask => (-o.limit_quote_minor_per_unit, o.seq),
        };
        key(a).cmp(&key(b))
    });
    out
}

/// Build engine legs from commands (canonicalized).
fn build_legs(engine: &Engine, legs_cmd: &[ComboLegCommand]) -> Option<Vec<ComboLeg>> {
    let mut legs = Vec::with_capacity(legs_cmd.len());
    for l in legs_cmd {
        let instrument = engine.instruments().get(&l.symbol)?;
        legs.push(ComboLeg {
            symbol: l.symbol.clone(),
            side: l.side,
            ratio: l.ratio,
            tick_size_quote_minor: instrument.tick_size(),
            lot_size_base_minor: instrument.lot_size(),
            base_decimals: instrument.base_decimals(),
        });
    }
    canonicalize_legs(legs)
}

/// Per-leg marks (quote minor per base) for a package.
fn package_marks(
    engine: &Engine,
    legs: &[ComboLeg],
    marks: &BTreeMap<String, poc_margin::MarkSet>,
) -> Option<BTreeMap<Symbol, u128>> {
    let mut out = BTreeMap::new();
    for leg in legs {
        let instrument = engine.instruments().get(&leg.symbol)?;
        let price = engine.instrument_mark(instrument, marks)?;
        out.insert(leg.symbol.clone(), price);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Plan a combined-order placement (pure over the pre-command snapshot).
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_place_combo(
    engine: &Engine,
    owner: SubaccountId,
    legs_cmd: &[ComboLegCommand],
    side: Side,
    limit: i128,
    units: u64,
    tif: ComboTif,
    now: TimestampMs,
) -> Vec<Event> {
    let combo_id = engine.combo_book.next_id();
    let reject = |reason: &'static str| {
        vec![Event::ComboRejected {
            combo_id,
            owner,
            reason,
            ts: now,
        }]
    };

    if units == 0 {
        return reject("units must be positive");
    }
    if legs_cmd.is_empty() || legs_cmd.len() > engine.config.combo.max_legs {
        return reject("leg count out of bounds");
    }
    if !engine.accounts.contains_key(&owner) {
        return reject("unknown account");
    }
    if legs_cmd.iter().any(|l| l.ratio == 0) {
        return reject("leg ratio must be positive");
    }
    let Some(legs) = build_legs(engine, legs_cmd) else {
        return reject("unknown or duplicate instrument in package");
    };
    let Some(marks_map) = engine.build_marks(now) else {
        return reject("missing mark");
    };
    let Some(leg_marks) = package_marks(engine, &legs, &marks_map) else {
        return reject("missing mark");
    };

    // FOK: upfront executability (compatible opposing combos + implied-in
    // walk must cover the request).
    if tif == ComboTif::Fok {
        let signature = poc_combo::ComboSignature {
            legs: legs
                .iter()
                .map(|l| (l.symbol.clone(), l.side, l.ratio))
                .collect(),
        };
        let exclude = std::collections::BTreeSet::new();
        let opposing: u64 =
            opponents_in_priority_order(engine, &signature, owner, side, limit, &exclude)
                .iter()
                .map(|o| o.open_units())
                .sum();
        let walk_units = implied_walk_units(engine, &legs, side, limit)
            .map(|(u, _)| u)
            .unwrap_or(0);
        if opposing.saturating_add(walk_units) < units {
            return reject("fok: insufficient package liquidity");
        }
    }

    // --- Package margin gate at the boundary price, fused with the
    // IM-delta reservation (one simulation pass).
    let Some((boundary_ticks, _)) = distribute_net(&legs, &leg_marks, limit) else {
        return reject("net distribution overflow");
    };
    let mut sim = engine
        .accounts
        .get(&owner)
        .cloned()
        .unwrap_or_else(|| poc_margin::MarginAccount::new(owner, 0));
    let mut boundary_legs: Vec<(Symbol, Side, u64, u64)> = Vec::new();
    for (i, leg) in legs.iter().enumerate() {
        let instrument = engine.instruments().get(&leg.symbol).cloned();
        let Some(instrument) = instrument else {
            return reject("unknown instrument in package");
        };
        let Some(price) = instrument.price_quote_minor(boundary_ticks[i]) else {
            return reject("boundary price off grid");
        };
        let os = owner_side_on_leg(side, leg.side);
        sim.apply_fill(&instrument, &leg.symbol, os, leg.lots_for(units), price);
        boundary_legs.push((
            leg.symbol.clone(),
            os,
            leg.lots_for(units),
            boundary_ticks[i],
        ));
    }
    let Some(summary) =
        engine
            .margin_engine_ref()
            .margin_summary(&sim, engine.instruments(), &marks_map)
    else {
        return reject("margin summary unavailable");
    };
    if summary.equity_quote_minor < to_i128(summary.initial_quote_minor) {
        return reject("insufficient margin for package");
    }
    let before_im = engine.margin_engine_ref().initial_margin(
        engine.accounts.get(&owner).unwrap_or(&sim),
        engine.instruments(),
        &marks_map,
    );
    let full_increment = summary.initial_quote_minor.saturating_sub(before_im);

    // The ack precedes every fill event: the combo ALWAYS rests first
    // (full units, full reservation) and the fill events scale it down.
    // A fully-filled placement leaves nothing resting; an IOC/FOK
    // remainder is closed by the lifecycle event below.
    let mut events: Vec<Event> = vec![Event::ComboPlaced {
        combo_id,
        owner,
        legs: legs.clone(),
        side,
        limit_quote_minor_per_unit: limit,
        units,
        tif,
        margin_reserved_quote_minor: full_increment,
        ts: now,
    }];
    let mut remaining = units;

    // --- Phase A: combo-vs-combo at the RESTING limit.
    let signature = poc_combo::ComboSignature {
        legs: legs
            .iter()
            .map(|l| (l.symbol.clone(), l.side, l.ratio))
            .collect(),
    };
    let mut excluded: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    while remaining > 0 {
        let opponents =
            opponents_in_priority_order(engine, &signature, owner, side, limit, &excluded);
        let Some(opp) = opponents.into_iter().next() else {
            break;
        };
        excluded.insert(opp.id);
        let u = remaining.min(opp.open_units());
        if u == 0 {
            continue;
        }
        // Trade at the RESTING limit; legs split on-grid from the marks.
        let Some((ticks, effective)) =
            distribute_net(&legs, &leg_marks, opp.limit_quote_minor_per_unit)
        else {
            break;
        };
        let taker_ok = match side {
            Side::Bid => effective <= limit,
            Side::Ask => effective >= limit,
        };
        if !taker_ok || !opp.accepts_net(effective) {
            // Grid rounding pushed the effective net outside a limit:
            // conservatively stop combo-vs-combo (implied-in/rest follow).
            break;
        }
        // Bilateral margin: the maker must afford the mirrored package.
        let maker_legs: Vec<(Symbol, Side, u64, u64)> = legs
            .iter()
            .enumerate()
            .map(|(i, leg)| {
                (
                    leg.symbol.clone(),
                    owner_side_on_leg(opp.side, leg.side),
                    leg.lots_for(u),
                    ticks[i],
                )
            })
            .collect();
        if !account_can_afford(engine, opp.owner, &maker_legs, &marks_map) {
            continue; // this opponent is margin-blocked; try the next
        }
        // Taker fees on the negotiated rails (makers pay zero).
        let taker_legs: Vec<(Symbol, Side, u64, u64)> = legs
            .iter()
            .enumerate()
            .map(|(i, leg)| {
                (
                    leg.symbol.clone(),
                    owner_side_on_leg(side, leg.side),
                    leg.lots_for(u),
                    ticks[i],
                )
            })
            .collect();
        let fees = institutions::rfq_taker_fees(engine, owner, &taker_legs);
        let mut seq = engine.seq_ref();
        for (i, leg) in legs.iter().enumerate() {
            let maker_side = owner_side_on_leg(opp.side, leg.side);
            let notional = engine
                .instruments
                .get(&leg.symbol)
                .and_then(|inst| inst.notional_quote_minor(ticks[i], leg.lots_for(u)))
                .unwrap_or(0);
            seq = seq.saturating_add(1);
            events.push(Event::TradeExecuted(Box::new(Trade {
                seq,
                symbol: leg.symbol.clone(),
                taker_order_id: 0,
                maker_order_id: 0,
                taker_subaccount: owner,
                maker_subaccount: opp.owner,
                maker_side,
                price_ticks: ticks[i],
                qty_lots: leg.lots_for(u),
                notional_quote_minor: notional,
                taker_fee_quote_minor: fees.get(i).copied().unwrap_or(0),
                maker_fee_quote_minor: 0,
                ts: now,
            })));
        }
        let taker_legs_view: Vec<(Symbol, Side, u64, u64)> = legs
            .iter()
            .enumerate()
            .map(|(i, leg)| {
                (
                    leg.symbol.clone(),
                    owner_side_on_leg(side, leg.side),
                    leg.lots_for(u),
                    ticks[i],
                )
            })
            .collect();
        let maker_legs_view: Vec<(Symbol, Side, u64, u64)> = legs
            .iter()
            .enumerate()
            .map(|(i, leg)| {
                (
                    leg.symbol.clone(),
                    owner_side_on_leg(opp.side, leg.side),
                    leg.lots_for(u),
                    ticks[i],
                )
            })
            .collect();
        events.push(Event::ComboTraded(Box::new(ComboTrade {
            combo_id,
            owner,
            counterparty: opp.owner,
            counterparty_combo_id: Some(opp.id),
            units: u,
            regime: "combo",
            leg_executions: taker_legs_view,
            effective_net_quote_minor_per_unit: effective,
            ts: now,
        })));
        events.push(Event::ComboTraded(Box::new(ComboTrade {
            combo_id: opp.id,
            owner: opp.owner,
            counterparty: owner,
            counterparty_combo_id: Some(combo_id),
            units: u,
            regime: "combo",
            leg_executions: maker_legs_view,
            effective_net_quote_minor_per_unit: effective,
            ts: now,
        })));
        remaining -= u;
    }

    // --- Phase B: implied-in from the per-leg books.
    if engine.config.combo.implied_matching && remaining > 0 {
        if let Some(walk) = implied_walk_plan(engine, &legs, side, limit) {
            let target = walk.units.min(remaining);
            if target > 0 {
                let emitted = plan_implied_in_execution(
                    engine,
                    &legs,
                    side,
                    limit,
                    combo_id,
                    owner,
                    target,
                    &walk.worst_ticks,
                    now,
                );
                if let Some(done) = emitted {
                    events.extend(done.0);
                    remaining -= done.1;
                }
            }
        }
    }

    // --- Phase C: lifecycle of the remainder.
    if remaining > 0 && matches!(tif, ComboTif::Ioc | ComboTif::Fok) {
        events.push(Event::ComboClosed {
            combo_id,
            owner,
            reason: if tif == ComboTif::Ioc {
                "ioc-remainder"
            } else {
                "fok"
            },
            ts: now,
        });
        if engine.config.combo.auto_rfq_on_shortfall {
            events.extend(plan_combo_shortfall_rfq(
                engine, owner, &legs, side, limit, remaining, now,
            ));
        }
    }
    events
}

/// The implied-in walk's unit bound for a package (no execution).
fn implied_walk_units(
    engine: &Engine,
    legs: &[ComboLeg],
    side: Side,
    limit: i128,
) -> Option<(u64, Vec<u64>)> {
    implied_walk_plan(engine, legs, side, limit).map(|w| (w.units, w.worst_ticks))
}

fn implied_walk_plan(
    engine: &Engine,
    legs: &[ComboLeg],
    side: Side,
    limit: i128,
) -> Option<poc_combo::ImpliedInPlan> {
    let levels = engine.config.combo.implied_depth_levels.max(1);
    let mut facing: Vec<Vec<FacingLevel>> = Vec::with_capacity(legs.len());
    for leg in legs {
        let book = engine.books.get(&leg.symbol)?;
        // The side of the leg book the TAKER draws from.
        let draw_side = match (side, leg.side) {
            (Side::Bid, Side::Bid) => Side::Ask,
            (Side::Bid, Side::Ask) => Side::Bid,
            (Side::Ask, Side::Bid) => Side::Bid,
            (Side::Ask, Side::Ask) => Side::Ask,
        };
        let (bids, asks) = book.depth(levels);
        let source = if draw_side == Side::Bid { bids } else { asks };
        facing.push(
            source
                .into_iter()
                .map(|lvl| FacingLevel {
                    price_ticks: lvl.price_ticks,
                    lots: lvl.total_qty_lots,
                })
                .collect(),
        );
    }
    plan_implied_in(legs, &facing, side, limit, levels)
}

/// Execute the implied-in take: two-pass STP-corrected synthetic IOC
/// per leg. Returns `(events, units executed)` when the net and margin
/// gates pass, `None` when the phase must be skipped.
#[allow(clippy::too_many_arguments)]
fn plan_implied_in_execution(
    engine: &Engine,
    legs: &[ComboLeg],
    side: Side,
    limit: i128,
    combo_id: u64,
    owner: SubaccountId,
    target_units: u64,
    worst_ticks: &[u64],
    now: TimestampMs,
) -> Option<(Vec<Event>, u64)> {
    // Pass 1: discover STP-adjusted availability per leg (pure).
    // The side of the book the taker draws from, and the taker's own
    // side on the leg (its mirror).
    let draw_sides: Vec<Side> = legs
        .iter()
        .map(|leg| match (side, leg.side) {
            (Side::Bid, Side::Bid) => Side::Ask,
            (Side::Bid, Side::Ask) => Side::Bid,
            (Side::Ask, Side::Bid) => Side::Bid,
            (Side::Ask, Side::Ask) => Side::Ask,
        })
        .collect();
    let taker_sides: Vec<Side> = draw_sides.iter().map(|s| s.opposite()).collect();
    let mut pass1 = Vec::with_capacity(legs.len());
    let mut shrunk = false;
    for (i, leg) in legs.iter().enumerate() {
        let book = engine.books.get(&leg.symbol)?;
        let order = synthetic_leg_order(
            owner,
            &leg.symbol,
            taker_sides[i],
            leg.lots_for(target_units),
            worst_ticks[i],
            now,
        );
        let outcome = book.match_taker(&order, Some(worst_ticks[i]), false, order.stp);
        let filled: u64 = outcome.fills.iter().map(|f| f.qty_lots).sum();
        if filled < leg.lots_for(target_units) {
            shrunk = true;
        }
        pass1.push(outcome);
    }
    let mut exec_units = target_units;
    if shrunk {
        exec_units = legs
            .iter()
            .enumerate()
            .map(|(i, leg)| {
                let filled: u64 = pass1[i].fills.iter().map(|f| f.qty_lots).sum();
                filled / u64::from(leg.ratio)
            })
            .min()
            .unwrap_or(0);
    }
    if exec_units == 0 {
        return None;
    }

    // Pass 2: the final pure match with exact lot ratios.
    let mut outcomes = Vec::with_capacity(legs.len());
    let mut total_flow: i128 = 0;
    for (i, leg) in legs.iter().enumerate() {
        let book = engine.books.get(&leg.symbol)?;
        let order = synthetic_leg_order(
            owner,
            &leg.symbol,
            taker_sides[i],
            leg.lots_for(exec_units),
            worst_ticks[i],
            now,
        );
        let outcome = book.match_taker(&order, Some(worst_ticks[i]), false, order.stp);
        for fill in &outcome.fills {
            let instrument = engine.instruments().get(&leg.symbol)?;
            let notional = instrument
                .notional_quote_minor(fill.price_ticks, fill.qty_lots)
                .unwrap_or(0);
            let flow = match leg.side {
                Side::Bid => to_i128(notional),
                Side::Ask => -to_i128(notional),
            };
            total_flow = total_flow.checked_add(flow)?;
        }
        outcomes.push(outcome);
    }
    // Net verification from the ACTUAL fills (per package unit).
    let per_unit = total_flow / i128::from(exec_units);
    let net_ok = match side {
        Side::Bid => per_unit <= limit,
        Side::Ask => per_unit >= limit,
    };
    if !net_ok {
        return None;
    }
    // Margin gate for the taker package (maintenance rule).
    let marks = engine.build_marks(now);
    let mut margin_legs: Vec<(Symbol, Side, u64, u64)> = Vec::new();
    for (i, leg) in legs.iter().enumerate() {
        let filled: u64 = outcomes[i].fills.iter().map(|f| f.qty_lots).sum();
        if filled == 0 {
            continue;
        }
        // VWAP in TICKS (the margin gate takes prices in ticks).
        let weighted: u64 = outcomes[i]
            .fills
            .iter()
            .map(|f| f.price_ticks.saturating_mul(f.qty_lots))
            .sum();
        let vwap_ticks = weighted / filled;
        margin_legs.push((leg.symbol.clone(), taker_sides[i], filled, vwap_ticks));
    }
    if let Some(marks) = &marks {
        if !account_can_afford(engine, owner, &margin_legs, marks) {
            return None;
        }
    }

    // Emit: trades (real makers), STP cancels, maker closes, combo view.
    let mut events = Vec::new();
    let mut seq = engine.seq_ref();
    let mut leg_views: Vec<(Symbol, Side, u64, u64)> = Vec::new();
    for (i, leg) in legs.iter().enumerate() {
        let instrument = engine.instruments.get(&leg.symbol).cloned()?;
        let mut leg_lots = 0_u64;
        let mut leg_vwap_ticks = 0_u64;
        for fill in &outcomes[i].fills {
            let (taker_fee, maker_fee, notional) = leg_fill_fees(
                engine,
                &instrument,
                owner,
                fill.maker_subaccount,
                fill.price_ticks,
                fill.qty_lots,
                marks.as_ref(),
            );
            seq = seq.saturating_add(1);
            events.push(Event::TradeExecuted(Box::new(Trade {
                seq,
                symbol: leg.symbol.clone(),
                taker_order_id: 0,
                maker_order_id: fill.maker_order_id,
                taker_subaccount: owner,
                maker_subaccount: fill.maker_subaccount,
                maker_side: fill.maker_side,
                price_ticks: fill.price_ticks,
                qty_lots: fill.qty_lots,
                notional_quote_minor: notional,
                taker_fee_quote_minor: taker_fee,
                maker_fee_quote_minor: maker_fee,
                ts: now,
            })));
            leg_lots += fill.qty_lots;
            leg_vwap_ticks += fill.price_ticks * fill.qty_lots;
        }
        leg_vwap_ticks = leg_vwap_ticks.checked_div(leg_lots).unwrap_or(0);
        leg_views.push((leg.symbol.clone(), taker_sides[i], leg_lots, leg_vwap_ticks));
        events.extend(maker_close_events(engine, &leg.symbol, &outcomes[i].fills));
        if !outcomes[i].stp.canceled_makers.is_empty() {
            events.push(Event::StpCancels {
                maker_ids: outcomes[i].stp.canceled_makers.clone(),
                ts: now,
            });
        }
    }
    events.push(Event::ComboTraded(Box::new(ComboTrade {
        combo_id,
        owner,
        counterparty: 0,
        counterparty_combo_id: None,
        units: exec_units,
        regime: "implied-in",
        leg_executions: leg_views,
        effective_net_quote_minor_per_unit: per_unit,
        ts: now,
    })));
    Some((events, exec_units))
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// Plan a combo cancellation.
pub(crate) fn plan_cancel_combo(
    engine: &Engine,
    owner: SubaccountId,
    combo_id: u64,
    now: TimestampMs,
) -> Vec<Event> {
    match engine.combo_book.get(combo_id) {
        Some(combo) if combo.owner == owner && combo.is_open() => vec![Event::ComboClosed {
            combo_id,
            owner,
            reason: "canceled",
            ts: now,
        }],
        _ => vec![Event::ComboRejected {
            combo_id,
            owner,
            reason: "unknown or foreign combo",
            ts: now,
        }],
    }
}

/// Plan the combo part of a cancel-all (optionally restricted to one
/// instrument's packages).
pub(crate) fn plan_cancel_all_combos(
    engine: &Engine,
    owner: SubaccountId,
    symbol: Option<&str>,
    now: TimestampMs,
) -> Vec<Event> {
    engine
        .combo_book
        .of_owner(owner)
        .into_iter()
        .filter(|combo| symbol.map_or(true, |s| combo.legs.iter().any(|l| l.symbol.as_str() == s)))
        .map(|combo| Event::ComboClosed {
            combo_id: combo.id,
            owner,
            reason: "canceled",
            ts: now,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Implied-out: a plain order's remainder fills into resting combos
// ---------------------------------------------------------------------------

/// The side of `symbol`'s book a resting combo's owner trades on.
fn combo_owner_side_on_leg(combo: &ComboOrder, leg: &ComboLeg) -> Side {
    owner_side_on_leg(combo.side, leg.side)
}

/// Plan implied-out fills for a plain taker's remainder on one symbol.
/// Returns the events plus the still-unfilled taker lots.
pub(crate) fn plan_implied_out_fills(
    engine: &Engine,
    taker_order: &Order,
    remaining_lots: u64,
    now: TimestampMs,
) -> (Vec<Event>, u64) {
    let mut events = Vec::new();
    let mut remaining = remaining_lots;
    if !engine.config.combo.implied_matching || remaining == 0 {
        return (events, remaining);
    }
    let combos = engine.combo_book.on_symbol(&taker_order.symbol);
    if combos.is_empty() {
        return (events, remaining);
    }
    let Some(marks_map) = engine.build_marks(now) else {
        return (events, remaining);
    };

    // Score every candidate combo: the implied tick price it offers the
    // aggressor on this leg (given BBO hedges on its other legs).
    struct Candidate {
        combo: ComboOrder,
        implied_ticks: u64,
        hedge_legs: Vec<(usize, u64)>, // (leg index, hedge touch ticks)
        hedge_flows: BTreeMap<Symbol, i128>,
        hedge_capacity_units: u64,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    'combos: for combo in combos {
        let combo = combo.clone();
        let Some(idx) = combo
            .legs
            .iter()
            .position(|l| l.symbol == taker_order.symbol)
        else {
            continue;
        };
        let aggressed_leg = &combo.legs[idx];
        // The combo owner must be the aggressor's counterparty on this leg.
        if combo_owner_side_on_leg(&combo, aggressed_leg) == taker_order.side {
            continue;
        }
        // Hedge plan: the owner takes from each other leg's book at the
        // TOUCH on their side (v1: touch only — documented boundary).
        let mut hedge_flows = BTreeMap::new();
        let mut hedge_legs = Vec::new();
        let mut hedge_capacity = u64::MAX;
        for (k, other) in combo.legs.iter().enumerate() {
            if k == idx {
                continue;
            }
            let owner_side = combo_owner_side_on_leg(&combo, other);
            let Some(book) = engine.books.get(&other.symbol) else {
                continue 'combos;
            };
            // The owner BUYS at the ask touch, SELLS at the bid touch.
            let touch = match owner_side {
                Side::Bid => book.best_ask(),
                Side::Ask => book.best_bid(),
            };
            let Some(touch_ticks) = touch else {
                continue 'combos; // cannot hedge this leg at all
            };
            let touch_lots = match owner_side {
                Side::Bid => book
                    .depth(1)
                    .1
                    .first()
                    .map(|l| l.total_qty_lots)
                    .unwrap_or(0),
                Side::Ask => book
                    .depth(1)
                    .0
                    .first()
                    .map(|l| l.total_qty_lots)
                    .unwrap_or(0),
            };
            let Some(flow) = other.flow_per_unit(touch_ticks) else {
                continue 'combos;
            };
            hedge_flows.insert(other.symbol.clone(), flow);
            hedge_capacity = hedge_capacity.min(touch_lots / u64::from(other.ratio));
            hedge_legs.push((k, touch_ticks));
        }
        let Some(bound) = implied_out_bound(&combo, &taker_order.symbol, &hedge_flows) else {
            continue;
        };
        let implied = bound.ticks();
        // Aggressor limit: improvements through the limit, never worse.
        let taker_ok = match taker_order.side {
            Side::Ask => implied >= taker_order.price_ticks.unwrap_or(0),
            Side::Bid => implied <= taker_order.price_ticks.unwrap_or(u64::MAX),
        };
        if !taker_ok {
            continue;
        }
        candidates.push(Candidate {
            combo,
            implied_ticks: implied,
            hedge_legs,
            hedge_flows,
            hedge_capacity_units: hedge_capacity,
        });
    }
    // Best price for the aggressor first; ties by combo id (time).
    candidates.sort_by(|a, b| {
        let key = |c: &Candidate| match taker_order.side {
            // Aggressor sells: higher implied bid first.
            Side::Ask => (-(c.implied_ticks as i128), c.combo.id),
            // Aggressor buys: lower implied ask first.
            Side::Bid => (c.implied_ticks as i128, c.combo.id),
        };
        key(a).cmp(&key(b))
    });

    let mut seq = engine.seq_ref();
    for cand in candidates {
        if remaining == 0 {
            break;
        }
        let Some(idx) = cand
            .combo
            .legs
            .iter()
            .position(|l| l.symbol == taker_order.symbol)
        else {
            continue;
        };
        let aggressed_leg = &cand.combo.legs[idx];
        let by_ratio = remaining / u64::from(aggressed_leg.ratio);
        let mut u = by_ratio
            .min(cand.combo.open_units())
            .min(cand.hedge_capacity_units);
        if u == 0 {
            continue;
        }
        // Net verification (defense): the effective net from the implied
        // price plus the hedge touches must satisfy the combo's limit.
        let mut per_unit = cand
            .hedge_flows
            .values()
            .fold(0_i128, |acc, f| acc.saturating_add(*f));
        per_unit =
            per_unit.saturating_add(aggressed_leg.flow_per_unit(cand.implied_ticks).unwrap_or(0));
        if !cand.combo.accepts_net(per_unit) {
            continue;
        }
        // The combo owner must afford package + hedges (maintenance).
        let mut margin_legs: Vec<(Symbol, Side, u64, u64)> = vec![(
            aggressed_leg.symbol.clone(),
            combo_owner_side_on_leg(&cand.combo, aggressed_leg),
            aggressed_leg.lots_for(u),
            cand.implied_ticks,
        )];
        for &(k, touch) in &cand.hedge_legs {
            let other = &cand.combo.legs[k];
            margin_legs.push((
                other.symbol.clone(),
                combo_owner_side_on_leg(&cand.combo, other),
                other.lots_for(u),
                touch,
            ));
        }
        if !account_can_afford(engine, cand.combo.owner, &margin_legs, &marks_map) {
            continue;
        }
        // Shrink to the actual hedge fill available (pure re-check).
        // v1: hedges are touch-bounded IOC; match_taker tells the truth.
        let mut hedge_outcomes = Vec::new();
        for &(k, touch) in &cand.hedge_legs {
            let other = &cand.combo.legs[k];
            let Some(book) = engine.books.get(&other.symbol) else {
                continue;
            };
            let order = synthetic_leg_order(
                cand.combo.owner,
                &other.symbol,
                combo_owner_side_on_leg(&cand.combo, other),
                other.lots_for(u),
                touch,
                now,
            );
            let outcome = book.match_taker(&order, Some(touch), false, order.stp);
            hedge_outcomes.push(outcome);
        }
        let hedge_filled_units = cand
            .hedge_legs
            .iter()
            .enumerate()
            .map(|(slot, &(k, _))| {
                let filled: u64 = hedge_outcomes[slot].fills.iter().map(|f| f.qty_lots).sum();
                filled / u64::from(cand.combo.legs[k].ratio)
            })
            .min()
            .unwrap_or(0);
        u = u.min(hedge_filled_units);
        if u == 0 {
            continue;
        }
        // Re-run the hedges at the exact size (two-pass, like implied-in).
        hedge_outcomes.clear();
        for &(k, touch) in &cand.hedge_legs {
            let other = &cand.combo.legs[k];
            let Some(book) = engine.books.get(&other.symbol) else {
                continue;
            };
            let order = synthetic_leg_order(
                cand.combo.owner,
                &other.symbol,
                combo_owner_side_on_leg(&cand.combo, other),
                other.lots_for(u),
                touch,
                now,
            );
            hedge_outcomes.push(book.match_taker(&order, Some(touch), false, order.stp));
        }

        // --- Emit: aggressed leg trade (combo owner is the maker).
        let Some(instrument) = engine.instruments.get(&aggressed_leg.symbol).cloned() else {
            continue;
        };
        let lots = aggressed_leg.lots_for(u);
        let (taker_fee, maker_fee, notional) = leg_fill_fees(
            engine,
            &instrument,
            taker_order.subaccount,
            cand.combo.owner,
            cand.implied_ticks,
            lots,
            Some(&marks_map),
        );
        seq = seq.saturating_add(1);
        events.push(Event::TradeExecuted(Box::new(Trade {
            seq,
            symbol: aggressed_leg.symbol.clone(),
            taker_order_id: taker_order.id,
            maker_order_id: 0,
            taker_subaccount: taker_order.subaccount,
            maker_subaccount: cand.combo.owner,
            maker_side: combo_owner_side_on_leg(&cand.combo, aggressed_leg),
            price_ticks: cand.implied_ticks,
            qty_lots: lots,
            notional_quote_minor: notional,
            taker_fee_quote_minor: taker_fee,
            maker_fee_quote_minor: maker_fee,
            ts: now,
        })));
        // --- Emit: hedge trades (combo owner is the taker).
        let mut leg_views: Vec<(Symbol, Side, u64, u64)> = vec![(
            aggressed_leg.symbol.clone(),
            combo_owner_side_on_leg(&cand.combo, aggressed_leg),
            lots,
            cand.implied_ticks,
        )];
        for (slot, &(k, _touch)) in cand.hedge_legs.iter().enumerate() {
            let other = &cand.combo.legs[k];
            let Some(instrument) = engine.instruments.get(&other.symbol).cloned() else {
                continue;
            };
            let mut leg_lots = 0_u64;
            let mut leg_vwap = 0_u64;
            for fill in &hedge_outcomes[slot].fills {
                let (t_fee, m_fee, notion) = leg_fill_fees(
                    engine,
                    &instrument,
                    cand.combo.owner,
                    fill.maker_subaccount,
                    fill.price_ticks,
                    fill.qty_lots,
                    Some(&marks_map),
                );
                seq = seq.saturating_add(1);
                events.push(Event::TradeExecuted(Box::new(Trade {
                    seq,
                    symbol: other.symbol.clone(),
                    taker_order_id: 0,
                    maker_order_id: fill.maker_order_id,
                    taker_subaccount: cand.combo.owner,
                    maker_subaccount: fill.maker_subaccount,
                    maker_side: fill.maker_side,
                    price_ticks: fill.price_ticks,
                    qty_lots: fill.qty_lots,
                    notional_quote_minor: notion,
                    taker_fee_quote_minor: t_fee,
                    maker_fee_quote_minor: m_fee,
                    ts: now,
                })));
                leg_lots += fill.qty_lots;
                leg_vwap += fill.price_ticks * fill.qty_lots;
            }
            let leg_vwap = leg_vwap.checked_div(leg_lots).unwrap_or(0);
            if leg_lots > 0 {
                leg_views.push((
                    other.symbol.clone(),
                    combo_owner_side_on_leg(&cand.combo, other),
                    leg_lots,
                    leg_vwap,
                ));
            }
            events.extend(maker_close_events(
                engine,
                &other.symbol,
                &hedge_outcomes[slot].fills,
            ));
            if !hedge_outcomes[slot].stp.canceled_makers.is_empty() {
                events.push(Event::StpCancels {
                    maker_ids: hedge_outcomes[slot].stp.canceled_makers.clone(),
                    ts: now,
                });
            }
        }
        // --- Combo-side view.
        events.push(Event::ComboTraded(Box::new(ComboTrade {
            combo_id: cand.combo.id,
            owner: cand.combo.owner,
            counterparty: taker_order.subaccount,
            counterparty_combo_id: None,
            units: u,
            regime: "implied-out",
            leg_executions: leg_views,
            effective_net_quote_minor_per_unit: per_unit,
            ts: now,
        })));
        remaining -= lots;
    }
    (events, remaining)
}

// ---------------------------------------------------------------------------
// Thin-book handoff: auto-published RFQs
// ---------------------------------------------------------------------------

/// The combo shortfall handoff: publish an RFQ for the remainder,
/// collared by the taker's limit where the non-negative bound system
/// can represent it (buyer caps yes, seller floors no — the taker's
/// explicit execution remains the control).
pub(crate) fn plan_combo_shortfall_rfq(
    engine: &Engine,
    taker: SubaccountId,
    legs: &[ComboLeg],
    side: Side,
    limit: i128,
    remaining: u64,
    now: TimestampMs,
) -> Vec<Event> {
    let rfq_legs: Vec<crate::command::RfqLegCommand> = legs
        .iter()
        .map(|leg| crate::command::RfqLegCommand {
            symbol: leg.symbol.clone(),
            side: owner_side_on_leg(side, leg.side),
            qty_lots: leg.lots_for(remaining),
        })
        .collect();
    let max_total = if side == Side::Bid && limit > 0 {
        limit
            .unsigned_abs()
            .checked_mul(u128::from(remaining))
            .filter(|v| *v > 0)
    } else {
        None
    };
    engine.plan_rfq_create(
        taker,
        &rfq_legs,
        &[],
        None,
        max_total,
        engine.config.combo.auto_rfq_ttl_ms,
        now,
    )
}

/// The single-leg sweep shortfall handoff for plain IOC/FOK orders.
pub(crate) fn plan_single_leg_shortfall_rfq(
    engine: &Engine,
    order: &Order,
    remaining: u64,
    now: TimestampMs,
) -> Vec<Event> {
    let rfq_legs = vec![crate::command::RfqLegCommand {
        symbol: order.symbol.clone(),
        side: order.side,
        qty_lots: remaining,
    }];
    // Buyer caps: a buy order's limit bounds its cost; sellers' floors
    // are not representable in the non-negative bound system. The cap
    // is the NOTIONAL of the remainder at the limit, not the per-base
    // price.
    let max_total = match (order.side, order.price_ticks) {
        (Side::Bid, Some(ticks)) => engine
            .instruments
            .get(&order.symbol)
            .and_then(|i| i.notional_quote_minor(ticks, remaining))
            .filter(|v| *v > 0),
        _ => None,
    };
    engine.plan_rfq_create(
        order.subaccount,
        &rfq_legs,
        &[],
        None,
        max_total,
        engine.config.combo.auto_rfq_ttl_ms,
        now,
    )
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

impl Engine {
    /// The composite structure BBO: what the leg books imply for one
    /// package unit of a structure (buy cost and sell proceeds).
    #[must_use]
    pub fn composite_bbo(&self, legs: &[ComboLegCommand]) -> Option<CompositeBbo> {
        let legs = build_legs(self, legs)?;
        let mut ask_cost: i128 = 0;
        let mut bid_proceeds: i128 = 0;
        let mut ask_ok = true;
        let mut bid_ok = true;
        for leg in &legs {
            let book = self.books.get(&leg.symbol)?;
            // Buying the package: pay the ask on buyer-Bid legs, receive
            // the bid on buyer-Ask legs (buyer-convention flow).
            let ask_price = match leg.side {
                Side::Bid => book.best_ask(),
                Side::Ask => book.best_bid(),
            };
            // Selling the package: receive the bid on buyer-Bid legs,
            // pay the ask on buyer-Ask legs — which is exactly the
            // buyer-convention flow evaluated at the mirrored touch.
            let bid_price = match leg.side {
                Side::Bid => book.best_bid(),
                Side::Ask => book.best_ask(),
            };
            match (ask_price, leg.flow_per_unit(ask_price.unwrap_or(0))) {
                (Some(_), Some(flow)) => ask_cost = ask_cost.saturating_add(flow),
                _ => ask_ok = false,
            }
            match (bid_price, leg.flow_per_unit(bid_price.unwrap_or(0))) {
                (Some(_), Some(flow)) => bid_proceeds = bid_proceeds.saturating_add(flow),
                _ => bid_ok = false,
            }
        }
        Some(CompositeBbo {
            bid_proceeds_quote_minor_per_unit: bid_ok.then_some(bid_proceeds),
            ask_cost_quote_minor_per_unit: ask_ok.then_some(ask_cost),
        })
    }
}

/// Scale a reservation from `open_before` to `open_after` units.
pub(crate) fn scale_reservation(old: u128, open_before: u64, open_after: u64) -> u128 {
    if open_before == 0 || old == 0 {
        return 0;
    }
    mul_div(
        old,
        u128::from(open_after),
        u128::from(open_before),
        poc_core::Rounding::Floor,
    )
    .unwrap_or(0)
}
