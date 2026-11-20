//! The best-execution router (G-11 depth): `RfqExecuteBest` prices the
//! RFQ's best live quote AND the composite book route — per-leg market
//! sweeps of the RFQ's remaining size, ALL-OR-NOTHING per leg, taker
//! fees included, margin-gated, cost-collared by the RFQ's own bounds
//! (mirroring `plan_take`'s `cost_within_bounds`) — and executes the
//! cheaper route NET OF FEES, journaling a `BestExecutionReport` with
//! both prices.
//!
//! The book route settles as ordinary `TradeExecuted` events against
//! real resting makers (makers earn maker fees, the taker pays taker
//! fees) followed by `RfqBookSwept`, which consumes the RFQ's remaining
//! size exactly like a quote execution (`RfqBook::consume_external_take`).
//! The quote route reuses the negotiated rails (`plan_rfq_execute`).

use poc_core::{Instrument, Side, SubaccountId, Symbol, TimestampMs};

use crate::engine::Engine;
use crate::event::Event;
use crate::institutions;

/// One priced route for an RFQ take.
struct PricedBookRoute {
    /// Per-leg pure match outcomes (in the RFQ's leg order).
    outcomes: Vec<poc_orderbook::MatchOutcome>,
    /// Lots taken per leg (= the RFQ's remaining per leg).
    take: Vec<u64>,
    /// Gross cost (positive = the taker pays), fees excluded.
    gross_cost_quote_minor: i128,
    /// Taker fees across all leg fills.
    taker_fees_quote_minor: i128,
}

/// Plan the best-execution route for one RFQ (pure over the snapshot).
pub(crate) fn plan_rfq_execute_best(
    engine: &Engine,
    taker: SubaccountId,
    rfq_id: u64,
    now: TimestampMs,
) -> Vec<Event> {
    let Some(rfq) = engine.rfq_book.rfq(rfq_id) else {
        return vec![Event::RfqRejected {
            subaccount: taker,
            reason: "unknown rfq",
            ts: now,
        }];
    };
    if rfq.status != poc_rfq::RfqStatus::Open {
        return vec![Event::RfqRejected {
            subaccount: taker,
            reason: "rfq not open",
            ts: now,
        }];
    }
    if rfq.taker != taker {
        return vec![Event::RfqRejected {
            subaccount: taker,
            reason: "not rfq owner",
            ts: now,
        }];
    }
    if now > rfq.valid_until {
        return vec![Event::RfqRejected {
            subaccount: taker,
            reason: "quote window closed",
            ts: now,
        }];
    }
    let take: Vec<u64> = rfq.remaining_lots.clone();
    if take.iter().all(|&t| t == 0) {
        return vec![Event::RfqRejected {
            subaccount: taker,
            reason: "rfq already filled",
            ts: now,
        }];
    }

    // --- Price the composite book route (all-or-nothing per leg).
    let book_route = price_book_route(engine, taker, rfq, &take, now);

    // --- Price the live quotes that can cover the FULL take.
    let mut best_quote: Option<(
        u64,  /*quote_id*/
        u64,  /*maker*/
        i128, /*cost*/
        i128, /*fees*/
    )> = None;
    for quote_id in engine
        .rfq_book
        .live_quotes(rfq_id)
        .iter()
        .map(|q| q.quote_id)
    {
        let Some(quote) = engine.rfq_book.quote(quote_id) else {
            continue;
        };
        if quote.maker == taker || now > quote.valid_until {
            continue;
        }
        let rem = quote.remaining_lots();
        // Full coverage required for the head-to-head comparison.
        if rem.iter().zip(&take).any(|(&q, &r)| q < r) {
            continue;
        }
        // plan_take prices exactly the executable set (ownership,
        // window, sizes, and the RFQ's own bounds enforced).
        let Ok(planned) =
            engine
                .rfq_book_ref()
                .plan_take(taker, rfq_id, quote_id, Some(&take), now)
        else {
            continue;
        };
        let cost = planned
            .total_taker_flow_quote_minor
            .checked_neg()
            .unwrap_or(0);
        let fees: i128 = institutions::rfq_taker_fees(engine, taker, &planned.legs)
            .iter()
            .sum();
        let net = cost.saturating_add(fees);
        if best_quote.as_ref().map_or(true, |(_, _, _, b)| net < *b) {
            best_quote = Some((quote_id, quote.maker, cost, fees));
        }
    }

    // --- Decide.
    let quote_net = best_quote.map(|(_, _, cost, fees)| cost.saturating_add(fees));
    let book_net = book_route.as_ref().map(|r| {
        r.gross_cost_quote_minor
            .saturating_add(r.taker_fees_quote_minor)
    });
    let chosen: &'static str = match (quote_net, book_net) {
        (Some(q), Some(b)) if b < q => "book",
        (Some(_), Some(_)) => "quote",
        (None, Some(_)) => "book",
        (Some(_), None) => "quote",
        (None, None) => {
            // No full-cover quote and no feasible book route: fall back
            // to the best partial quote (progress beats rejection), or
            // reject when nothing is quotable.
            let partial = best_partial_quote(engine, taker, rfq_id, &take, now);
            return match partial {
                Some(quote_id) => {
                    institutions::plan_rfq_execute(engine, taker, rfq_id, quote_id, None, now)
                }
                None => vec![Event::RfqRejected {
                    subaccount: taker,
                    reason: "no execution route",
                    ts: now,
                }],
            };
        }
    };

    let mut events = vec![Event::BestExecutionReport {
        rfq_id,
        taker,
        quote_cost_quote_minor: quote_net,
        book_cost_quote_minor: book_net,
        chosen,
        ts: now,
    }];
    match chosen {
        "quote" => {
            let Some((quote_id, _, _, _)) = best_quote else {
                return vec![Event::RfqRejected {
                    subaccount: taker,
                    reason: "no execution route",
                    ts: now,
                }];
            };
            events.extend(institutions::plan_rfq_execute(
                engine, taker, rfq_id, quote_id, None, now,
            ));
        }
        _ => {
            let Some(route) = book_route else {
                return vec![Event::RfqRejected {
                    subaccount: taker,
                    reason: "no execution route",
                    ts: now,
                }];
            };
            // Margin gate (maintenance rule, both directions are the
            // same taker here).
            let mut margin_legs: Vec<(Symbol, Side, u64, u64)> = Vec::new();
            for (i, leg) in rfq.legs.iter().enumerate() {
                let filled: u64 = route.outcomes[i].fills.iter().map(|f| f.qty_lots).sum();
                if filled == 0 {
                    continue;
                }
                let weighted: u64 = route.outcomes[i]
                    .fills
                    .iter()
                    .map(|f| f.price_ticks.saturating_mul(f.qty_lots))
                    .sum();
                margin_legs.push((leg.symbol.clone(), leg.side, filled, weighted / filled));
            }
            let marks = engine.build_marks(now);
            let margin_ok = marks.as_ref().is_some_and(|m| {
                let Some(sim) = engine.account(taker).cloned() else {
                    return false;
                };
                let mut sim = sim;
                for (symbol, side, qty, price_ticks) in &margin_legs {
                    let Some(instrument) = engine.instruments().get(symbol).cloned() else {
                        return false;
                    };
                    let Some(price) = instrument.price_quote_minor(*price_ticks) else {
                        return false;
                    };
                    sim.apply_fill(&instrument, symbol, *side, *qty, price);
                }
                engine
                    .margin_engine_ref()
                    .margin_summary(&sim, engine.instruments(), m)
                    .is_some_and(|s| {
                        s.equity_quote_minor
                            >= i128::try_from(s.maintenance_quote_minor).unwrap_or(i128::MAX)
                    })
            });
            if !margin_ok {
                // The cheaper route is unaffordable: fall back to the
                // quote when one exists, else reject.
                if let Some((quote_id, _, _, _)) = best_quote {
                    events.push(Event::BestExecutionReport {
                        rfq_id,
                        taker,
                        quote_cost_quote_minor: quote_net,
                        book_cost_quote_minor: book_net,
                        chosen: "quote",
                        ts: now,
                    });
                    events.extend(institutions::plan_rfq_execute(
                        engine, taker, rfq_id, quote_id, None, now,
                    ));
                    return events;
                }
                return vec![Event::RfqRejected {
                    subaccount: taker,
                    reason: "insufficient margin for book route",
                    ts: now,
                }];
            }
            // Emit: ordinary trades against real makers, then the sweep.
            let mut seq = engine.seq_ref();
            for (i, leg) in rfq.legs.iter().enumerate() {
                let Some(instrument) = engine.instruments().get(&leg.symbol).cloned() else {
                    continue;
                };
                for fill in &route.outcomes[i].fills {
                    let notional = instrument
                        .notional_quote_minor(fill.price_ticks, fill.qty_lots)
                        .unwrap_or(0);
                    let taker_fee =
                        taker_fee_for(engine, &instrument, taker, fill.price_ticks, fill.qty_lots);
                    let maker_fee = maker_fee_for(
                        engine,
                        &instrument,
                        fill.maker_subaccount,
                        fill.price_ticks,
                        fill.qty_lots,
                    );
                    seq = seq.saturating_add(1);
                    events.push(Event::TradeExecuted(Box::new(crate::event::Trade {
                        seq,
                        symbol: leg.symbol.clone(),
                        taker_order_id: 0,
                        maker_order_id: fill.maker_order_id,
                        taker_subaccount: taker,
                        maker_subaccount: fill.maker_subaccount,
                        maker_side: fill.maker_side,
                        price_ticks: fill.price_ticks,
                        qty_lots: fill.qty_lots,
                        notional_quote_minor: notional,
                        taker_fee_quote_minor: taker_fee,
                        maker_fee_quote_minor: maker_fee,
                        ts: now,
                    })));
                }
                // Exhausted makers leave the book.
                let mut seen = std::collections::BTreeSet::new();
                for fill in &route.outcomes[i].fills {
                    if !seen.insert(fill.maker_order_id) {
                        continue;
                    }
                    if let Some(resting) = engine
                        .books
                        .get(&leg.symbol)
                        .and_then(|b| b.get(fill.maker_order_id))
                    {
                        if resting.order.open_qty() == fill.qty_lots {
                            let mut final_order = resting.order.clone();
                            final_order.filled_lots = final_order.qty_lots;
                            events.push(Event::OrderClosed {
                                order_id: fill.maker_order_id,
                                subaccount: fill.maker_subaccount,
                                symbol: leg.symbol.clone(),
                                order: final_order,
                                reason: crate::event::OrderCloseReason::Filled,
                            });
                        }
                    }
                }
                if !route.outcomes[i].stp.canceled_makers.is_empty() {
                    events.push(Event::StpCancels {
                        maker_ids: route.outcomes[i].stp.canceled_makers.clone(),
                        ts: now,
                    });
                }
            }
            events.push(Event::RfqBookSwept {
                rfq_id,
                taker,
                take_per_leg: route.take.clone(),
                ts: now,
            });
        }
    }
    events
}

/// The best live quote even when it only partially covers the take.
fn best_partial_quote(
    engine: &Engine,
    taker: SubaccountId,
    rfq_id: u64,
    take: &[u64],
    now: TimestampMs,
) -> Option<u64> {
    let mut best: Option<(u64, i128)> = None;
    for quote in engine.rfq_book.live_quotes(rfq_id) {
        if quote.maker == taker || now > quote.valid_until {
            continue;
        }
        let rem = quote.remaining_lots();
        let cover: Vec<u64> = rem.iter().zip(take).map(|(&q, &r)| q.min(r)).collect();
        if cover.iter().all(|&c| c == 0) {
            continue;
        }
        let Ok(planned) =
            engine
                .rfq_book_ref()
                .plan_take(taker, rfq_id, quote.quote_id, Some(&cover), now)
        else {
            continue;
        };
        let cost = planned
            .total_taker_flow_quote_minor
            .checked_neg()
            .unwrap_or(0);
        if best.as_ref().map_or(true, |(_, b)| cost < *b) {
            best = Some((quote.quote_id, cost));
        }
    }
    best.map(|(id, _)| id)
}

/// Price the composite book route: per-leg market sweeps of the full
/// remaining size, all-or-nothing; returns `None` when any leg cannot
/// cover, the RFQ's bounds reject the cost, or the math overflows.
fn price_book_route(
    engine: &Engine,
    taker: SubaccountId,
    rfq: &poc_rfq::RfqRequest,
    take: &[u64],
    now: TimestampMs,
) -> Option<PricedBookRoute> {
    let mut outcomes = Vec::with_capacity(rfq.legs.len());
    let mut gross_cost: i128 = 0;
    let mut taker_fees: i128 = 0;
    for (i, leg) in rfq.legs.iter().enumerate() {
        if take[i] == 0 {
            outcomes.push(poc_orderbook::MatchOutcome {
                fills: Vec::new(),
                stp: poc_orderbook::StpEffects::default(),
                taker_remaining_lots: 0,
            });
            continue;
        }
        let book = engine.books.get(&leg.symbol)?;
        // ALL-OR-NOTHING: the full remaining must be available at any
        // price the bounds will later verify.
        if book.available_within(leg.side, None) < take[i] {
            return None;
        }
        let order =
            crate::combo::synthetic_leg_order(taker, &leg.symbol, leg.side, take[i], u64::MAX, now);
        let outcome = book.match_taker(&order, None, false, order.stp);
        let filled: u64 = outcome.fills.iter().map(|f| f.qty_lots).sum();
        if filled < take[i] {
            return None; // STP consumed part: the route cannot cover
        }
        let instrument = engine.instruments().get(&leg.symbol)?;
        for fill in &outcome.fills {
            let notional = instrument
                .notional_quote_minor(fill.price_ticks, fill.qty_lots)
                .unwrap_or(0);
            // Cost from the TAKER's viewpoint on this leg.
            let leg_cost = match leg.side {
                Side::Bid => i128::try_from(notional).ok()?,
                Side::Ask => i128::try_from(notional).ok()?.checked_neg()?,
            };
            gross_cost = gross_cost.checked_add(leg_cost)?;
            taker_fees = taker_fees.saturating_add(taker_fee_for(
                engine,
                instrument,
                taker,
                fill.price_ticks,
                fill.qty_lots,
            ));
        }
        outcomes.push(outcome);
    }
    // The RFQ's own bounds collar the route exactly like `plan_take` —
    // the signed generalization (a net credit to the taker satisfies
    // any cost cap; a floor on cost still applies).
    let bounds_ok = rfq
        .min_total_cost_quote_minor
        .and_then(|m| i128::try_from(m).ok())
        .map_or(true, |m| gross_cost >= m)
        && rfq
            .max_total_cost_quote_minor
            .and_then(|m| i128::try_from(m).ok())
            .map_or(true, |m| gross_cost <= m);
    if !bounds_ok {
        return None;
    }
    Some(PricedBookRoute {
        outcomes,
        take: take.to_vec(),
        gross_cost_quote_minor: gross_cost,
        taker_fees_quote_minor: taker_fees,
    })
}

fn taker_fee_for(
    engine: &Engine,
    instrument: &Instrument,
    taker: SubaccountId,
    price_ticks: u64,
    qty_lots: u64,
) -> i128 {
    let notional = instrument
        .notional_quote_minor(price_ticks, qty_lots)
        .unwrap_or(0);
    let tier = engine.fee_schedule_ref().tier_for(taker);
    let fee = match instrument {
        Instrument::Option(_) => {
            let caps = engine.config().option_fee_caps;
            let underlying = engine
                .build_marks(engine.now_ref())
                .as_ref()
                .and_then(|ms| ms.get(instrument.base_symbol()))
                .map(|s| s.spot_quote_minor_per_base)
                .unwrap_or(0);
            let notional_u = instrument
                .position_notional_minor(underlying, i64::try_from(qty_lots).unwrap_or(i64::MAX))
                .unwrap_or(0);
            poc_economics::FeeCalculator::option_taker_fee(tier, &caps, notional_u, notional)
                .unwrap_or(0)
        }
        _ => poc_economics::FeeCalculator::taker_fee(tier, notional).unwrap_or(0),
    };
    poc_economics::apply_tier_discount(fee, engine.mm_discount_of(taker))
}

fn maker_fee_for(
    engine: &Engine,
    instrument: &Instrument,
    maker: SubaccountId,
    price_ticks: u64,
    qty_lots: u64,
) -> i128 {
    let notional = instrument
        .notional_quote_minor(price_ticks, qty_lots)
        .unwrap_or(0);
    let tier = engine.fee_schedule_ref().tier_for(maker);
    let fee = match instrument {
        Instrument::Option(_) => {
            let caps = engine.config().option_fee_caps;
            let underlying = engine
                .build_marks(engine.now_ref())
                .as_ref()
                .and_then(|ms| ms.get(instrument.base_symbol()))
                .map(|s| s.spot_quote_minor_per_base)
                .unwrap_or(0);
            let notional_u = instrument
                .position_notional_minor(underlying, i64::try_from(qty_lots).unwrap_or(i64::MAX))
                .unwrap_or(0);
            poc_economics::FeeCalculator::option_maker_fee(tier, &caps, notional_u, notional)
                .unwrap_or(0)
        }
        _ => poc_economics::FeeCalculator::maker_fee(tier, notional).unwrap_or(0),
    };
    poc_economics::apply_tier_discount(fee, engine.mm_discount_of(maker))
}
