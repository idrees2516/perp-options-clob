//! Market-maker tier program wiring (G-15): observation, review, and the
//! fee discounts tiers earn.
//!
//! The measurement rides the sweep's randomized liquidity samples —
//! the same journaled [`LiquidityScored`](crate::event::Event::LiquidityScored)
//! events that drive reward payouts (G-39), so a maker cannot grind
//! uptime against a deterministic clock. Every sampled tick, for every
//! *enrolled* maker, the engine computes the worst-side spread and the
//! smaller-side size from that tick's observations and credits the
//! tightest tier whose spread/size bar the tick met. At review
//! boundaries the window drains, tiers are re-earned or lost, and the
//! active fee discount is re-assigned through a journaled
//! [`MmTierAdjusted`](crate::event::Event::MmTierAdjusted) event.
//!
//! The discount composes *after* the volume ladder: the tier buys
//! predictable depth, the volume tier buys flow, and the two ladders
//! pay for different behaviours — the same composition Deribit's
//! maker program uses against its volume tiers.

use std::collections::BTreeMap;

use crate::engine::Engine;
use crate::event::{Event, LiquidityObservation};
use poc_core::{Side, TimestampMs};

/// Apply one journaled liquidity-scoring tick to the MM ledger: every
/// enrolled maker's tick statistics are folded into their window.
///
/// Per tick, each enrolled maker's observations are grouped by side
/// into quote sets `(spread_bps, size_lots)`; the tightest tier whose
/// bar *some bid* and *some ask* both clear is credited to the maker's
/// window. A one-sided or absent maker scores nothing for the tick —
/// presence is what the obligations buy.
pub(crate) fn apply_liquidity_scored(engine: &mut Engine, observations: &[LiquidityObservation]) {
    if engine.mm_ledger.enrolled.is_empty() {
        return;
    }
    // (subaccount, side) -> quote set for this tick.
    let mut quotes: BTreeMap<(u64, Side), Vec<(u64, u64)>> = BTreeMap::new();
    for obs in observations {
        quotes
            .entry((obs.subaccount, obs.side))
            .or_default()
            .push((obs.spread_bps, obs.size_lots));
    }
    let enrolled: Vec<u64> = engine.mm_ledger.enrolled.keys().copied().collect();
    for sub in enrolled {
        let bids = quotes.get(&(sub, Side::Bid)).cloned().unwrap_or_default();
        let asks = quotes.get(&(sub, Side::Ask)).cloned().unwrap_or_default();
        let best = best_tier_met(engine, &bids, &asks);
        engine.mm_ledger.observe(sub, best);
    }
}

/// The best (tightest) tier a tick's bid/ask quote sets satisfy: a
/// tier is met iff some bid AND some ask sit within its spread band at
/// at least its size.
fn best_tier_met(engine: &Engine, bids: &[(u64, u64)], asks: &[(u64, u64)]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, tier) in engine.config.mm_program.tiers.iter().enumerate() {
        let bid_ok = bids
            .iter()
            .any(|(s, q)| *s <= tier.max_spread_bps && *q >= tier.min_size_lots);
        let ask_ok = asks
            .iter()
            .any(|(s, q)| *s <= tier.max_spread_bps && *q >= tier.min_size_lots);
        if bid_ok && ask_ok {
            best = Some(i);
            break;
        }
    }
    best
}

/// The sweep stage that closes review windows: every enrolled maker is
/// re-evaluated from the drained window and the outcome journaled.
pub(crate) fn plan_mm_review(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    let interval = engine.config.mm_program.review_interval_ms;
    if interval == 0 || engine.config.mm_program.tiers.is_empty() {
        return Vec::new();
    }
    if now < engine.next_mm_review_ts {
        return Vec::new();
    }
    let mut events = Vec::new();
    // Simulate the drain on a clone so apply replays the identical
    // evaluation (the same discipline every other sweep stage uses).
    let mut ledger = engine.mm_ledger.clone();
    for (sub, stats) in ledger.drain_window() {
        let tier = engine.config.mm_program.evaluate(&stats);
        let (name, discount) = tier.map_or((None, 0), |t| (Some(t.name), t.fee_discount_bps));
        events.push(Event::MmTierAdjusted {
            subaccount: sub,
            tier: name,
            fee_discount_bps: discount,
            uptime_permille: stats.uptime_permille(),
            ticks: stats.ticks_total,
            ts: now,
        });
    }
    events
}

/// Advance the review schedule after the window closes (apply side of
/// the review sweep stage).
pub(crate) fn advance_review_schedule(engine: &mut Engine) {
    let interval = engine.config.mm_program.review_interval_ms;
    if interval == 0 {
        return;
    }
    // Schedule the next window from the current boundary, never from
    // the past (a long-stalled engine still reviews at most once per
    // boundary, then re-anchors).
    engine.next_mm_review_ts = engine
        .next_mm_review_ts
        .saturating_add(interval)
        .max(engine.now);
}

/// Quote-balance interest (G-18 completion): charge the *utilized*
/// portion of positive quote cash at UTC day boundaries. Utilization is
/// the maintenance requirement the quote balance actually backs (min
/// of the two), so idle cash pays nothing — the same principle as the
/// non-quote collateral charge, applied to the quote currency itself.
///
/// Default rate is zero (a governance decision to enable); the
/// machinery, conservation, and tests ship either way.
pub(crate) fn plan_quote_interest(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    let rate = engine.config.quote_interest_bps_per_day;
    if rate == 0 {
        return Vec::new();
    }
    let day = now / DAY_MS;
    if day <= engine.last_quote_interest_day {
        return Vec::new();
    }
    let mut events = Vec::new();
    for (sub, account) in &engine.accounts {
        let cash = account.cash_quote_minor;
        if cash <= 0 {
            continue;
        }
        let usage = engine
            .effective_margin_summary(account)
            .map(|s| s.maintenance_quote_minor)
            .unwrap_or(0);
        if usage == 0 {
            continue;
        }
        let utilized = usage.min(cash.unsigned_abs());
        let interest = mul_div_ceiling(utilized, u128::from(rate)).unwrap_or(0);
        if interest == 0 {
            continue;
        }
        events.push(Event::QuoteInterestAccrued {
            subaccount: *sub,
            amount_quote_minor: interest,
            ts: now,
        });
    }
    events
}

/// ceil(bps / 10_000) share of an amount — interest rounds up against
/// the holder, matching the non-quote collateral path.
fn mul_div_ceiling(amount: u128, bps: u128) -> Option<u128> {
    poc_core::mul_div(amount, bps, 10_000, poc_core::Rounding::Ceil)
}

/// Record the UTC day of a quote-interest pass (apply side).
pub(crate) fn mark_quote_interest_day(engine: &mut Engine, now: TimestampMs) {
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    engine.last_quote_interest_day = now / DAY_MS;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_interest_ceils_against_the_holder() {
        assert_eq!(mul_div_ceiling(10_000, 100).unwrap(), 100);
        // 1 bps of 1 = 0.0001 -> rounds up to 1.
        assert_eq!(mul_div_ceiling(1, 1).unwrap(), 1);
        // 9999 units at 1 bps = 0.9999 -> 1.
        assert_eq!(mul_div_ceiling(9_999, 1).unwrap(), 1);
    }
}
