//! Execution algorithms (G-08 OCO brackets, G-10 TWAP parents) and the
//! OCO sibling cascade.
//!
//! ## One-cancels-other (G-08)
//!
//! A bracket is two orders — canonically a take-profit and a stop-loss on
//! opposite sides of a position. The engine assigns the pair a group id
//! (journaled as [`Event::OcoLinked`]) and guarantees the invariant: *the
//! first sibling to reach any terminal state (filled completely,
//! triggered, canceled, expired, or IOC remainder) cancels the other*.
//! The cascade runs in [`Engine::process`] after every planning pass —
//! it scans the planned batch for terminal OCO members (a fully-filled
//! order, or a parked stop whose activation left the parking lot) and
//! appends the sibling's [`OrderCloseReason::OcoSibling`] closure. The
//! scan is a pure function over the planned events plus the pre-apply
//! state, so replay reproduces it exactly.
//!
//! ## TWAP parents (G-10)
//!
//! A parent order is pure accounting: it never touches the book. Each
//! tick, the slicer emits at most one child per parent — a marketable
//! IOC limit bounded by the parent's price cap — journaled as
//! [`Event::TwapSliced`] carrying the *full child request* (replay never
//! re-derives slice arithmetic). The engine then runs the child through
//! the ordinary place path: risk gates, matching, fees. Children are
//! sequentially committed, so two parents slicing on the same tick
//! cannot collide on order ids.

use crate::command::OrderRequest;
use crate::engine::Engine;
use crate::event::{Event, OrderCloseReason, OrderRejected, TwapParent};
use poc_core::{OrderId, Side, SubaccountId, Symbol, TimeInForce, TimestampMs};
use poc_risk::Rejection;

impl Engine {
    // ------------------------------------------------------------------
    // G-08: OCO brackets
    // ------------------------------------------------------------------

    /// Place an OCO pair: gate atomically, then commit both legs
    /// sequentially (the second leg's risk gate sees the first leg's
    /// reservation — no double-spend of order margin).
    pub(crate) fn process_place_oco(
        &mut self,
        first: OrderRequest,
        second: OrderRequest,
        now: TimestampMs,
    ) -> Vec<Event> {
        // Pair-level validity.
        let reject = |reason: &str| -> Vec<Event> {
            vec![Event::OrderRejection(Box::new(OrderRejected {
                request: first.clone(),
                reason: Rejection::InvalidOrder(reason.into()),
                order_id: self.next_order_id,
            }))]
        };
        if first.subaccount != second.subaccount {
            return reject("oco legs must share a subaccount");
        }
        if first.symbol != second.symbol {
            return reject("oco legs must share an instrument");
        }
        // Bracket semantics: both legs close the same position, so they
        // share a side (a long's TP sell-limit + SL sell-stop are both
        // asks); the OCO discipline is what makes them exclusive.
        if first.side != second.side {
            return reject("oco legs must share a side (bracket)");
        }
        // Atomicity pre-pass: the same cumulative-margin gate batches use.
        if let Some(rejection) = self.batch_gate(&[first.clone(), second.clone()], now) {
            return self.commit(rejection);
        }

        // Link, then commit both legs sequentially.
        let group = self.next_oco_id;
        let (id_first, id_second) = (self.next_order_id, self.next_order_id + 1);
        let mut first = first;
        let mut second = second;
        first.oco_group = Some(group);
        second.oco_group = Some(group);
        let mut events = self.commit(vec![Event::OcoLinked {
            group,
            first: id_first,
            second: id_second,
            ts: now,
        }]);

        let leg_one = self.process(crate::command::Command::Place {
            request: first,
            now,
        });
        let leg_two = self.process(crate::command::Command::Place {
            request: second,
            now,
        });
        events.extend(leg_one.iter().cloned());
        events.extend(leg_two.iter().cloned());

        // If either leg was rejected, unwind the sibling for atomicity.
        let rejected = events.iter().any(|e| matches!(e, Event::OrderRejection(_)));
        if rejected {
            let unwind: Vec<Event> = [id_first, id_second]
                .into_iter()
                .filter_map(|id| self.find_open_order(id))
                .map(|order| Event::OrderClosed {
                    order_id: order.id,
                    subaccount: order.subaccount,
                    symbol: order.symbol.clone(),
                    order,
                    reason: OrderCloseReason::Canceled,
                })
                .collect();
            events.extend(self.commit(unwind));
            return events;
        }

        // A leg may already have terminated (e.g. an IOC take-profit that
        // filled immediately): cascade now.
        let siblings = self.plan_oco_siblings(&events);
        events.extend(self.commit(siblings));
        events
    }

    /// Scan a planned event batch for OCO members that reached a terminal
    /// state and plan the sibling cancels. Pure over (state, events).
    pub(crate) fn plan_oco_siblings(&self, events: &[Event]) -> Vec<Event> {
        let mut done_groups: Vec<u64> = Vec::new();
        let mut out = Vec::new();
        for event in events {
            let terminal_id = match event {
                Event::OrderClosed {
                    order_id, reason, ..
                } if *reason != OrderCloseReason::OcoSibling => Some(*order_id),
                // A parked member activating (stop trigger) leaves the
                // parking lot — terminal for the bracket.
                Event::OrderResting { order, .. } => {
                    if !order.order_type.is_parked() && self.stop_orders.contains_key(&order.id) {
                        Some(order.id)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let Some(id) = terminal_id else { continue };
            let Some(group) = self.oco_group_of(id) else {
                continue;
            };
            if done_groups.contains(&group) {
                continue;
            }
            done_groups.push(group);
            let Some((a, b)) = self.oco_groups.get(&group).copied() else {
                continue;
            };
            for sibling in [a, b] {
                if sibling == id {
                    continue;
                }
                if let Some(order) = self.find_open_order(sibling) {
                    out.push(Event::OrderClosed {
                        order_id: sibling,
                        subaccount: order.subaccount,
                        symbol: order.symbol.clone(),
                        order,
                        reason: OrderCloseReason::OcoSibling,
                    });
                }
            }
        }
        out
    }

    /// The group an order id belongs to, if any.
    pub(crate) fn oco_group_of(&self, order_id: OrderId) -> Option<u64> {
        self.oco_groups
            .iter()
            .find(|(_, (a, b))| *a == order_id || *b == order_id)
            .map(|(g, _)| *g)
    }

    /// Look up an order wherever it lives (book or parking lot).
    pub(crate) fn find_open_order(&self, order_id: OrderId) -> Option<poc_core::Order> {
        if let Some(parked) = self.stop_orders.get(&order_id) {
            return Some(parked.clone());
        }
        self.books
            .values()
            .find_map(|book| book.get(order_id).map(|r| r.order.clone()))
    }

    // ------------------------------------------------------------------
    // G-10: TWAP parents
    // ------------------------------------------------------------------

    /// Open a TWAP parent after validating its slicing parameters.
    pub(crate) fn process_place_twap(
        &mut self,
        subaccount: SubaccountId,
        symbol: Symbol,
        side: Side,
        total_lots: u64,
        slices: u64,
        slice_interval_ms: TimestampMs,
        limit_ticks: Option<u64>,
        now: TimestampMs,
    ) -> Vec<Event> {
        let reject = |reason: &str| -> Vec<Event> {
            vec![Event::OrderRejection(Box::new(OrderRejected {
                request: OrderRequest {
                    subaccount,
                    symbol: symbol.clone(),
                    side,
                    order_type: poc_core::OrderType::Limit,
                    price_ticks: limit_ticks,
                    qty_lots: total_lots,
                    tif: TimeInForce::Gtc,
                    post_only: false,
                    reduce_only: false,
                    stp: poc_core::SelfTradePrevention::CancelNewest,
                    display_lots: None,
                    oco_group: None,
                    client_ts: now,
                },
                reason: Rejection::InvalidOrder(reason.into()),
                order_id: self.next_order_id,
            }))]
        };
        if slices == 0 || total_lots == 0 {
            return reject("twap needs slices and quantity");
        }
        if total_lots < slices {
            return reject("twap quantity below slice count");
        }
        if slice_interval_ms == 0 {
            return reject("twap slice interval must be positive");
        }
        if !self.instruments.contains_key(&symbol) {
            return reject("unknown instrument");
        }
        if let Some(ticks) = limit_ticks {
            if self
                .instruments
                .get(&symbol)
                .is_some_and(|i| i.price_quote_minor(ticks).is_none())
            {
                return reject("limit off tick grid");
            }
        }
        // The whole parent is margin-gated up front (children re-gate at
        // their own placement against live state).
        let request = OrderRequest {
            subaccount,
            symbol: symbol.clone(),
            side,
            order_type: poc_core::OrderType::Limit,
            price_ticks: limit_ticks,
            qty_lots: total_lots,
            tif: TimeInForce::Gtc,
            post_only: false,
            reduce_only: false,
            stp: poc_core::SelfTradePrevention::CancelNewest,
            display_lots: None,
            oco_group: None,
            client_ts: now,
        };
        if let Some(rejection) = self.batch_gate(&[request], now) {
            return rejection;
        }
        let parent = TwapParent {
            parent_id: self.next_twap_id,
            subaccount,
            symbol,
            side,
            total_lots,
            slices,
            slice_interval_ms,
            limit_ticks,
            next_slice_ts: now.saturating_add(slice_interval_ms),
            slices_placed: 0,
            lots_placed: 0,
            opened_ts: now,
        };
        self.commit(vec![Event::TwapOpened(Box::new(parent))])
    }

    /// Cancel a TWAP parent.
    pub(crate) fn process_cancel_twap(
        &mut self,
        subaccount: SubaccountId,
        parent_id: u64,
        now: TimestampMs,
    ) -> Vec<Event> {
        let Some(parent) = self.twap_parents.get(&parent_id) else {
            return Vec::new();
        };
        if parent.subaccount != subaccount {
            return Vec::new();
        }
        self.commit(vec![Event::TwapClosed {
            parent_id,
            subaccount,
            reason: "canceled",
            placed_lots: parent.lots_placed,
            ts: now,
        }])
    }

    /// Run one heartbeat tick, then run every TWAP child the slicer
    /// emitted through the ordinary place path (sequentially committed,
    /// so their order ids advance and their risk gates see live state).
    pub(crate) fn process_tick(&mut self, now: TimestampMs) -> Vec<Event> {
        let mut events = self.plan(&crate::command::Command::Tick { now });
        let siblings = self.plan_oco_siblings(&events);
        events.extend(siblings);

        // Peel the TWAP slice markers out; their children run after the
        // tick's other effects are applied.
        let mut out = Vec::with_capacity(events.len());
        let mut children: Vec<OrderRequest> = Vec::new();
        for event in events {
            if let Event::TwapSliced { request, .. } = &event {
                children.push(request.clone());
            }
            out.push(event);
        }
        self.commit(out.clone());

        for child in children {
            let child_events = self.process(crate::command::Command::Place {
                request: child,
                now,
            });
            out.extend(child_events);
        }
        out
    }
}
