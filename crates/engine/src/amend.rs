//! Batch placement, batch cancel, and order amendment (G-09).
//!
//! ## Amendment queue rules (Derive V3 / Paradex, resolved)
//!
//! The pure-match engine cannot mutate a resting order's price or size in
//! place without breaking price-time priority, so:
//!
//! * **price change** (either direction — improving *or* widening) or a
//!   **size increase** → cancel-and-replace: the old order closes, a new
//!   order id rests at the back of its price level. No queue privilege
//!   survives an economic change (the strictest reading of the Derive
//!   rule — the choice that is hardest to game).
//! * **size reduction at the same price** → applied in place via
//!   [`Event::OrderAmended`]; queue priority is kept (Deribit/Paradex
//!   behaviour).
//! * parked stop and trailing orders amend only through replacement
//!   (their trigger state is part of the parked order).
//!
//! ## Batch placement
//!
//! A batch is **atomic on validity, sequential on events**: every request
//! is validated against the pre-batch snapshot and one failure rejects
//! the whole batch; a *cumulative* order-margin simulation then gates the
//! accounts touched (per-order gates alone would let a batch of N orders
//! each pass N times the free equity). Intra-batch price-time interaction
//! does not occur — the batch is a transport and margin-atomicity
//! primitive, not a matching primitive; matching happens against the
//! pre-batch book exactly as if the orders had arrived in the same
//! millisecond on different connections.

use std::collections::BTreeMap;

use poc_core::{Order, OrderId, SubaccountId, TimestampMs};

use crate::command::OrderRequest;
use crate::engine::Engine;
use crate::event::{Event, OrderAmended, OrderCloseReason, OrderRejected};

impl Engine {
    /// Plan a batch cancel: concatenate the per-order plans.
    pub(crate) fn plan_cancel_batch(
        &self,
        subaccount: SubaccountId,
        order_ids: &[OrderId],
    ) -> Vec<Event> {
        let mut events = Vec::new();
        for &id in order_ids {
            events.extend(self.plan_cancel(subaccount, id));
        }
        events
    }

    /// Plan a batch placement: all-or-nothing validity + a cumulative
    /// order-margin gate, then the per-order plans.
    pub(crate) fn plan_place_batch(
        &self,
        requests: &[OrderRequest],
        now: TimestampMs,
    ) -> Vec<Event> {
        // Static validation first — one bad request rejects the batch.
        for request in requests {
            let Some(instrument) = self.instruments.get(&request.symbol) else {
                return batch_rejection(requests, self.next_order_id, "unknown instrument");
            };
            if let Err(why) = instrument.validate_qty(request.qty_lots) {
                return batch_rejection(requests, self.next_order_id, &why.to_string());
            }
            if let Some(ticks) = request.price_ticks {
                if instrument.price_quote_minor(ticks).is_none() {
                    return batch_rejection(requests, self.next_order_id, "price off tick grid");
                }
            }
            if self
                .books
                .get(&request.symbol)
                .is_some_and(|b| b.auction_mode())
                && !matches!(request.order_type, poc_core::OrderType::Limit)
            {
                return batch_rejection(
                    requests,
                    self.next_order_id,
                    "only limit orders rest in an auction",
                );
            }
        }

        // Cumulative order-margin simulation per touched account: the sum
        // of the batch's per-order reservations must still fit inside the
        // account's spendable cash after its existing reservations.
        let marks = self.build_marks(now);
        let Some(marks) = marks else {
            return batch_rejection(requests, self.next_order_id, "missing marks");
        };
        let mut reserved: BTreeMap<SubaccountId, u128> = BTreeMap::new();
        for request in requests {
            let Some(instrument) = self.instruments.get(&request.symbol) else {
                continue;
            };
            let Some(account) = self.accounts.get(&request.subaccount) else {
                continue;
            };
            let Some(mark) = self.instrument_mark(instrument, &marks) else {
                return batch_rejection(requests, self.next_order_id, "missing mark");
            };
            let order = Order {
                id: 0,
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
                client_ts: request.client_ts,
                engine_ts: now,
            };
            let ctx = poc_risk::OrderRiskContext {
                order: &order,
                instrument,
                instruments: &self.instruments,
                account,
                marks: &marks,
                margin_engine: &self.margin_engine,
                mark_quote_minor: mark,
                best_bid_ticks: self.books.get(&request.symbol).and_then(|b| b.best_bid()),
                best_ask_ticks: self.books.get(&request.symbol).and_then(|b| b.best_ask()),
                limits: self.config.limits,
                halted: self
                    .halted
                    .get(instrument.base_symbol())
                    .copied()
                    .unwrap_or(false),
                estimated_fee_quote_minor: self.estimate_fee(instrument, &order, mark),
                collateral_equity_quote_minor: poc_core::to_i128(
                    self.collateral_value_of(request.subaccount, now),
                ),
            };
            if let Err(reason) = poc_risk::check_order(&ctx) {
                return batch_rejection(requests, self.next_order_id, &reason.to_string());
            }
            let increment = poc_risk::order_margin_increment(&ctx);
            *reserved.entry(request.subaccount).or_insert(0) = reserved
                .get(&request.subaccount)
                .copied()
                .unwrap_or(0)
                .saturating_add(increment);
        }
        for (sub, total_reserved) in &reserved {
            if let Some(account) = self.accounts.get(sub) {
                let existing = account.order_margin_quote_minor;
                let headroom = account
                    .cash_quote_minor
                    .saturating_sub(i128::try_from(existing).unwrap_or(i128::MAX));
                if i128::try_from(*total_reserved).unwrap_or(i128::MAX) > headroom {
                    return batch_rejection(
                        requests,
                        self.next_order_id,
                        "batch exceeds order margin",
                    );
                }
            }
        }

        // All gates passed: plan each order (each sees the pre-batch
        // snapshot — the documented batch semantics).
        let mut events = Vec::new();
        for request in requests {
            events.extend(self.plan_place(request, now));
        }
        events
    }

    /// Plan an amendment following the G-09 queue rules.
    pub(crate) fn plan_amend(
        &self,
        subaccount: SubaccountId,
        order_id: OrderId,
        new_price_ticks: Option<u64>,
        new_open_lots: Option<u64>,
        now: TimestampMs,
    ) -> Vec<Event> {
        let reject = |reason: &str| -> Vec<Event> {
            vec![Event::OrderRejection(Box::new(OrderRejected {
                request: placeholder_request(subaccount),
                reason: poc_risk::Rejection::InvalidOrder(reason.into()),
                order_id,
            }))]
        };

        // Parked (stop / trailing) orders amend only by replacement; the
        // engine does not mutate parked trigger state in place.
        if let Some(parked) = self.stop_orders.get(&order_id) {
            if parked.subaccount != subaccount {
                return Vec::new();
            }
            let mut replacement = parked.clone();
            if let Some(px) = new_price_ticks {
                if let poc_core::OrderType::StopLimit { trigger_price, .. } = replacement.order_type
                {
                    replacement.order_type = poc_core::OrderType::StopLimit {
                        trigger_price,
                        limit_price: px,
                    };
                } else if let poc_core::OrderType::TrailingStopLimit { offset_ticks, .. } =
                    replacement.order_type
                {
                    replacement.order_type = poc_core::OrderType::TrailingStopLimit {
                        offset_ticks,
                        limit_ticks: px,
                    };
                } else {
                    return reject("order does not carry a limit price");
                }
            }
            if let Some(qty) = new_open_lots {
                if qty == 0 {
                    return reject("quantity must be positive");
                }
                replacement.qty_lots = qty;
                replacement.filled_lots = 0;
            }
            replacement.engine_ts = now;
            return vec![
                Event::OrderClosed {
                    order_id,
                    subaccount,
                    symbol: parked.symbol.clone(),
                    order: parked.clone(),
                    reason: OrderCloseReason::Canceled,
                },
                Event::OrderResting {
                    order: replacement,
                    margin_reserved_quote_minor: 0,
                },
            ];
        }

        // Resting limit orders.
        let Some(account) = self.accounts.get(&subaccount) else {
            return Vec::new();
        };
        let Some(info) = account.open_orders.get(&order_id) else {
            return Vec::new();
        };
        let symbol = info.symbol.clone();
        let Some(resting) = self
            .books
            .get(&symbol)
            .and_then(|b| b.get(order_id))
            .map(|r| r.order.clone())
        else {
            return Vec::new();
        };
        let price_changed = new_price_ticks.is_some_and(|p| Some(p) != resting.price_ticks);
        let old_open = resting.open_qty();
        let new_open = new_open_lots.unwrap_or(old_open);
        if new_open == 0 {
            return reject("quantity must be positive");
        }
        let size_increases = new_open > old_open;

        if price_changed || size_increases {
            // Cancel-and-replace: fresh id, back of the level.
            let mut replacement = resting.clone();
            if let Some(px) = new_price_ticks {
                replacement.price_ticks = Some(px);
            }
            replacement.qty_lots = new_open + resting.filled_lots;
            replacement.filled_lots = 0;
            replacement.display_lots = replacement.display_lots.map(|d| d.min(new_open));
            replacement.engine_ts = now;
            replacement.id = self.next_order_id;
            let reservation = self.reservations.get(&order_id).copied().unwrap_or(0);
            return vec![
                Event::OrderClosed {
                    order_id,
                    subaccount,
                    symbol: symbol.clone(),
                    order: resting,
                    reason: OrderCloseReason::Canceled,
                },
                Event::OrderResting {
                    order: replacement,
                    margin_reserved_quote_minor: reservation,
                },
            ];
        }

        // In-place reduction at the same price: keeps priority.
        if new_open < old_open {
            return vec![Event::OrderAmended(Box::new(OrderAmended {
                order_id,
                subaccount,
                symbol,
                new_open_lots: new_open,
                ts: now,
            }))];
        }

        // Nothing changed.
        Vec::new()
    }
}

/// One rejection for the whole batch (G-09 atomicity).
fn batch_rejection(requests: &[OrderRequest], order_id: OrderId, reason: &str) -> Vec<Event> {
    let request = requests
        .first()
        .cloned()
        .unwrap_or_else(|| placeholder_request(0));
    vec![Event::OrderRejection(Box::new(OrderRejected {
        request,
        reason: poc_risk::Rejection::InvalidOrder(format!("batch rejected: {reason}")),
        order_id,
    }))]
}

/// A minimal request shell for rejection events synthesized without a
/// client request (amend paths).
fn placeholder_request(subaccount: SubaccountId) -> OrderRequest {
    OrderRequest {
        subaccount,
        symbol: String::new(),
        side: poc_core::Side::Bid,
        order_type: poc_core::OrderType::Limit,
        price_ticks: None,
        qty_lots: 0,
        tif: poc_core::TimeInForce::Gtc,
        post_only: false,
        reduce_only: false,
        stp: poc_core::SelfTradePrevention::CancelNewest,
        display_lots: None,
        client_ts: 0,
    }
}
