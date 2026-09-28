//! The tick sweep: everything time drives, in one deterministic pass.
//!
//! Ordering within a tick (each stage plans against the pre-tick
//! snapshot; applications happen in the same order, so the journal is
//! exact even when stages interact):
//!
//! 1. **Halt detection** — oracle quorum lost/resumed per underlying.
//! 2. **Auction uncross** (G-12) — due auctions print at a uniform price
//!    and resume continuous trading.
//! 3. **Stop & trailing triggers** — parked orders whose trigger the mark
//!    crossed are activated through the standard match path; trailing
//!    extremes tighten first (G-07).
//! 4. **GTD expiry** — resting orders past their date leave the book.
//! 5. **Funding** — perp intervals settle from mark/index TWAPs (impact-
//!    notional sampled marks, G-39); everlasting rolls settle the
//!    premium TWAP (G-01).
//! 6. **Option expiry** — 30-minute TWAP settlement, positions cash out,
//!    instruments delist.
//! 7. **Auto-listing & rebase** (G-34/G-03) — strike grids roll, far
//!    empty strikes delist, out-of-band everlasting strikes migrate.
//! 8. **Liquidity scoring & rewards** — maker observations accumulate;
//!    reward intervals close pro-rata from the pool.
//! 9. **Liquidation cascade** — under-margined accounts are closed:
//!    collateral converted, book first, fund second, iterative ADL last
//!    (G-19).

use std::collections::BTreeMap;

use poc_core::{
    mul_div, to_i128, Instrument, Order, OrderType, Side, SubaccountId, Symbol, TimestampMs,
};
use poc_economics::FundingCalculator;
use poc_margin::{MarginAccount, MarkSet};
use poc_risk::{AdlCandidate, AdlRanking, LiquidationCandidate};

use crate::engine::Engine;
use crate::event::{
    Event, FundingPaid, FundingSettled, LiquidityObservation, OptionSettled, OrderCloseReason,
    RewardPaid,
};
use crate::vaults::plan_vault_epochs;

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

    // 2. Auction uncross (G-12).
    events.extend(crate::auction::plan_auction_uncross(engine, now));

    // 3. Stop & trailing triggers (G-07: extremes tighten first).
    if let Some(marks) = &marks {
        events.extend(plan_trailing_updates(engine, now, marks));
        events.extend(plan_stop_triggers(engine, now, marks));
    }

    // 4. GTD expiry.
    events.extend(plan_gtd_expiry(engine, now));

    // 5. Funding intervals.
    events.extend(plan_funding(engine, now, &marks));

    // 6. Option expiry.
    events.extend(plan_option_expiry(engine, now, &marks));

    // 6b. Auto-listing + everlasting rebase (G-34/G-03).
    events.extend(crate::listing::plan_auto_listing(engine, now));

    // 7. Liquidity scoring and rewards.
    events.extend(plan_liquidity(engine, now, &marks));

    // 7. Volatility surface: observe book touches, govern the marks (G-04).
    if let Some(marks) = &marks {
        events.extend(plan_vol_surface(engine, now, marks));
    }

    // 7b. TWAP slicing (G-10): at most one child per parent per tick.
    events.extend(plan_twap_slices(engine, now));

    // 7c. Volatility index publication (G-05, DVOL-shaped).
    events.extend(plan_vol_index(engine, now));

    // 7d. Collateral interest accrual (G-18, daily boundaries).
    events.extend(plan_collateral_interest(engine, now));

    // 7e. Insurance inventory: mark to market, rebalance drips (G-23).
    if let Some(marks) = &marks {
        events.extend(plan_insurance_inventory(engine, now, marks));
    }

    // 7f. LP vault epochs (G-16).
    events.extend(plan_vault_epochs(engine, now));

    // 8. RFQ + block sweeps: expiries and delayed broadcasts (G-11/G-13).
    events.extend(plan_rfq_sweep(engine, now));
    events.extend(plan_block_sweep(engine, now));

    // 9. Price-dislocation circuit breaker (G-21).
    events.extend(plan_price_breaker(engine, now));

    // 10. Liquidation cascade (needs live marks; skipped while halted or
    // velocity-suspended).
    if let Some(marks) = &marks {
        events.extend(plan_liquidations(engine, now, marks));
    }

    events
}

/// Feed the option books' touches into the governed surface (G-04).
///
/// The plan is pure: observations are journaled as `SurfaceObserved`
/// events and the governance sweep as `SurfaceSwept`, so replay drives
/// the surface through the identical observation sequence — the
/// determinism the rest of the engine already guarantees.
fn plan_vol_surface(
    engine: &Engine,
    now: TimestampMs,
    marks: &BTreeMap<String, MarkSet>,
) -> Vec<Event> {
    let mut events = Vec::new();
    for (symbol, instrument) in &engine.instruments {
        let Instrument::Option(m) = instrument else {
            continue;
        };
        let Some(book) = engine.books.get(symbol) else {
            continue;
        };
        let Some(set) = marks.get(instrument.base_symbol()) else {
            continue;
        };
        let (Some(bid_ticks), Some(ask_ticks)) = book.bbo() else {
            continue;
        };
        let (Some(bid), Some(ask)) = (
            instrument.price_quote_minor(bid_ticks),
            instrument.price_quote_minor(ask_ticks),
        ) else {
            continue;
        };
        let sizes = book.best_touch_sizes();
        events.push(Event::SurfaceObserved(Box::new(
            crate::event::SurfaceObservation {
                symbol: symbol.clone(),
                spot_quote_minor: set.spot_quote_minor_per_base,
                strike_quote_minor: m.strike_quote_minor,
                is_call: matches!(m.kind, poc_core::OptionKind::Call),
                tte_ms: m.tte_ms(now),
                bid_quote_minor: bid,
                ask_quote_minor: ask,
                bid_lots: sizes.0,
                ask_lots: sizes.1,
                ts: now,
            },
        )));
    }
    events.push(Event::SurfaceSwept { now });
    events
}

/// Expire RFQs and quotes past their windows (journaled; apply mutates).
fn plan_rfq_sweep(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    let mut events = Vec::new();
    for id in engine.rfq_book.expired_rfq_ids(now) {
        events.push(Event::RfqClosed {
            rfq_id: id,
            quote_id: None,
            reason: "expired",
        });
    }
    for id in engine.rfq_book.expired_quote_ids(now) {
        events.push(Event::RfqClosed {
            rfq_id: 0,
            quote_id: Some(id),
            reason: "expired",
        });
    }
    events
}

/// Print due blocks to the public tape (delayed broadcast, G-13).
fn plan_block_sweep(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    engine
        .blocks
        .due_ids(now)
        .into_iter()
        .map(|id| Event::BlockPrinted { block_id: id })
        .collect()
}

/// Price-dislocation breaker (G-21): a BBO mid sustained beyond the
/// dislocation band from the oracle mark trips the instrument breaker
/// for a cooldown. The dYdX v4 halt-band shape, deterministic and journaled.
fn plan_price_breaker(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    let mut events = Vec::new();
    let params = &engine.config.breaker;
    for (symbol, instrument) in &engine.instruments {
        let Instrument::Perp(_) = instrument else {
            continue;
        };
        let Some(book) = engine.books.get(symbol) else {
            continue;
        };
        let (Some(bid), Some(ask)) = book.bbo() else {
            continue;
        };
        let Some(bid_q) = instrument.price_quote_minor(bid) else {
            continue;
        };
        let Some(ask_q) = instrument.price_quote_minor(ask) else {
            continue;
        };
        let mid = (bid_q + ask_q) / 2;
        let Some(set) = engine
            .oracles
            .get(instrument.base_symbol())
            .and_then(|o| o.mark(now))
        else {
            continue;
        };
        let diff = mid.abs_diff(set);
        let dislocated = mid > 0
            && poc_core::mul_div(diff, 10_000, mid, poc_core::Rounding::Floor)
                .is_some_and(|bps| bps > u128::from(params.dislocation_bps));
        let blocked = engine.is_breaker_blocked(symbol, now);
        if dislocated && !blocked {
            events.push(Event::BreakerTripped {
                kind: "price-dislocation",
                symbol: symbol.clone(),
                ts: now,
            });
        } else if !dislocated && blocked {
            events.push(Event::BreakerReleased {
                kind: "price-dislocation",
                symbol: symbol.clone(),
                ts: now,
            });
        }
    }
    events
}

// ----------------------------------------------------------------------
// Stop & trailing triggers
// ----------------------------------------------------------------------

/// Update parked trailing stops' running extremes (G-07).
///
/// A buy trailer tracks the running LOW; a sell trailer tracks the
/// running HIGH. Every improvement is journaled as a `TrailingUpdated`
/// event, so replay reconstructs the trigger state exactly. Applied
/// before [`plan_stop_triggers`] in the same tick — the extreme tightens
/// with the mark that crosses it.
fn plan_trailing_updates(
    engine: &Engine,
    now: TimestampMs,
    marks: &BTreeMap<String, MarkSet>,
) -> Vec<Event> {
    let mut events = Vec::new();
    for stop in engine.stop_orders.values() {
        let (offset_ticks, _limit_ticks) = match stop.order_type {
            OrderType::TrailingStopMarket { offset_ticks } => (offset_ticks, None),
            OrderType::TrailingStopLimit {
                offset_ticks,
                limit_ticks,
            } => (offset_ticks, Some(limit_ticks)),
            _ => continue,
        };
        let _ = offset_ticks;
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
        let Some(current) = stop.trailing_extreme_quote_minor else {
            continue;
        };
        let improved = match stop.side {
            Side::Bid => mark < current, // buy trailer: track the low
            Side::Ask => mark > current, // sell trailer: track the high
        };
        if improved {
            events.push(Event::TrailingUpdated {
                order_id: stop.id,
                subaccount: stop.subaccount,
                symbol: stop.symbol.clone(),
                extreme_quote_minor: mark,
                ts: now,
            });
        }
    }
    events
}

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
        // The trigger in quote minor: stops carry a fixed trigger price
        // (ticks); trailing stops derive it from their running extreme
        // and offset (G-07).
        let (trigger, limit_price) = match stop.order_type {
            OrderType::StopMarket { trigger_price } => (trigger_price, None),
            OrderType::StopLimit {
                trigger_price,
                limit_price,
            } => (trigger_price, Some(limit_price)),
            OrderType::TrailingStopMarket { offset_ticks } => {
                let Some(extreme) = stop.trailing_extreme_quote_minor else {
                    continue;
                };
                let Some(offset_quote) =
                    instrument.tick_size().checked_mul(u128::from(offset_ticks))
                else {
                    continue;
                };
                let trigger = match stop.side {
                    // Buy trailer: fires `offset` above the running low.
                    Side::Bid => extreme.saturating_add(offset_quote),
                    // Sell trailer: fires `offset` below the running high.
                    Side::Ask => extreme.saturating_sub(offset_quote).max(1),
                };
                let Some(trigger_ticks) = instrument.ticks_from_quote_minor(trigger) else {
                    continue;
                };
                (trigger_ticks, None)
            }
            OrderType::TrailingStopLimit {
                offset_ticks,
                limit_ticks,
            } => {
                let Some(extreme) = stop.trailing_extreme_quote_minor else {
                    continue;
                };
                let Some(offset_quote) =
                    instrument.tick_size().checked_mul(u128::from(offset_ticks))
                else {
                    continue;
                };
                let trigger = match stop.side {
                    Side::Bid => extreme.saturating_add(offset_quote),
                    Side::Ask => extreme.saturating_sub(offset_quote).max(1),
                };
                let Some(trigger_ticks) = instrument.ticks_from_quote_minor(trigger) else {
                    continue;
                };
                (trigger_ticks, Some(limit_ticks))
            }
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

    // Everlasting options: the roll IS the funding (G-01, Paradigm EO).
    // Longs pay shorts the interval's mark-premium TWAP per lot; the
    // claim never expires, never settles, never delists.
    for (symbol, instrument) in &engine.instruments {
        let Instrument::Option(m) = instrument else {
            continue;
        };
        if m.variant != poc_core::OptionVariant::Everlasting {
            continue;
        }
        let Some(&due) = engine.next_funding_ts.get(symbol) else {
            continue;
        };
        if now < due {
            continue;
        }
        let interval = m.everlasting.interval_ms;
        // Premium TWAP over the interval from book samples; fallback to the
        // current model mark when the book never printed (fail-safe, same
        // convention as the perp index TWAP fallback).
        let premium_twap_per_base = engine.mark_twap(symbol, now, interval).or_else(|| {
            marks
                .as_ref()
                .and_then(|ms| ms.get(instrument.base_symbol()))
                .and_then(|set| set.marks.get(symbol))
                .and_then(|mark| match mark {
                    poc_margin::Mark::Option {
                        premium_quote_minor_per_base,
                        ..
                    } => Some(*premium_quote_minor_per_base),
                    _ => None,
                })
        });
        let Some(premium_twap_per_base) = premium_twap_per_base else {
            continue;
        };
        let Some(base_unit) = 10_u128.checked_pow(m.base_decimals) else {
            continue;
        };
        let per_lot = poc_core::mul_div(
            premium_twap_per_base,
            m.lot_size_base_minor,
            base_unit,
            poc_core::Rounding::NearestHalfUp,
        )
        .unwrap_or(0);
        if per_lot == 0 {
            continue;
        }
        events.push(Event::Funding(Box::new(FundingSettled {
            symbol: symbol.clone(),
            rate_bps: 0, // the roll settles per-lot amounts, not a rate
            ts: now,
        })));
        for (sub, account) in &engine.accounts {
            let lots = account.lots_of(symbol);
            if lots == 0 {
                continue;
            }
            let credit = -(to_i128(per_lot).saturating_mul(i128::from(lots)));
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
            Instrument::Option(m)
                if m.expiry_ts_ms <= now && m.variant == poc_core::OptionVariant::Dated =>
            {
                Some(m.symbol.clone())
            }
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
                    size_lots: resting.visible_qty(),
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
        if let Some(summary) = engine.effective_margin_summary_at(account, marks, now) {
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

        // Liquidate the account's non-quote collateral into spendable
        // quote first (G-17): every unit it holds reduces what the fund
        // or the counterparties must pay for its bankruptcy.
        if let Some(balances) = engine.collateral.get(&candidate.subaccount) {
            for (code, &amount) in balances {
                if amount == 0 {
                    continue;
                }
                let (Some(price), Some(cfg)) = (
                    engine.collateral_price(code, now),
                    engine.collateral_config(code),
                ) else {
                    continue;
                };
                let Some(unit) = 10_u128.checked_pow(cfg.decimals) else {
                    continue;
                };
                let Some(to_quote) =
                    poc_core::mul_div(amount, price, unit, poc_core::Rounding::Floor)
                else {
                    continue;
                };
                if to_quote == 0 {
                    continue;
                }
                events.push(Event::CollateralConversion(Box::new(
                    crate::event::CollateralConverted {
                        subaccount: candidate.subaccount,
                        from: code.clone(),
                        to: "USD".into(),
                        from_amount_minor: amount,
                        to_amount_minor: to_quote,
                        rate_quote_minor_per_unit: price,
                        ts: now,
                    },
                )));
                sim.cash_quote_minor = sim.cash_quote_minor.saturating_add(to_i128(to_quote));
            }
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
                    display_lots: None,
                    trailing_extreme_quote_minor: None,
                    oco_group: None,
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

            // Phase B (G-19): fund-first, then *iterative* ADL. The
            // post-book remainder routes to the insurance fund while it
            // can absorb the account's live deficit; when it cannot, ADL
            // closes counterparties in capped rounds — a fraction of each
            // position per round, the round price recomputed from the
            // remaining deficit — so no single counterparty eats an entire
            // bankruptcy at once (the BitMEX staged-deleveraging shape).
            if remainder == 0 {
                continue;
            }
            let mut remaining = remainder;
            let mut closed_by: BTreeMap<SubaccountId, u64> = BTreeMap::new();
            let mut round = 0_u32;
            let max_rounds = engine.config.adl_max_rounds.max(1);
            while remaining > 0 {
                // Live deficit of the running simulation.
                let deficit_now =
                    match engine
                        .margin_engine
                        .margin_summary(&sim, &engine.instruments, marks)
                    {
                        Some(s) if s.equity_quote_minor < 0 => (-s.equity_quote_minor) as u128,
                        _ => 0,
                    };
                // Fund-first: the buyer of last resort while it can pay.
                if deficit_now == 0 || engine.insurance.can_absorb(deficit_now) {
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
                    remaining = 0;
                    break;
                }
                if round >= max_rounds {
                    break;
                }
                round += 1;
                // This round's counterparties: opposite holders, each
                // capped at `adl_round_bps` of what they still hold.
                let fraction_bps = engine.config.adl_round_bps.clamp(1, 10_000);
                let mut round_take: u64 = 0;
                let mut chunked: Vec<(SubaccountId, u64)> = Vec::new();
                let mut holders: Vec<AdlCandidate> = Vec::new();
                for (&sub, acct) in &engine.accounts {
                    if sub == candidate.subaccount {
                        continue;
                    }
                    let lots = acct.lots_of(&action.symbol);
                    // Counterparties hold the side the closing order
                    // fills against: closing a bankrupt long (Ask) takes
                    // the shorts (their buy-back reduces them).
                    if lots == 0 || lots.signum() != action.closing_side.sign() {
                        continue;
                    }
                    let already = closed_by.get(&sub).copied().unwrap_or(0);
                    let left = lots.unsigned_abs().saturating_sub(already);
                    if left == 0 {
                        continue;
                    }
                    let chunk = poc_core::mul_div(
                        u128::from(left),
                        u128::from(fraction_bps),
                        10_000,
                        poc_core::Rounding::Ceil,
                    )
                    .unwrap_or(u128::from(left))
                    .max(1);
                    let chunk = u64::try_from(chunk).unwrap_or(left).min(left);
                    chunked.push((sub, chunk));
                    round_take = round_take.saturating_add(chunk);
                    let entry = acct
                        .position(&action.symbol)
                        .map(|p| p.avg_entry_quote_minor)
                        .unwrap_or(0);
                    holders.push(AdlCandidate {
                        subaccount: sub,
                        symbol: action.symbol.clone(),
                        signed_lots: lots,
                        entry_quote_minor: entry,
                        mark_quote_minor: mark_price,
                    });
                }
                if round_take == 0 {
                    break;
                }
                round_take = round_take.min(remaining);
                // The round's price covers the live deficit over the
                // round's size (the bankruptcy-price trick, per round).
                let base_per_lot = lot_base(&instrument);
                let adl_price = bankruptcy_price(
                    mark_price,
                    deficit_now,
                    round_take,
                    base_per_lot,
                    action.closing_side,
                );
                for c in AdlRanking::rank(holders) {
                    if round_take == 0 {
                        break;
                    }
                    let allowed = chunked
                        .iter()
                        .find(|(s, _)| *s == c.subaccount)
                        .map_or(0, |&(_, ch)| ch);
                    if allowed == 0 {
                        continue;
                    }
                    let lots = allowed.min(round_take);
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
                    round_take -= lots;
                    remaining = remaining.saturating_sub(lots);
                    *closed_by.entry(c.subaccount).or_insert(0) += lots;
                }
            }
            // Still open after the rounds: the insurance fund takes the
            // tail (possibly driving it negative — explicit venue debt).
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

// ----------------------------------------------------------------------
// TWAP slicing (G-10)
// ----------------------------------------------------------------------

/// Emit at most one child placement per parent per tick. The child is a
/// full [`crate::command::OrderRequest`] journaled inside the marker
/// event; the engine runs it through the ordinary place path after the
/// tick's other effects are applied.
fn plan_twap_slices(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    let mut events = Vec::new();
    for parent in engine.twap_parents.values() {
        if parent.complete() || now < parent.next_slice_ts {
            continue;
        }
        let index = parent.slices_placed;
        let lots = parent.slice_lots(index);
        if lots == 0 {
            continue;
        }
        let request = crate::command::OrderRequest {
            subaccount: parent.subaccount,
            symbol: parent.symbol.clone(),
            side: parent.side,
            order_type: poc_core::OrderType::Limit,
            price_ticks: parent.limit_ticks,
            qty_lots: lots,
            tif: poc_core::TimeInForce::Ioc,
            post_only: false,
            reduce_only: false,
            stp: poc_core::SelfTradePrevention::CancelNewest,
            display_lots: None,
            oco_group: None,
            client_ts: now,
        };
        let slice_index = index;
        events.push(Event::TwapSliced {
            parent_id: parent.parent_id,
            request,
            slice_index,
            ts: now,
        });
        if slice_index + 1 == parent.slices {
            events.push(Event::TwapClosed {
                parent_id: parent.parent_id,
                subaccount: parent.subaccount,
                reason: "completed",
                placed_lots: parent.lots_placed.saturating_add(lots),
                ts: now,
            });
        }
    }
    events
}

// ----------------------------------------------------------------------
// Volatility index (G-05, DVOL-shaped)
// ----------------------------------------------------------------------

/// Integer square root (deterministic; no f64 in the publication path).
fn isqrt(x: u128) -> u128 {
    if x == 0 {
        return 0;
    }
    let mut r = x;
    let mut q = (x >> 1) + 1;
    while q < r {
        r = q;
        q = (x / r + r) / 2;
    }
    r
}

/// Publish a 30-day-shaped volatility index per underlying: a
/// moneyness-weighted variance of the governed mark IVs of the
/// underlying's option markets, expressed in permille (index x 1000).
///
/// This is the honest DVOL simplification: a full variance-strip
/// replication needs a liquid strip across strikes and tenors; a young
/// surface publishes noise, not an index. The weighting is linear in
/// moneyness distance inside the configured band, options contribute
/// their squared IV, and the square root lands in index points
/// (55% vol prints 55_000 permille).
fn plan_vol_index(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    let band = engine.config.vol_index_band_bps.max(1);
    let mut events = Vec::new();
    let bases: Vec<String> = engine
        .oracles
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for base in bases {
        let Some(spot) = engine.oracles.get(&base).and_then(|o| o.mark(now)) else {
            continue;
        };
        let mut weight_variance_sum: u128 = 0;
        let mut weight_sum: u128 = 0;
        for (symbol, instrument) in &engine.instruments {
            let poc_core::Instrument::Option(m) = instrument else {
                continue;
            };
            if m.base_symbol != base {
                continue;
            }
            let Some(mark_iv_bps) = engine.vol_surface.mark_iv_bps(symbol) else {
                continue;
            };
            // Moneyness distance in bps of spot (integer, floored).
            let strike = m.strike_quote_minor;
            let distance_bps = if spot > 0 {
                let diff = strike.abs_diff(spot);
                mul_div(diff, 10_000, spot, poc_core::Rounding::Floor).unwrap_or(0)
            } else {
                continue;
            };
            if distance_bps >= u128::from(band) {
                continue;
            }
            let weight = u128::from(band - u64::try_from(distance_bps).unwrap_or(band)); // linear kernel
            weight_sum = weight_sum.saturating_add(weight);
            weight_variance_sum =
                weight_variance_sum.saturating_add(weight.saturating_mul(
                    u128::from(mark_iv_bps).saturating_mul(u128::from(mark_iv_bps)),
                ));
        }
        if weight_sum == 0 {
            continue;
        }
        let variance_bps2 = weight_variance_sum / weight_sum;
        // Index points: iv in bps -> index in permille is bps * 10.
        let index_permille =
            u64::try_from(isqrt(variance_bps2).saturating_mul(10)).unwrap_or(u64::MAX);
        events.push(Event::VolIndexPublished {
            base_symbol: base,
            index_permille,
            ts: now,
        });
    }
    events
}

// ----------------------------------------------------------------------
// Collateral interest (G-18)
// ----------------------------------------------------------------------

/// Accrue daily interest on *utilized* non-quote collateral at UTC day
/// boundaries: the in-kind charge for margin capacity borrowed in a
/// foreign currency. Utilization is the haircut value the account
/// actually deploys (min of haircut value and maintenance requirement),
/// so idle collateral pays nothing.
fn plan_collateral_interest(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    if engine.config.collateral_interest_bps_per_day.is_empty() {
        return Vec::new();
    }
    let day = now / DAY_MS;
    if day <= engine.last_collateral_interest_day {
        return Vec::new();
    }
    let mut events = Vec::new();
    for (subaccount, balances) in &engine.collateral {
        // The account's maintenance usage bounds what its collateral
        // is doing for it.
        let usage = engine
            .accounts
            .get(subaccount)
            .and_then(|a| engine.effective_margin_summary(a))
            .map(|s| s.maintenance_quote_minor)
            .unwrap_or(0);
        if usage == 0 {
            continue;
        }
        for (code, balance) in balances {
            let rate = engine
                .config
                .collateral_interest_bps_per_day
                .get(code)
                .copied()
                .unwrap_or(0);
            if rate == 0 || *balance == 0 {
                continue;
            }
            let Some(cfg) = engine.collateral_config(code) else {
                continue;
            };
            let Some(price) = engine.collateral_price(code, now) else {
                continue;
            };
            let Some(unit) = 10_u128.checked_pow(cfg.decimals) else {
                continue;
            };
            // Haircut value of the whole balance.
            let gross = mul_div(*balance, price, unit, poc_core::Rounding::Floor).unwrap_or(0);
            let net = mul_div(
                gross,
                10_000_u128.saturating_sub(u128::from(cfg.haircut_bps)),
                10_000,
                poc_core::Rounding::Floor,
            )
            .unwrap_or(0);
            let utilized = net.min(usage);
            // Interest in quote, rounded up (owed).
            let interest_quote =
                mul_div(utilized, u128::from(rate), 10_000, poc_core::Rounding::Ceil).unwrap_or(0);
            if interest_quote == 0 {
                continue;
            }
            // In-kind charge: minor units of the currency covering the
            // quote interest, rounded up against the holder.
            let amount_minor = mul_div(interest_quote, unit, price, poc_core::Rounding::Ceil)
                .unwrap_or(0)
                .min(*balance);
            if amount_minor == 0 {
                continue;
            }
            events.push(Event::CollateralInterestAccrued {
                subaccount: *subaccount,
                currency: code.clone(),
                amount_minor,
                quote_value_minor: interest_quote,
                ts: now,
            });
        }
    }
    events
}

// ----------------------------------------------------------------------
// Insurance inventory (G-23)
// ----------------------------------------------------------------------

/// Mark the fund's carried positions to the current marks and drip
/// inventory back into the book when the market pays an edge over the
/// carrying mark.
fn plan_insurance_inventory(
    engine: &Engine,
    now: TimestampMs,
    marks: &BTreeMap<String, MarkSet>,
) -> Vec<Event> {
    let mut events = Vec::new();
    for (symbol, (signed_lots, last_mark)) in &engine.insurance_inventory {
        if *signed_lots == 0 {
            continue;
        }
        let Some(instrument) = engine.instruments.get(symbol) else {
            continue;
        };
        let Some(mark) = engine.instrument_mark(instrument, marks) else {
            continue;
        };
        // 1. Mark to market.
        let per_lot = |price: u128| -> i128 {
            let base = instrument
                .position_notional_minor(price, *signed_lots)
                .unwrap_or(0);
            to_i128(base)
        };
        let pnl = per_lot(mark).saturating_sub(per_lot(*last_mark));
        if pnl != 0 {
            events.push(Event::InsuranceMarked {
                symbol: symbol.clone(),
                signed_lots: *signed_lots,
                mark_quote_minor: mark,
                pnl_quote_minor: pnl,
                ts: now,
            });
        }
        // 2. Rebalance drip: only long inventory (the fund buys bankrupt
        //    longs' positions), only when the book bids an edge over the
        //    new mark. The drip crosses the book as a synthetic IOC
        //    taker owned by the reserved insurance subaccount (no real
        //    account record — fills journal, makers settle, the fund
        //    books the proceeds), exactly the liquidation phase-A pattern.
        if *signed_lots > 0 {
            let lots_available = u64::try_from(*signed_lots).unwrap_or(0);
            let drip = lots_available.min(engine.config.insurance_rebalance_max_lots);
            if drip > 0 {
                if let Some(book) = engine.books.get(symbol) {
                    if let Some(bid_ticks) = book.best_bid() {
                        let Some(bid_quote) = instrument.price_quote_minor(bid_ticks) else {
                            continue;
                        };
                        let edge = mul_div(
                            bid_quote.saturating_sub(mark),
                            10_000,
                            mark.max(1),
                            poc_core::Rounding::Floor,
                        )
                        .unwrap_or(0);
                        if edge >= u128::from(engine.config.insurance_rebalance_edge_bps) {
                            let taker = Order {
                                id: engine.next_order_id,
                                subaccount: INSURANCE_SUBACCOUNT,
                                symbol: symbol.clone(),
                                side: Side::Ask,
                                order_type: OrderType::Limit,
                                price_ticks: Some(bid_ticks),
                                qty_lots: drip,
                                filled_lots: 0,
                                tif: poc_core::TimeInForce::Ioc,
                                post_only: false,
                                reduce_only: false,
                                stp: poc_core::SelfTradePrevention::CancelOldest,
                                display_lots: None,
                                trailing_extreme_quote_minor: None,
                                oco_group: None,
                                client_ts: now,
                                engine_ts: now,
                            };
                            let start = events.len();
                            events.extend(engine.plan_match(&taker, now, 0));
                            // Proceeds = the notional actually printed.
                            let mut proceeds = 0_u128;
                            let mut lots_sold = 0_u64;
                            for event in &events[start..] {
                                if let Event::TradeExecuted(trade) = event {
                                    proceeds = proceeds.saturating_add(trade.notional_quote_minor);
                                    lots_sold = lots_sold.saturating_add(trade.qty_lots);
                                }
                            }
                            if lots_sold > 0 {
                                events.push(Event::InsuranceRebalanced {
                                    symbol: symbol.clone(),
                                    lots: lots_sold,
                                    proceeds_quote_minor: proceeds,
                                    ts: now,
                                });
                            } else {
                                // No liquidity actually crossed: drop the
                                // empty plan.
                                events.truncate(start);
                            }
                        }
                    }
                }
            }
        }
    }
    events
}

/// The reserved subaccount id the insurance fund trades synthetic
/// inventory through (no account record — fills journal, makers settle,
/// the fund books proceeds).
pub(crate) const INSURANCE_SUBACCOUNT: SubaccountId = u64::MAX - 1;
