//! The tick sweep: everything time drives, in one deterministic pass.
//!
//! Ordering within a tick (each stage plans against the pre-tick
//! snapshot; applications happen in the same order, so the journal is
//! exact even when stages interact):
//!
//! 1. **Halt detection** — oracle quorum lost/resumed per underlying.
//! 2. **Stop triggers** — parked stop orders whose trigger the mark
//!    crossed are activated through the standard match path.
//! 3. **GTD expiry** — resting orders past their date leave the book.
//! 4. **Funding** — perp intervals settle from mark/index TWAPs.
//! 5. **Option expiry** — 30-minute TWAP settlement, positions cash out,
//!    instruments delist.
//! 6. **Liquidity scoring & rewards** — maker observations accumulate;
//!    reward intervals close pro-rata from the pool.
//! 7. **Liquidation cascade** — under-margined accounts are closed:
//!    book first, insurance second, ADL last.

use std::collections::BTreeMap;

use poc_core::{to_i128, Instrument, Order, OrderType, Side, SubaccountId, Symbol, TimestampMs};
use poc_economics::FundingCalculator;
use poc_margin::{MarginAccount, MarkSet};
use poc_risk::{AdlCandidate, AdlRanking, LiquidationCandidate};

use crate::engine::Engine;
use crate::event::{
    Event, FundingPaid, FundingSettled, LiquidityObservation, OptionSettled, OrderCloseReason,
    RewardPaid,
};

/// Plan the entire tick. Pure over the pre-tick snapshot.
pub(crate) fn plan_tick(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    let mut events = Vec::new();

    // The clock event anchors replay: without it, tick-derived events
    // without their own timestamps would leave the replayed clock behind
    // the live one.
    events.push(Event::ClockAdvanced { now });

    // 1. Halt detection (fail-safe on quorum loss, resume on recovery).
    for (base, oracle) in &engine.oracles {
        let live = oracle.mark(now).is_some();
        let halted = engine.halted.get(base).copied().unwrap_or(false);
        if !live && !halted {
            events.push(Event::MarketHalted {
                base_symbol: base.clone(),
                ts: now,
            });
        } else if live && halted {
            events.push(Event::MarketResumed {
                base_symbol: base.clone(),
                ts: now,
            });
        }
    }

    let marks = engine.build_marks(now);

    // 2. Stop triggers.
    if let Some(marks) = &marks {
        events.extend(plan_stop_triggers(engine, now, marks));
    }

    // 3. GTD expiry.
    events.extend(plan_gtd_expiry(engine, now));

    // 4. Funding intervals.
    events.extend(plan_funding(engine, now, &marks));

    // 5. Option expiry.
    events.extend(plan_option_expiry(engine, now, &marks));

    // 6. Liquidity scoring and rewards.
    events.extend(plan_liquidity(engine, now, &marks));

    // 7. Liquidation cascade (needs live marks; skipped while halted).
    if let Some(marks) = &marks {
        events.extend(plan_liquidations(engine, now, marks));
    }

    events
}

// ----------------------------------------------------------------------
// Stop triggers
// ----------------------------------------------------------------------

fn plan_stop_triggers(
    engine: &Engine,
    now: TimestampMs,
    marks: &BTreeMap<String, MarkSet>,
) -> Vec<Event> {
    let mut events = Vec::new();
    for stop in engine.stop_orders.values() {
        let Some(instrument) = engine.instruments.get(&stop.symbol) else {
            continue;
        };
        let Some(set) = marks.get(instrument.base_symbol()) else {
            continue;
        };
        let mark = match instrument {
            Instrument::Perp(_) => set.spot_quote_minor_per_base,
            Instrument::Option(m) => match set.marks.get(&m.symbol) {
                Some(poc_margin::Mark::Option {
                    premium_quote_minor_per_base,
                    ..
                }) => *premium_quote_minor_per_base,
                _ => continue,
            },
        };
        let (trigger, limit_price) = match stop.order_type {
            OrderType::StopMarket { trigger_price } => (trigger_price, None),
            OrderType::StopLimit {
                trigger_price,
                limit_price,
            } => (trigger_price, Some(limit_price)),
            _ => continue,
        };
        // Convert the trigger (ticks) to quote-minor for comparison.
        let Some(trigger_quote) = instrument.price_quote_minor(trigger) else {
            continue;
        };
        let crossed = match stop.side {
            Side::Bid => mark >= trigger_quote, // stop-buy arms upward
            Side::Ask => mark <= trigger_quote, // stop-sell arms downward
        };
        if !crossed {
            continue;
        }
        // Activate: same id, now a plain market/limit order.
        let mut activated = stop.clone();
        activated.order_type = match limit_price {
            Some(p) => {
                activated.price_ticks = Some(p);
                OrderType::Limit
            }
            None => OrderType::Market,
        };
        activated.engine_ts = now;
        events.extend(engine.plan_match(&activated, now, 0));
    }
    events
}

// ----------------------------------------------------------------------
// GTD expiry
// ----------------------------------------------------------------------

fn plan_gtd_expiry(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    let mut events = Vec::new();
    for (symbol, book) in &engine.books {
        for resting in book.resting_orders() {
            if let poc_core::TimeInForce::Gtd(expiry) = resting.order.tif {
                if expiry <= now {
                    events.push(Event::OrderClosed {
                        order_id: resting.order.id,
                        subaccount: resting.order.subaccount,
                        symbol: symbol.clone(),
                        order: resting.order.clone(),
                        reason: OrderCloseReason::Expired,
                    });
                }
            }
        }
    }
    events
}

// ----------------------------------------------------------------------
// Funding
// ----------------------------------------------------------------------

fn plan_funding(
    engine: &Engine,
    now: TimestampMs,
    marks: &Option<BTreeMap<String, MarkSet>>,
) -> Vec<Event> {
    let mut events = Vec::new();
    for (symbol, instrument) in &engine.instruments {
        let Instrument::Perp(market) = instrument else {
            continue;
        };
        let Some(&due) = engine.next_funding_ts.get(symbol) else {
            continue;
        };
        if now < due {
            continue;
        }
        let interval = market.funding.interval_ms;

        // Index TWAP from the oracle; mark TWAP from BBO-mid samples.
        let index_twap = engine
            .oracles
            .get(instrument.base_symbol())
            .and_then(|o| o.twap(now, interval));
        let mark_twap = engine.mark_twap(symbol, now, interval).or(index_twap);
        let (Some(index_twap), Some(mark_twap)) = (index_twap, mark_twap) else {
            continue;
        };
        let Some(quote) = FundingCalculator::perp_funding(mark_twap, index_twap, &market.funding)
        else {
            continue;
        };

        let spot_now = marks
            .as_ref()
            .and_then(|m| m.get(instrument.base_symbol()))
            .map(|s| s.spot_quote_minor_per_base)
            .unwrap_or(index_twap);
        let per_lot = FundingCalculator::payment_per_lot(
            quote.rate_bps,
            spot_now,
            market.lot_size_base_minor,
            market.base_decimals,
        );
        let Some(per_lot) = per_lot else {
            continue;
        };

        events.push(Event::Funding(Box::new(FundingSettled {
            symbol: symbol.clone(),
            rate_bps: quote.rate_bps,
            ts: now,
        })));

        // Longs pay shorts when the rate is positive: credit = -lots × per_lot.
        for (sub, account) in &engine.accounts {
            let lots = account.lots_of(symbol);
            if lots == 0 || per_lot == 0 {
                continue;
            }
            let credit = -(i128::from(lots) * per_lot);
            events.push(Event::FundingFlow(Box::new(FundingPaid {
                subaccount: *sub,
                symbol: symbol.clone(),
                credit_quote_minor: credit,
            })));
        }
    }
    events
}

// ----------------------------------------------------------------------
// Option expiry
// ----------------------------------------------------------------------

fn plan_option_expiry(
    engine: &Engine,
    now: TimestampMs,
    _marks: &Option<BTreeMap<String, MarkSet>>,
) -> Vec<Event> {
    let mut events = Vec::new();
    let expired: Vec<Symbol> = engine
        .instruments
        .values()
        .filter_map(|i| match i {
            Instrument::Option(m) if m.expiry_ts_ms <= now => Some(m.symbol.clone()),
            _ => None,
        })
        .collect();

    for symbol in expired {
        let Some(Instrument::Option(market)) = engine.instruments.get(&symbol).cloned() else {
            continue;
        };
        // Cash settlement on the 30-minute TWAP ending at expiry.
        let settlement = engine
            .oracles
            .get(&market.base_symbol)
            .and_then(|o| o.twap(market.expiry_ts_ms, 30 * 60 * 1000))
            .or_else(|| {
                engine
                    .oracles
                    .get(&market.base_symbol)
                    .and_then(|o| o.last_mark())
            });
        let Some(settlement) = settlement else {
            continue; // fail-safe: no settlement without a defensible price
        };
        let instrument = Instrument::Option(market.clone());

        for (sub, account) in &engine.accounts {
            let lots = account.lots_of(&symbol);
            if lots == 0 {
                continue;
            }
            // Intrinsic per lot (quote minor), signed by position side.
            let intrinsic_per_lot = instrument
                .option_intrinsic_quote_minor(&market, settlement)
                .unwrap_or(0);
            let payout = to_i128(intrinsic_per_lot).saturating_mul(i128::from(lots));
            events.push(Event::OptionExpiry(Box::new(OptionSettled {
                subaccount: *sub,
                symbol: symbol.clone(),
                signed_lots: lots,
                settlement_quote_minor: settlement,
                payout_quote_minor: payout,
            })));
        }
        events.push(Event::OptionDelisted { symbol });
    }
    events
}

// ----------------------------------------------------------------------
// Liquidity scoring and rewards
// ----------------------------------------------------------------------

fn plan_liquidity(
    engine: &Engine,
    now: TimestampMs,
    marks: &Option<BTreeMap<String, MarkSet>>,
) -> Vec<Event> {
    let mut events = Vec::new();
    // Score every qualifying resting quote.
    let mut observations: Vec<LiquidityObservation> = Vec::new();
    if let Some(marks) = marks {
        for (symbol, instrument) in &engine.instruments {
            let Some(book) = engine.books.get(symbol) else {
                continue;
            };
            let Some(set) = marks.get(instrument.base_symbol()) else {
                continue;
            };
            let mid_quote = match instrument {
                Instrument::Perp(_) => Some(set.spot_quote_minor_per_base),
                Instrument::Option(m) => match set.marks.get(&m.symbol) {
                    Some(poc_margin::Mark::Option {
                        premium_quote_minor_per_base,
                        ..
                    }) => Some(*premium_quote_minor_per_base),
                    _ => None,
                },
            };
            let Some(mid) = mid_quote else { continue };
            if mid == 0 {
                continue;
            }
            for resting in book.resting_orders() {
                let Some(price_quote) = resting
                    .order
                    .price_ticks
                    .and_then(|t| instrument.price_quote_minor(t))
                else {
                    continue;
                };
                let diff = price_quote.abs_diff(mid);
                let spread_bps =
                    u64::try_from(diff.saturating_mul(10_000) / mid).unwrap_or(u64::MAX);
                // Two-sided: the account quotes both sides within the band.
                let has_other_side = book.resting_orders().any(|other| {
                    other.order.subaccount == resting.order.subaccount
                        && other.order.side != resting.order.side
                });
                observations.push(LiquidityObservation {
                    subaccount: resting.order.subaccount,
                    size_lots: resting.order.open_qty(),
                    spread_bps,
                    two_sided: has_other_side,
                });
            }
        }
    }
    if !observations.is_empty() {
        events.push(Event::LiquidityScored {
            observations: observations.clone(),
        });
    }

    // Reward boundary: settle on clones that include this tick's
    // observations (they are applied via the `LiquidityScored` event just
    // before the `Reward`/`RewardsSettled` events, so the live settle at
    // apply time sees the identical scores).
    if now >= engine.next_reward_ts {
        let mut incentives = engine.incentives.clone();
        for obs in &observations {
            incentives.on_observation(obs.subaccount, obs.size_lots, obs.spread_bps, obs.two_sided);
        }
        let mut pool = engine.reward_pool.clone();
        let settlement = incentives.settle(&mut pool);
        for (sub, amount) in settlement.payments {
            if amount > 0 {
                events.push(Event::Reward(Box::new(RewardPaid {
                    subaccount: sub,
                    amount_quote_minor: amount,
                })));
            }
        }
        events.push(Event::RewardsSettled);
    }
    events
}

// ----------------------------------------------------------------------
// Liquidation cascade
// ----------------------------------------------------------------------

fn plan_liquidations(
    engine: &Engine,
    now: TimestampMs,
    marks: &BTreeMap<String, MarkSet>,
) -> Vec<Event> {
    let mut events = Vec::new();

    // Candidates, most severe deficit first (deterministic order).
    let mut candidates: Vec<LiquidationCandidate> = Vec::new();
    for (sub, account) in &engine.accounts {
        if let Some(summary) =
            engine
                .margin_engine
                .margin_summary(account, &engine.instruments, marks)
        {
            if summary.equity_quote_minor < to_i128(summary.maintenance_quote_minor) {
                candidates.push(LiquidationCandidate {
                    subaccount: *sub,
                    equity_quote_minor: summary.equity_quote_minor,
                    maintenance_quote_minor: summary.maintenance_quote_minor,
                });
            }
        }
    }
    if candidates.is_empty() {
        return events;
    }
    candidates.sort_by(|a, b| {
        b.deficit()
            .cmp(&a.deficit())
            .then(a.subaccount.cmp(&b.subaccount))
    });

    let mut touched: Vec<SubaccountId> = Vec::new();
    let mut synthetic_id = engine.next_order_id;
    for candidate in candidates {
        if touched.contains(&candidate.subaccount) {
            continue; // already liquidated in this tick's batch
        }
        let Some(account) = engine.accounts.get(&candidate.subaccount) else {
            continue;
        };
        let Some(plan) = engine.liq_planner.plan(
            candidate.subaccount,
            account,
            &engine.instruments,
            marks,
            &engine.margin_engine,
        ) else {
            continue;
        };
        touched.push(candidate.subaccount);

        // Track this candidate's executions to simulate the final equity.
        let mut sim = account.clone();
        let mut deficit_estimate = 0_u128;
        if plan.bankrupt {
            deficit_estimate = (-plan.projected_equity_after).unsigned_abs();
        }

        for action in &plan.actions {
            let Some(instrument) = engine.instruments.get(&action.symbol).cloned() else {
                continue;
            };
            if !engine.books.contains_key(&action.symbol) {
                continue;
            }
            let Some(set) = marks.get(instrument.base_symbol()) else {
                continue;
            };
            let mark_price = match &instrument {
                Instrument::Perp(_) => set.spot_quote_minor_per_base,
                Instrument::Option(m) => match set.marks.get(&m.symbol) {
                    Some(poc_margin::Mark::Option {
                        premium_quote_minor_per_base,
                        ..
                    }) => *premium_quote_minor_per_base,
                    _ => continue,
                },
            };

            // Phase A: cross the book as an aggressive IOC taker whose
            // limit is the penalized price (fills only at better prices).
            let penalized_ticks =
                instrument.ticks_from_quote_minor(action.penalized_price_quote_minor);
            let mut filled_on_book = 0_u64;
            if let Some(ticks) = penalized_ticks {
                let taker = Order {
                    id: synthetic_id,
                    subaccount: candidate.subaccount,
                    symbol: action.symbol.clone(),
                    side: action.closing_side,
                    order_type: OrderType::Limit,
                    price_ticks: Some(ticks),
                    qty_lots: action.lots,
                    filled_lots: 0,
                    tif: poc_core::TimeInForce::Ioc,
                    post_only: false,
                    reduce_only: true,
                    stp: poc_core::SelfTradePrevention::CancelOldest,
                    client_ts: now,
                    engine_ts: now,
                };
                synthetic_id += 1;
                let batch_start = events.len();
                events.extend(engine.plan_match(&taker, now, 0));
                for event in &events[batch_start..] {
                    if let Event::TradeExecuted(t) = event {
                        if t.taker_order_id == taker.id {
                            filled_on_board_track(&mut sim, &instrument, t);
                            filled_on_book += t.qty_lots;
                        }
                    }
                }
            }
            let remainder = action.lots.saturating_sub(filled_on_book);

            // Phase B: the uninsurable (bankrupt + exhausted fund) path
            // routes the remainder through ADL at the bankruptcy price;
            // otherwise the insurance fund is the buyer of last resort at
            // the penalized price.
            let uninsurable =
                deficit_estimate > 0 && !engine.insurance.can_absorb(deficit_estimate);

            if remainder == 0 {
                continue;
            }

            if uninsurable {
                let base_per_lot = lot_base(&instrument);
                let adl_price = bankruptcy_price(
                    action.penalized_price_quote_minor,
                    deficit_estimate,
                    remainder,
                    base_per_lot,
                    action.closing_side,
                );
                let mut remaining = remainder;
                let opposite_holders: Vec<AdlCandidate> = engine
                    .accounts
                    .iter()
                    .filter(|(&sub, _)| sub != candidate.subaccount && !touched.contains(&sub))
                    .filter_map(|(&sub, acct)| {
                        let lots = acct.lots_of(&action.symbol);
                        if lots == 0 || lots.signum() == action.closing_side.sign() {
                            return None;
                        }
                        let entry = acct
                            .position(&action.symbol)
                            .map(|p| p.avg_entry_quote_minor)
                            .unwrap_or(0);
                        Some(AdlCandidate {
                            subaccount: sub,
                            symbol: action.symbol.clone(),
                            signed_lots: lots,
                            entry_quote_minor: entry,
                            mark_quote_minor: mark_price,
                        })
                    })
                    .collect();
                for c in AdlRanking::rank(opposite_holders) {
                    if remaining == 0 {
                        break;
                    }
                    let lots = c.signed_lots.unsigned_abs().min(remaining);
                    events.push(Event::Adl(Box::new(crate::event::AdlExecuted {
                        liquidated_subaccount: candidate.subaccount,
                        counterparty_subaccount: c.subaccount,
                        symbol: action.symbol.clone(),
                        lots,
                        price_quote_minor: adl_price,
                        closing_side_is_ask: action.closing_side == Side::Ask,
                    })));
                    sim.apply_fill(
                        &instrument,
                        &action.symbol,
                        action.closing_side,
                        lots,
                        adl_price,
                    );
                    remaining -= lots;
                    touched.push(c.subaccount);
                }
                // Anything left after ADL falls to the insurance fund
                // (possibly driving it negative — explicit venue debt).
                if remaining > 0 {
                    events.push(Event::Liquidation(Box::new(
                        crate::event::LiquidationExecuted {
                            subaccount: candidate.subaccount,
                            symbol: action.symbol.clone(),
                            lots: remaining,
                            price_quote_minor: action.penalized_price_quote_minor,
                            to_insurance: true,
                            penalty_quote_minor: penalty_of(
                                mark_price,
                                action.penalized_price_quote_minor,
                                remaining,
                                &instrument,
                            ),
                            absorbed_quote_minor: 0,
                            closing_side_is_ask: action.closing_side == Side::Ask,
                        },
                    )));
                    sim.apply_fill(
                        &instrument,
                        &action.symbol,
                        action.closing_side,
                        remaining,
                        action.penalized_price_quote_minor,
                    );
                }
            } else {
                events.push(Event::Liquidation(Box::new(
                    crate::event::LiquidationExecuted {
                        subaccount: candidate.subaccount,
                        symbol: action.symbol.clone(),
                        lots: remainder,
                        price_quote_minor: action.penalized_price_quote_minor,
                        to_insurance: true,
                        penalty_quote_minor: penalty_of(
                            mark_price,
                            action.penalized_price_quote_minor,
                            remainder,
                            &instrument,
                        ),
                        absorbed_quote_minor: 0,
                        closing_side_is_ask: action.closing_side == Side::Ask,
                    },
                )));
                sim.apply_fill(
                    &instrument,
                    &action.symbol,
                    action.closing_side,
                    remainder,
                    action.penalized_price_quote_minor,
                );
            }
        }

        // Final bankruptcy settlement: whatever the simulations leave
        // below zero is absorbed by the insurance fund (exact, not the
        // planner's projection).
        if let Some(final_summary) =
            engine
                .margin_engine
                .margin_summary(&sim, &engine.instruments, marks)
        {
            if final_summary.equity_quote_minor < 0 {
                events.push(Event::Liquidation(Box::new(
                    crate::event::LiquidationExecuted {
                        subaccount: candidate.subaccount,
                        symbol: "".into(),
                        lots: 0,
                        price_quote_minor: 0,
                        to_insurance: false,
                        penalty_quote_minor: 0,
                        absorbed_quote_minor: (-final_summary.equity_quote_minor).unsigned_abs(),
                        closing_side_is_ask: false,
                    },
                )));
            }
        }
    }
    events
}

/// Feed a planned trade into the liquidation simulation account.
fn filled_on_board_track(
    sim: &mut MarginAccount,
    instrument: &Instrument,
    trade: &crate::event::Trade,
) {
    let price = instrument.price_quote_minor(trade.price_ticks).unwrap_or(0);
    sim.apply_fill(
        instrument,
        &trade.symbol,
        trade.maker_side.opposite(),
        trade.qty_lots,
        price,
    );
    sim.apply_fee(trade.taker_fee_quote_minor);
}

/// Base units per one lot.
fn lot_base(instrument: &Instrument) -> u128 {
    let base_unit = 10_u128.checked_pow(instrument.base_decimals()).unwrap_or(1);
    instrument.lot_size() / base_unit.max(1)
}

/// The bankruptcy price: penalized price adjusted so closing the
/// remaining `lots` at it covers the account deficit exactly.
fn bankruptcy_price(
    penalized: u128,
    deficit: u128,
    lots: u64,
    base_per_lot: u128,
    closing_side: Side,
) -> u128 {
    let base_qty = u128::from(lots).saturating_mul(base_per_lot.max(1));
    let bump = if base_qty == 0 {
        0
    } else {
        poc_core::mul_div(deficit, 1, base_qty, poc_core::Rounding::Ceil).unwrap_or(0)
    };
    match closing_side {
        Side::Ask => penalized.saturating_add(bump),
        Side::Bid => penalized.saturating_sub(bump).max(1),
    }
}

/// Penalty value of an insurance closure: |mark − price| × lots, in
/// quote minor.
fn penalty_of(mark: u128, price: u128, lots: u64, instrument: &Instrument) -> u128 {
    let per_base = mark.abs_diff(price);
    let base_unit = 10_u128.checked_pow(instrument.base_decimals()).unwrap_or(1);
    poc_core::mul_div(
        per_base,
        u128::from(lots).saturating_mul(instrument.lot_size()),
        base_unit,
        poc_core::Rounding::Ceil,
    )
    .unwrap_or(0)
}
