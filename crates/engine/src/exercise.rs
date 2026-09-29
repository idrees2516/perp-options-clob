//! American early-exercise settlement (the exercise queue).
//!
//! Lifecycle, start to finish:
//!
//! 1. **Request** — [`crate::command::Command::Exercise`] validates the
//!    tender (American market, live long position, healthy oracle) and
//!    journals [`crate::event::Event::ExerciseQueued`], parking the
//!    request until `now + settlement_twap_ms`. The *requested* lots are
//!    not locked: the holder may still trade the position; settlement
//!    takes `min(requested, position at settle)` — a self-limiting rule
//!    that keeps the matching path untouched and can never over-settle.
//! 2. **TWAP window** — the window ending at `settle_at` accumulates
//!    oracle marks. A single wick cannot strike the settlement: the
//!    manipulator would have to hold the *average* of the whole window.
//! 3. **Settlement** — the sweep strikes intrinsic on the TWAP, closes
//!    the exercised long at that value, and assigns the matching short
//!    positions **pro-rata by short size** with largest-remainder
//!    remainder distribution (deterministic: ties by subaccount id).
//!    The long pays an exercise fee on intrinsic proceeds, routed
//!    through the standard revenue split. Assigned shorts may breach
//!    maintenance margin; the liquidation cascade later in the same
//!    sweep handles them.
//!
//! If the TWAP is not yet computable (young or halted oracle), the
//! request **defers** by one window rather than settling on a degraded
//! price — the fail-safe convention the rest of the engine uses.
//!
//! Assignment conserves the position ledger exactly: the lots closed on
//! the long side equal the lots assigned across shorts (largest
//! remainder guarantees the sum when total shorts ≥ settled lots, which
//! the instrument's zero-sum open interest guarantees).

use poc_core::num::{mul_div, mul_div_floor, Rounding};
use poc_core::{to_i128, Instrument, SubaccountId, Symbol, TimestampMs};

use crate::engine::Engine;
use crate::event::{ExerciseAssignment, OptionExercised};

/// A parked early-exercise request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExerciseRequest {
    /// Engine-assigned request id (deterministic: monotonic).
    pub id: u64,
    /// The tendering long.
    pub subaccount: SubaccountId,
    /// American option market.
    pub symbol: Symbol,
    /// Requested lots.
    pub lots: u64,
    /// Request acceptance time.
    pub requested_at: TimestampMs,
    /// Settlement (and TWAP window end) time.
    pub settle_at: TimestampMs,
}

/// Plan the `Exercise` command: pure validation, then a queued event.
pub(super) fn plan_exercise(
    engine: &Engine,
    subaccount: SubaccountId,
    symbol: &str,
    lots: u64,
    now: TimestampMs,
) -> Vec<crate::event::Event> {
    use crate::event::Event;

    let reject = |reason: &'static str| {
        vec![Event::ExerciseRejected {
            subaccount,
            symbol: symbol.into(),
            requested_lots: lots,
            reason,
            ts: now,
        }]
    };

    if lots == 0 {
        return reject("zero-lots");
    }
    let Some(Instrument::Option(market)) = engine.instruments.get(symbol) else {
        return reject("not-an-option");
    };
    if !market.is_american() {
        return reject("european-style");
    }
    if market.variant == poc_core::OptionVariant::Dated && market.expiry_ts_ms <= now {
        return reject("expired");
    }
    let Some(account) = engine.accounts.get(&subaccount) else {
        return reject("unknown-account");
    };
    let long = account.lots_of(symbol);
    if long <= 0 {
        return reject("no-long-position");
    }
    if lots > long.unsigned_abs() {
        return reject("exceeds-position");
    }
    if engine
        .halted
        .get(&market.base_symbol)
        .copied()
        .unwrap_or(false)
    {
        return reject("underlying-halted");
    }

    let request_id = engine.next_exercise_id;
    let settle_at = now.saturating_add(market.american.settlement_twap_ms);
    vec![Event::ExerciseQueued {
        request_id,
        subaccount,
        symbol: symbol.into(),
        lots,
        requested_at: now,
        settle_at,
    }]
}

/// Settle every matured exercise request (sweep step 6c).
///
/// Determinism: requests settle in id order; assignment is pro-rata with
/// largest-remainder distribution, ties by subaccount id; everything is a
/// pure function of engine state at `now`, so replays reproduce the
/// settlement bit for bit.
pub(crate) fn plan_american_exercises(
    engine: &Engine,
    now: TimestampMs,
) -> Vec<crate::event::Event> {
    use crate::event::Event;

    let mut events = Vec::new();
    let matured: Vec<&ExerciseRequest> = engine
        .exercises
        .values()
        .filter(|ex| ex.settle_at <= now)
        .collect();
    for req in matured {
        let Some(Instrument::Option(market)) = engine.instruments.get(&req.symbol).cloned() else {
            // Market vanished (delisted): consume the request with an
            // empty settlement so the queue cannot leak.
            events.push(Event::OptionExercised(Box::new(OptionExercised {
                request_id: req.id,
                subaccount: req.subaccount,
                symbol: req.symbol.clone(),
                requested_lots: req.lots,
                settled_lots: 0,
                settlement_quote_minor: 0,
                intrinsic_per_lot_quote_minor: 0,
                gross_payout_quote_minor: 0,
                exercise_fee_quote_minor: 0,
                assignments: Vec::new(),
                ts: now,
            })));
            continue;
        };

        // Strike the settlement on the TWAP window ending at settle_at.
        let twap = engine
            .oracles
            .get(&market.base_symbol)
            .and_then(|o| o.twap(req.settle_at, market.american.settlement_twap_ms));
        let Some(settlement) = twap else {
            // Fail-safe: defer by one window rather than settle on a
            // degraded price.
            events.push(Event::ExerciseDeferred {
                request_id: req.id,
                new_settle_at: req
                    .settle_at
                    .saturating_add(market.american.settlement_twap_ms),
            });
            continue;
        };

        let instrument = Instrument::Option(market.clone());
        // The long may have traded since requesting: settle what stands.
        let held = engine
            .accounts
            .get(&req.subaccount)
            .map(|a| a.lots_of(&req.symbol))
            .unwrap_or(0);
        let settled_lots = req.lots.min(held.max(0).unsigned_abs());

        // Pro-rata assignment across shorts, largest-remainder exact
        // (deterministic: remainder descending, then subaccount id).
        let shorts: Vec<(SubaccountId, u64)> = engine
            .accounts
            .iter()
            .filter(|&(sub, acc)| *sub != req.subaccount && acc.lots_of(&req.symbol) < 0)
            .map(|(sub, acc)| (*sub, acc.lots_of(&req.symbol).unsigned_abs()))
            .collect();
        let total_short: u64 = shorts.iter().map(|&(_, l)| l).sum();
        // Defensive clamp: open interest is zero-sum per instrument, so
        // total shorts ≥ this long's lots ≥ settled — but if state were
        // ever inconsistent we settle only what can be assigned, keeping
        // the position ledger balanced on both sides.
        let effective = settled_lots.min(total_short);

        let mut alloc: Vec<u64> = Vec::with_capacity(shorts.len());
        let mut rem: Vec<u64> = Vec::with_capacity(shorts.len());
        for &(_, l) in &shorts {
            let scaled = u128::from(l).saturating_mul(u128::from(effective));
            let base = mul_div_floor(
                u128::from(l),
                u128::from(effective),
                u128::from(total_short),
            )
            .unwrap_or(0);
            alloc.push(u64::try_from(base).unwrap_or(0));
            rem.push(u64::try_from(scaled % u128::from(total_short.max(1))).unwrap_or(0));
        }
        // Σfrac < n, so at most one extra lot per short and a single
        // pass over the remainder ranking closes the gap exactly.
        let assigned: u64 = alloc.iter().sum();
        let mut order: Vec<usize> = (0..shorts.len()).collect();
        order.sort_by(|&a, &b| rem[b].cmp(&rem[a]).then(shorts[a].0.cmp(&shorts[b].0)));
        for &i in order
            .iter()
            .take((effective - assigned.min(effective)) as usize)
        {
            alloc[i] += 1;
        }

        // Intrinsic per lot at the TWAP settlement.
        let intrinsic_per_lot = instrument
            .option_intrinsic_quote_minor(&market, settlement)
            .unwrap_or(0);
        let gross = intrinsic_per_lot
            .checked_mul(u128::from(effective))
            .unwrap_or(0);
        let fee = mul_div(
            gross,
            u128::from(market.american.exercise_fee_bps),
            10_000,
            Rounding::NearestHalfUp,
        )
        .unwrap_or(0);

        let assignments: Vec<ExerciseAssignment> = shorts
            .iter()
            .zip(alloc.iter())
            .filter(|&(_, &a)| a > 0)
            .map(|(&(sub, _), &a)| ExerciseAssignment {
                subaccount: sub,
                lots: a,
                charge_quote_minor: intrinsic_per_lot.saturating_mul(u128::from(a)),
            })
            .collect();

        events.push(Event::OptionExercised(Box::new(OptionExercised {
            request_id: req.id,
            subaccount: req.subaccount,
            symbol: req.symbol.clone(),
            requested_lots: req.lots,
            settled_lots: effective,
            settlement_quote_minor: settlement,
            intrinsic_per_lot_quote_minor: intrinsic_per_lot,
            gross_payout_quote_minor: to_i128(gross),
            exercise_fee_quote_minor: fee,
            assignments,
            ts: now,
        })));
    }
    events
}
