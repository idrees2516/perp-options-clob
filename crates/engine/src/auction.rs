//! Auction uncrossing (G-12): the sweep stage that turns an accumulated
//! auction book into uniform-price prints.
//!
//! The engine opens an auction with [`crate::command::Command::BeginAuction`]
//! (or auto-listing does, G-34). While open, orders rest without matching.
//! At the uncross deadline the sweep:
//!
//! 1. runs the pure [`poc_orderbook::LimitOrderBook::uncross`] — the
//!    volume-maximizing uniform clearing price;
//! 2. journals one [`Event::TradeExecuted`](crate::event::Event::TradeExecuted)
//!    per fill (maker-priced fees, but **both sides pay maker fees**: in a
//!    uniform-price auction there is no liquidity *taker*, every participant
//!    provided resting size — the Derive opening-auction convention);
//! 3. journals `OrderClosed(Filled)` for every fully-consumed order, both
//!    sides;
//! 4. emits [`Event::AuctionUncrossed`] with the resting-taker reductions
//!    (both counterparties of an auction print are resting orders, and the
//!    normal trade apply path only reduces the maker side of the book);
//! 5. restores continuous matching on the book.

use crate::engine::Engine;
use crate::event::{Event, OrderCloseReason, Trade};
use poc_core::{OrderId, TimestampMs};

/// Plan the uncross of every due auction. Pure over the snapshot.
pub(crate) fn plan_auction_uncross(engine: &Engine, now: TimestampMs) -> Vec<Event> {
    let mut events = Vec::new();
    let due: Vec<String> = engine
        .auctions
        .iter()
        .filter(|&(_, &uncross_at)| now >= uncross_at)
        .map(|(symbol, _)| symbol.clone())
        .collect();
    for symbol in due {
        let Some(book) = engine.books.get(&symbol) else {
            continue;
        };
        let Some(instrument) = engine.instruments.get(&symbol) else {
            continue;
        };
        let outcome = book.uncross();
        let clearing = outcome.clearing_price_ticks;

        // Trades at the uniform price. Both sides pay the *maker* fee.
        let mut taker_reductions: Vec<(OrderId, u64)> = Vec::new();
        for fill in &outcome.fills {
            let notional = instrument
                .notional_quote_minor(fill.price_ticks, fill.qty_lots)
                .unwrap_or(0);
            let taker_tier = engine.fee_schedule.tier_for(fill.taker_subaccount);
            let maker_tier = engine.fee_schedule.tier_for(fill.maker_subaccount);
            // Auctions charge the maker rate on both participants.
            let taker_fee = match instrument {
                poc_core::Instrument::Option(_) => {
                    let caps = &engine.config.option_fee_caps;
                    let underlying_notional = engine
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
                    poc_economics::FeeCalculator::option_maker_fee(
                        taker_tier,
                        caps,
                        underlying_notional,
                        notional,
                    )
                    .unwrap_or(0)
                }
                _ => poc_economics::FeeCalculator::maker_fee(taker_tier, notional).unwrap_or(0),
            };
            let maker_fee =
                poc_economics::FeeCalculator::maker_fee(maker_tier, notional).unwrap_or(0);
            events.push(Event::TradeExecuted(Box::new(Trade {
                seq: engine.seq + u64::try_from(taker_reductions.len()).unwrap_or(0) + 1,
                symbol: symbol.clone(),
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
            taker_reductions.push((fill.taker_order_id, fill.qty_lots));
        }

        // Close every fully-consumed order on both sides.
        let mut touched: Vec<OrderId> = Vec::new();
        for fill in &outcome.fills {
            for id in [fill.maker_order_id, fill.taker_order_id] {
                if !touched.contains(&id) {
                    touched.push(id);
                }
            }
        }
        for id in touched {
            let Some(resting) = book.get(id) else {
                continue;
            };
            let consumed_by_fills: u64 = outcome
                .fills
                .iter()
                .filter(|f| f.maker_order_id == id || f.taker_order_id == id)
                .map(|f| f.qty_lots)
                .sum();
            let mut final_order = resting.order.clone();
            final_order.filled_lots = final_order.filled_lots.saturating_add(consumed_by_fills);
            if !final_order.is_open() {
                events.push(Event::OrderClosed {
                    order_id: id,
                    subaccount: final_order.subaccount,
                    symbol: symbol.clone(),
                    order: final_order,
                    reason: OrderCloseReason::Filled,
                });
            }
        }

        events.push(Event::AuctionUncrossed {
            symbol: symbol.clone(),
            clearing_price_ticks: clearing,
            matched_lots: outcome.matched_lots,
            taker_reductions,
            ts: now,
        });
    }
    events
}
