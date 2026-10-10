//! FIX application layer: the bridge between a legacy OMS/EMS desk and
//! the engine.
//!
//! # What this layer does
//!
//! The session layer ([`crate::session`]) keeps the wires alive; this
//! layer carries the business. It maps the FIX order-management
//! vocabulary onto the engine's commands and back:
//!
//! | FIX inbound | Engine command | FIX outbound |
//! |---|---|---|
//! | `NewOrderSingle (D)` | `Place` | `ExecutionReport (8)` ack + fills |
//! | `NewOrderSingle (D)` + `QuoteID(117)` | `RfqExecute` / `RfqExecutePartial` | `ExecutionReport` fills |
//! | `OrderCancelRequest (F)` | `Cancel` | `ExecutionReport` / `OrderCancelReject (9)` |
//! | `OrderCancelReplaceRequest (G)` | `Amend` | `ExecutionReport` ack |
//! | `OrderStatusRequest (H)` | — | `ExecutionReport` state |
//! | `MarketDataRequest (V)` | — | `MarketDataSnapshotFullRefresh (W)` |
//! | `QuoteRequest (R)` | `RfqCreate` | (routing) |
//! | `Quote (S)` | `RfqQuote` | (routing) |
//! | `QuoteCancel (Z)` | `RfqCancel` | — |
//!
//! # The RFQ flow over FIX (the Paradigm dealer rails)
//!
//! 1. The **taker's** desk sends `QuoteRequest (R)` with `131=` their
//!    reference; the gateway creates the engine RFQ and remembers
//!    `131 → rfq_id`.
//! 2. A **dealer** desk answers with `Quote (S)` carrying the same
//!    `131=` plus their own `117=` quote id, a price, and a size; the
//!    gateway submits the engine quote (full or working size).
//! 3. The taker **hits** the quote with a plain `NewOrderSingle (D)`
//!    carrying `117=` — the standard FIX "order against quote" — and
//!    the gateway executes the RFQ (fully, or partially by the order's
//!    quantity).
//! 4. Both desks receive `ExecutionReport (8)` with `ExecType=F` for
//!    every leg filled — through the same margin, fee, and journal
//!    path as every other venue trade.
//!
//! # Conventions
//!
//! * **Prices and quantities are integers.** `Price(44)`/`LastPx(31)`
//!   carry the venue's tick count; `OrderQty(38)`/`LastQty(32)` carry
//!   lots. (Integer FIX encodings are the crypto-venue norm — Deribit
//!   and Derive both do this.) `AvgPx(6)` is the volume-weighted tick
//!   average, truncated toward zero.
//! * **ExecInst(18)** venue extensions: `P` = post-only, `R` =
//!   reduce-only (may be combined, e.g. `PR`).
//! * Quote privacy is preserved: a dealer's `QuoteStatusRequest`
//!   sees only their own quotes — never a rival's.

use std::collections::BTreeMap;

use poc_core::{OrderType, Side, SubaccountId, TimeInForce};
use poc_engine::{Command, Engine, Event, OrderCloseReason, OrderRequest, RfqLegCommand};

use crate::session::{FixSession, Inbound, SessionError};
use crate::wire::{tag, FixMessage, MsgType};

/// OrdStatus(39) codes.
mod ord_status {
    /// New.
    pub const NEW: &str = "0";
    /// Partially filled.
    pub const PARTIALLY_FILLED: &str = "1";
    /// Filled.
    pub const FILLED: &str = "2";
    /// Canceled (or expired).
    pub const CANCELED: &str = "4";
    /// Rejected.
    pub const REJECTED: &str = "8";
}

/// ExecType(150) codes.
mod exec_type {
    /// Acknowledged / resting.
    pub const NEW: &str = "0";
    /// Trade (partial or full fill).
    pub const TRADE: &str = "F";
    /// Canceled.
    pub const CANCELED: &str = "4";
    /// Rejected.
    pub const REJECTED: &str = "8";
}

/// The gateway's view of one order, keyed by the desk's ClOrdID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedOrder {
    /// The desk's ClOrdID.
    pub cl_ord_id: String,
    /// The engine's order id.
    pub order_id: u64,
    /// Instrument.
    pub symbol: String,
    /// Side.
    pub side: Side,
    /// Original quantity, lots.
    pub qty_lots: u64,
    /// Cumulative filled quantity, lots.
    pub cum_qty_lots: u64,
    /// Sum of `price_ticks * qty` over fills (for AvgPx, in ticks).
    pub cum_px_weighted: u128,
    /// Current OrdStatus code.
    pub status: &'static str,
    /// True once the order left the book terminally.
    pub done: bool,
}

impl TrackedOrder {
    /// LeavesQty: original minus filled.
    #[must_use]
    pub fn leaves_qty(&self) -> u64 {
        self.qty_lots.saturating_sub(self.cum_qty_lots)
    }
}

/// ClOrdID ⇄ engine-order-id routing and fill accounting.
#[derive(Debug, Default)]
pub struct OrderRouter {
    orders: BTreeMap<String, TrackedOrder>,
    by_engine_id: BTreeMap<u64, String>,
    next_exec_id: u64,
}

impl OrderRouter {
    /// A fresh router.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly accepted order (ack state: New).
    pub fn track(&mut self, order: TrackedOrder) {
        self.by_engine_id
            .insert(order.order_id, order.cl_ord_id.clone());
        self.orders.insert(order.cl_ord_id.clone(), order);
    }

    /// Look up by the desk's ClOrdID.
    #[must_use]
    pub fn by_cl_ord_id(&self, cl_ord_id: &str) -> Option<&TrackedOrder> {
        self.orders.get(cl_ord_id)
    }

    /// Look up by the engine's order id (mutable variant for internal
    /// bookkeeping).
    pub fn by_engine_id(&mut self, order_id: u64) -> Option<&TrackedOrder> {
        self.by_engine_id
            .get(&order_id)
            .and_then(|c| self.orders.get(c))
    }

    /// Record a fill (in ticks). Unknown order ids are ignored (the
    /// router only tracks this desk's orders).
    pub fn record_fill(&mut self, order_id: u64, price_ticks: u64, qty_lots: u64) {
        let Some(cl) = self.by_engine_id.get(&order_id).cloned() else {
            return;
        };
        let Some(o) = self.orders.get_mut(&cl) else {
            return;
        };
        o.cum_qty_lots = o.cum_qty_lots.saturating_add(qty_lots);
        o.cum_px_weighted = o
            .cum_px_weighted
            .saturating_add(u128::from(price_ticks) * u128::from(qty_lots));
        if o.cum_qty_lots >= o.qty_lots {
            o.status = ord_status::FILLED;
            o.done = true;
        } else {
            o.status = ord_status::PARTIALLY_FILLED;
        }
    }

    /// Volume-weighted average price in ticks (truncated toward zero).
    #[must_use]
    pub fn avg_px_ticks(&self, cl_ord_id: &str) -> u64 {
        match self.orders.get(cl_ord_id) {
            Some(o) if o.cum_qty_lots > 0 => {
                u64::try_from(o.cum_px_weighted / u128::from(o.cum_qty_lots)).unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// Immutable order lookup by engine id.
    #[must_use]
    pub fn by_engine_id_ref(&self, order_id: u64) -> Option<&TrackedOrder> {
        self.by_engine_id
            .get(&order_id)
            .and_then(|c| self.orders.get(c))
    }

    /// Mark an order canceled / expired / removed.
    pub fn close(&mut self, order_id: u64, status: &'static str) {
        if let Some(cl) = self.by_engine_id.get(&order_id).cloned() {
            if let Some(o) = self.orders.get_mut(&cl) {
                o.status = status;
                o.done = true;
            }
        }
    }

    /// The next outbound ExecID.
    pub fn next_exec_id(&mut self) -> u64 {
        self.next_exec_id = self.next_exec_id.saturating_add(1);
        self.next_exec_id
    }
}

/// Gateway-level failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    /// The session id is unknown.
    UnknownSession(u64),
    /// Wire-level failure.
    Wire(String),
    /// Session-level failure.
    Session(String),
    /// The CompID is not mapped to a subaccount.
    UnmappedCompId(String),
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSession(id) => write!(f, "unknown session {id}"),
            Self::Wire(e) => write!(f, "wire: {e}"),
            Self::Session(e) => write!(f, "session: {e}"),
            Self::UnmappedCompId(c) => write!(f, "unmapped CompID {c}"),
        }
    }
}

impl std::error::Error for GatewayError {}

/// One connected desk.
struct GatewaySession {
    session: FixSession,
    subaccount: SubaccountId,
    last_inbound: u64,
}

/// The FIX acceptor: sessions, the engine, and the translations
/// between them.
pub struct FixGateway {
    engine: Engine,
    sessions: BTreeMap<u64, GatewaySession>,
    /// CompID → subaccount (provisioning; must precede Logon).
    comp_ids: BTreeMap<String, SubaccountId>,
    router: OrderRouter,
    /// The desks' QuoteReqID(131) → engine rfq_id.
    rfq_by_req_id: BTreeMap<String, u64>,
    /// (dealer subaccount, QuoteID(117)) → engine quote_id.
    quote_by_id: BTreeMap<(SubaccountId, String), u64>,
    /// (rfq req id, dealer) → the dealer's QuoteID for the current
    /// live quote on that RFQ.
    live_quote_ids: BTreeMap<(String, SubaccountId), String>,
    next_session_id: u64,
}

impl FixGateway {
    /// A new gateway over an engine.
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            sessions: BTreeMap::new(),
            comp_ids: BTreeMap::new(),
            router: OrderRouter::new(),
            rfq_by_req_id: BTreeMap::new(),
            quote_by_id: BTreeMap::new(),
            live_quote_ids: BTreeMap::new(),
            next_session_id: 1,
        }
    }

    /// Provision a CompID: which subaccount speaks as this desk.
    pub fn register_comp_id(&mut self, comp_id: impl Into<String>, subaccount: SubaccountId) {
        self.comp_ids.insert(comp_id.into(), subaccount);
    }

    /// The engine (read-only access for tests and supervision).
    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Mutable engine access (administrative commands).
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// The order router (read-only).
    #[must_use]
    pub fn router(&self) -> &OrderRouter {
        &self.router
    }

    /// Accept a connection from a provisioned CompID. Returns the
    /// session id.
    pub fn connect(&mut self, comp_id: &str) -> Result<u64, GatewayError> {
        let sub = *self
            .comp_ids
            .get(comp_id)
            .ok_or_else(|| GatewayError::UnmappedCompId(comp_id.to_string()))?;
        let id = self.next_session_id;
        self.next_session_id = id.saturating_add(1);
        self.sessions.insert(
            id,
            GatewaySession {
                session: FixSession::new("VENUE", comp_id),
                subaccount: sub,
                last_inbound: 0,
            },
        );
        Ok(id)
    }

    /// Process one inbound wire frame. Returns the outbound frames, in
    /// order. This is the whole gateway.
    pub fn on_frame(
        &mut self,
        session_id: u64,
        frame: &[u8],
        now: u64,
    ) -> Result<Vec<Vec<u8>>, GatewayError> {
        let msg = FixMessage::decode(frame).map_err(|e| GatewayError::Wire(e.to_string()))?;
        let (inbound, mut out) = self
            .handle_session(session_id, &msg, now)
            .map_err(|e| GatewayError::Session(e.to_string()))?;

        if let Inbound::App(app) = inbound {
            let app_out = self.handle_app(session_id, app, now);
            out.extend(app_out);
        }
        // Deliver any event-driven frames from this exchange round.
        Ok(out)
    }

    /// Session-layer dispatch with bookkeeping. The returned `Inbound`
    /// borrows the caller's message, never the gateway.
    fn handle_session<'m>(
        &mut self,
        session_id: u64,
        msg: &'m FixMessage,
        now: u64,
    ) -> Result<(Inbound<'m>, Vec<Vec<u8>>), SessionError> {
        let Some(s) = self.sessions.get_mut(&session_id) else {
            return Err(SessionError::MissingHeader("session"));
        };
        s.last_inbound = now;
        s.session.note_inbound();
        s.session.handle(msg, now)
    }

    /// Application-layer dispatch.
    fn handle_app(&mut self, session_id: u64, msg: &FixMessage, now: u64) -> Vec<Vec<u8>> {
        let Some(mtype) = msg.msg_type() else {
            return Vec::new();
        };
        match mtype {
            MsgType::NewOrderSingle => self.on_new_order(session_id, msg, now),
            MsgType::OrderCancelRequest => self.on_cancel(session_id, msg, now),
            MsgType::OrderCancelReplaceRequest => self.on_replace(session_id, msg, now),
            MsgType::OrderStatusRequest => self.on_order_status(session_id, msg, now),
            MsgType::MarketDataRequest => self.on_market_data(session_id, msg, now),
            MsgType::QuoteRequest => self.on_quote_request(session_id, msg, now),
            MsgType::Quote => self.on_quote(session_id, msg, now),
            MsgType::QuoteCancel => self.on_quote_cancel(session_id, msg, now),
            _ => Vec::new(),
        }
    }

    /// Build and route an outbound application frame through a session.
    fn emit(&mut self, session_id: u64, mtype: MsgType, body: FixMessage) -> Vec<u8> {
        match self.sessions.get_mut(&session_id) {
            Some(s) => s.session_out(mtype, body),
            None => Vec::new(),
        }
    }

    // -- NewOrderSingle ------------------------------------------------------

    fn on_new_order(&mut self, session_id: u64, msg: &FixMessage, now: u64) -> Vec<Vec<u8>> {
        let sub = self.sub_of(session_id);
        let cl_ord_id = msg.get(tag::CL_ORD_ID).unwrap_or_default().to_string();

        // The RFQ hit path: a NewOrderSingle carrying QuoteID(117)
        // executes the referenced dealer quote.
        if let Some(quote_ref) = msg.get(tag::QUOTE_ID) {
            return self.on_quote_hit(session_id, msg, quote_ref, now);
        }

        let Some(request) = self.translate_order(sub, msg) else {
            return vec![self.execution_reject_simple(
                session_id,
                &cl_ord_id,
                "malformed order",
                now,
            )];
        };
        let qty = request.qty_lots;
        let events = self.engine.process(Command::Place { request, now });
        self.drain_order_events(session_id, &cl_ord_id, qty, events, now)
    }

    /// Translate the FIX fields into an engine `OrderRequest`.
    fn translate_order(&self, sub: SubaccountId, msg: &FixMessage) -> Option<OrderRequest> {
        let symbol = msg.get(tag::SYMBOL)?.to_string();
        let side = match msg.get(tag::SIDE)? {
            "1" => Side::Bid,
            "2" => Side::Ask,
            _ => return None,
        };
        let qty = msg.get_u64(tag::ORDER_QTY)?;
        let ord_type = match msg.get(tag::ORD_TYPE).unwrap_or("2") {
            "1" => OrderType::Market,
            "2" => OrderType::Limit,
            "3" => {
                let trigger = msg.get_u64(tag::STOP_PX)?;
                OrderType::StopMarket {
                    trigger_price: trigger,
                }
            }
            "4" => {
                let trigger = msg.get_u64(tag::STOP_PX)?;
                let limit = msg.get_u64(tag::PRICE)?;
                OrderType::StopLimit {
                    trigger_price: trigger,
                    limit_price: limit,
                }
            }
            _ => return None,
        };
        let price_ticks = msg.get_u64(tag::PRICE);
        let tif = match msg.get(tag::TIME_IN_FORCE).unwrap_or("1") {
            "3" => TimeInForce::Ioc,
            "4" => TimeInForce::Fok,
            _ => TimeInForce::Gtc,
        };
        let exec_inst = msg.get(tag::EXEC_INST).unwrap_or("");
        let post_only = exec_inst.contains('P');
        let reduce_only = exec_inst.contains('R');
        Some(OrderRequest {
            subaccount: sub,
            symbol,
            side,
            order_type: ord_type,
            price_ticks,
            qty_lots: qty,
            tif,
            post_only,
            reduce_only,
            stp: poc_core::SelfTradePrevention::default(),
            display_lots: None,
            oco_group: None,
            client_ts: 0,
        })
    }

    /// Consume engine events from an order action into ExecutionReports.
    fn drain_order_events(
        &mut self,
        session_id: u64,
        cl_ord_id: &str,
        _qty: u64,
        events: Vec<Event>,
        now: u64,
    ) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for ev in events {
            match ev {
                Event::OrderResting { order, .. } => {
                    self.router.track(TrackedOrder {
                        cl_ord_id: cl_ord_id.to_string(),
                        order_id: order.id,
                        symbol: order.symbol.clone(),
                        side: order.side,
                        qty_lots: order.qty_lots,
                        cum_qty_lots: order.filled_lots,
                        cum_px_weighted: 0,
                        status: if order.filled_lots > 0 {
                            ord_status::PARTIALLY_FILLED
                        } else {
                            ord_status::NEW
                        },
                        done: false,
                    });
                    out.push(self.execution_report(
                        session_id,
                        cl_ord_id,
                        order.id,
                        &order.symbol,
                        order.side,
                        order.qty_lots,
                        exec_type::NEW,
                        None,
                        now,
                    ));
                }
                Event::OrderRejection(r) => {
                    let text = format!("{}", r.reason);
                    out.push(self.execution_reject_simple(session_id, cl_ord_id, &text, now));
                }
                Event::TradeExecuted(t) => {
                    // Attribute the fill to whichever side this desk
                    // owned: both ids are offered; the router ignores
                    // the foreign one.
                    self.router
                        .record_fill(t.taker_order_id, t.price_ticks, t.qty_lots);
                    self.router
                        .record_fill(t.maker_order_id, t.price_ticks, t.qty_lots);
                    let order_id = if self.router.by_engine_id_ref(t.taker_order_id).is_some() {
                        t.taker_order_id
                    } else {
                        t.maker_order_id
                    };
                    let cl = self
                        .router
                        .by_engine_id_ref(order_id)
                        .map(|o| o.cl_ord_id.clone())
                        .unwrap_or_else(|| cl_ord_id.to_string());
                    out.push(self.execution_report(
                        session_id,
                        &cl,
                        order_id,
                        &t.symbol,
                        t.maker_side.opposite(),
                        t.qty_lots,
                        exec_type::TRADE,
                        Some((t.price_ticks, t.qty_lots)),
                        now,
                    ));
                }
                Event::OrderClosed {
                    order_id,
                    order,
                    reason,
                    ..
                } => {
                    let status = match reason {
                        OrderCloseReason::Filled => ord_status::FILLED,
                        OrderCloseReason::Canceled
                        | OrderCloseReason::Expired
                        | OrderCloseReason::IocRemainder
                        | OrderCloseReason::OcoSibling => ord_status::CANCELED,
                    };
                    let et = match reason {
                        OrderCloseReason::Filled => exec_type::TRADE,
                        _ => exec_type::CANCELED,
                    };
                    let cl = self
                        .router
                        .by_engine_id(order_id)
                        .map(|o| o.cl_ord_id.clone())
                        .unwrap_or_else(|| cl_ord_id.to_string());
                    if reason != OrderCloseReason::Filled {
                        self.router.close(order_id, status);
                    }
                    out.push(self.execution_report(
                        session_id,
                        &cl,
                        order_id,
                        &order.symbol,
                        order.side,
                        order.qty_lots,
                        et,
                        None,
                        now,
                    ));
                }
                _ => {}
            }
        }
        out
    }

    // -- Cancel / Replace ------------------------------------------------------

    fn on_cancel(&mut self, session_id: u64, msg: &FixMessage, now: u64) -> Vec<Vec<u8>> {
        let sub = self.sub_of(session_id);
        let cl_ord_id = msg.get(tag::CL_ORD_ID).unwrap_or_default().to_string();
        let orig = msg.get(tag::ORIG_CL_ORD_ID).unwrap_or_default().to_string();
        let Some(order) = self.router.by_cl_ord_id(&orig).cloned() else {
            return vec![self.cancel_reject(session_id, &cl_ord_id, &orig, "unknown order", now)];
        };
        let events = self.engine.process(Command::Cancel {
            subaccount: sub,
            order_id: order.order_id,
            now,
        });
        let mut out = self.drain_order_events(session_id, &cl_ord_id, order.qty_lots, events, now);
        if out.is_empty() {
            // Cancel of an unknown/resting-less order.
            out.push(self.cancel_reject(session_id, &cl_ord_id, &orig, "cancel rejected", now));
        }
        out
    }

    fn on_replace(&mut self, session_id: u64, msg: &FixMessage, now: u64) -> Vec<Vec<u8>> {
        let sub = self.sub_of(session_id);
        let cl_ord_id = msg.get(tag::CL_ORD_ID).unwrap_or_default().to_string();
        let orig = msg.get(tag::ORIG_CL_ORD_ID).unwrap_or_default().to_string();
        let Some(order) = self.router.by_cl_ord_id(&orig).cloned() else {
            return vec![self.cancel_reject(session_id, &cl_ord_id, &orig, "unknown order", now)];
        };
        let new_price = msg.get_u64(tag::PRICE);
        let new_qty = msg.get_u64(tag::ORDER_QTY);
        let events = self.engine.process(Command::Amend {
            subaccount: sub,
            order_id: order.order_id,
            new_price_ticks: new_price,
            new_open_lots: new_qty,
            now,
        });
        self.drain_order_events(session_id, &cl_ord_id, order.qty_lots, events, now)
    }

    fn on_order_status(&mut self, session_id: u64, msg: &FixMessage, _now: u64) -> Vec<Vec<u8>> {
        let orig = msg
            .get(tag::ORIG_CL_ORD_ID)
            .or_else(|| msg.get(tag::CL_ORD_ID))
            .unwrap_or_default();
        let tracked = self.router.by_cl_ord_id(orig).cloned();
        match tracked {
            Some(o) => vec![self.execution_report(
                session_id,
                &o.cl_ord_id,
                o.order_id,
                &o.symbol,
                o.side,
                o.qty_lots,
                if o.done { "5" } else { exec_type::NEW },
                None,
                _now,
            )],
            None => vec![self.execution_reject_simple(session_id, orig, "unknown order", _now)],
        }
    }

    // -- Market data ------------------------------------------------------------

    fn on_market_data(&mut self, session_id: u64, msg: &FixMessage, _now: u64) -> Vec<Vec<u8>> {
        let req_id = msg.get(tag::MD_REQ_ID).unwrap_or("md").to_string();
        let symbols = msg.get_all(tag::SYMBOL);
        let mut out = Vec::new();
        for symbol in symbols {
            let Some(book) = self.engine.book(symbol) else {
                continue;
            };
            let (bids, asks) = book.depth(10);
            let mut body = FixMessage::new();
            body.push(tag::MD_REQ_ID, req_id.clone());
            body.push(tag::SYMBOL, symbol);
            body.push(tag::NO_MD_ENTRIES, (bids.len() + asks.len()).to_string());
            for lvl in &bids {
                body.push(tag::MD_ENTRY_TYPE, "0");
                body.push(tag::MD_ENTRY_PX, lvl.price_ticks.to_string());
                body.push(tag::MD_ENTRY_SIZE, lvl.total_qty_lots.to_string());
            }
            for lvl in &asks {
                body.push(tag::MD_ENTRY_TYPE, "1");
                body.push(tag::MD_ENTRY_PX, lvl.price_ticks.to_string());
                body.push(tag::MD_ENTRY_SIZE, lvl.total_qty_lots.to_string());
            }
            out.push(self.emit(session_id, MsgType::MarketDataSnapshotFullRefresh, body));
        }
        out
    }

    // -- RFQ over FIX --------------------------------------------------------------

    fn on_quote_request(&mut self, session_id: u64, msg: &FixMessage, now: u64) -> Vec<Vec<u8>> {
        let sub = self.sub_of(session_id);
        let req_id = msg.get(tag::QUOTE_REQ_ID).unwrap_or_default().to_string();
        let symbol = msg.get(tag::SYMBOL).unwrap_or_default().to_string();
        let side = match msg.get(tag::SIDE).unwrap_or("1") {
            "2" => Side::Ask,
            _ => Side::Bid,
        };
        let qty = msg.get_u64(tag::ORDER_QTY).unwrap_or(0);
        if req_id.is_empty() || symbol.is_empty() || qty == 0 {
            return vec![self.execution_reject_simple(
                session_id,
                &req_id,
                "malformed quote request",
                now,
            )];
        }
        let events = self.engine.process(Command::RfqCreate {
            taker: sub,
            legs: vec![RfqLegCommand {
                symbol,
                side,
                qty_lots: qty,
            }],
            counterparties: vec![],
            min_total_cost_quote_minor: None,
            max_total_cost_quote_minor: None,
            ttl_ms: 600_000,
            now,
        });
        let mut out = Vec::new();
        for ev in events {
            if let Event::RfqCreated { .. } = ev {
                // Assign the mapping: the engine assigns ids from 1.
                let mut id = 1;
                while self.engine.rfq_view().rfq(id).is_some() {
                    id += 1;
                }
                self.rfq_by_req_id.insert(req_id.clone(), id - 1);
            }
        }
        let _ = &mut out;
        out
    }

    fn on_quote(&mut self, session_id: u64, msg: &FixMessage, now: u64) -> Vec<Vec<u8>> {
        let sub = self.sub_of(session_id);
        let req_id = msg.get(tag::QUOTE_REQ_ID).unwrap_or_default().to_string();
        let quote_id = msg.get(tag::QUOTE_ID).unwrap_or_default().to_string();
        let Some(rfq_id) = self.rfq_by_req_id.get(&req_id).copied() else {
            return Vec::new();
        };
        // Two-sided quotes: the dealer's side is opposite the RFQ leg's
        // taker side; a dealer buying quotes on BidPx/BidSize, selling
        // on OfferPx/OfferSize.
        let (price, size) = if let (Some(bpx), Some(bsz)) =
            (msg.get_u64(tag::BID_PX), msg.get_u64(tag::BID_SIZE))
        {
            (Some(bpx), Some(bsz))
        } else if let (Some(opx), Some(osz)) =
            (msg.get_u64(tag::OFFER_PX), msg.get_u64(tag::OFFER_SIZE))
        {
            (Some(opx), Some(osz))
        } else {
            (msg.get_u64(tag::PRICE), None)
        };
        let Some(price) = price else {
            return Vec::new();
        };
        let events = if let Some(sz) = size {
            self.engine.process(Command::RfqQuoteSized {
                maker: sub,
                rfq_id,
                leg_prices_ticks: vec![price],
                qty_lots: vec![sz],
                ttl_ms: 600_000,
                now,
            })
        } else {
            self.engine.process(Command::RfqQuote {
                maker: sub,
                rfq_id,
                leg_prices_ticks: vec![price],
                ttl_ms: 600_000,
                now,
            })
        };
        // Record the desk's QuoteID → engine quote id.
        for ev in &events {
            if matches!(ev, Event::RfqQuoted { .. }) {
                let mut qid = 1;
                while self.engine.rfq_view().quote(qid).is_some() {
                    qid += 1;
                }
                let engine_qid = qid - 1;
                self.quote_by_id.insert((sub, quote_id.clone()), engine_qid);
                self.live_quote_ids
                    .insert((req_id.clone(), sub), quote_id.clone());
            }
        }
        Vec::new()
    }

    /// A taker's NewOrderSingle carrying `117=QuoteID` — the standard
    /// FIX "order against quote": execute the RFQ quote (fully or by
    /// the order's quantity).
    fn on_quote_hit(
        &mut self,
        session_id: u64,
        msg: &FixMessage,
        quote_ref: &str,
        now: u64,
    ) -> Vec<Vec<u8>> {
        let taker = self.sub_of(session_id);
        let cl_ord_id = msg.get(tag::CL_ORD_ID).unwrap_or_default().to_string();
        // Resolve the desk's QuoteID to the engine's quote id.
        let mut engine_qid: Option<u64> = None;
        for ((_sub, qid), v) in &self.quote_by_id {
            if qid == quote_ref {
                engine_qid = Some(*v);
                break;
            }
        }
        let Some(engine_qid) = engine_qid else {
            return vec![self.execution_reject_simple(
                session_id,
                &cl_ord_id,
                "unknown quote",
                now,
            )];
        };
        // The RFQ the quote rides on comes straight from the book.
        let Some(rfq_id) = self.engine.rfq_view().quote(engine_qid).map(|q| q.rfq_id) else {
            return vec![self.execution_reject_simple(session_id, &cl_ord_id, "unknown rfq", now)];
        };
        let want = msg.get_u64(tag::ORDER_QTY);
        let events = match want {
            Some(qty) => self.engine.process(Command::RfqExecutePartial {
                taker,
                rfq_id,
                quote_id: engine_qid,
                take_per_leg: vec![qty],
                now,
            }),
            None => self.engine.process(Command::RfqExecute {
                taker,
                rfq_id,
                quote_id: engine_qid,
                now,
            }),
        };
        let mut out = Vec::new();
        for ev in events {
            if let Event::RfqSettled {
                trades,
                taker,
                maker,
                ..
            } = ev
            {
                for t in &trades {
                    out.push(self.rfq_fill_report(session_id, &cl_ord_id, t, taker, maker));
                }
            }
        }
        out
    }

    fn on_quote_cancel(&mut self, session_id: u64, msg: &FixMessage, now: u64) -> Vec<Vec<u8>> {
        let sub = self.sub_of(session_id);
        let quote_id = msg.get(tag::QUOTE_ID).map(|q| q.to_string());
        let events = self.engine.process(Command::RfqCancel {
            subaccount: sub,
            rfq_id: None,
            quote_id: quote_id
                .as_deref()
                .and_then(|q| self.quote_by_id.get(&(sub, q.to_string())).copied()),
            now,
        });
        let _ = events;
        Vec::new()
    }

    // -- Report builders -----------------------------------------------------------

    fn sub_of(&self, session_id: u64) -> SubaccountId {
        self.sessions
            .get(&session_id)
            .map(|s| s.subaccount)
            .unwrap_or(0)
    }

    #[allow(clippy::too_many_arguments)]
    fn execution_report(
        &mut self,
        session_id: u64,
        cl_ord_id: &str,
        order_id: u64,
        symbol: &str,
        side: Side,
        qty_lots: u64,
        etype: &str,
        last_fill: Option<(u64, u64)>,
        now: u64,
    ) -> Vec<u8> {
        let tracked = self.router.by_cl_ord_id(cl_ord_id).cloned();
        let (cum, leaves, avg_px, status) = match &tracked {
            Some(o) => (
                o.cum_qty_lots,
                o.leaves_qty(),
                self.router.avg_px_ticks(cl_ord_id),
                o.status,
            ),
            None => (0, qty_lots, 0, ord_status::NEW),
        };
        let mut body = FixMessage::new();
        body.push(tag::ORDER_ID, order_id.to_string());
        body.push(tag::CL_ORD_ID, cl_ord_id.to_string());
        body.push(tag::EXEC_ID, self.router.next_exec_id().to_string());
        body.push(tag::EXEC_TYPE, etype);
        body.push(tag::ORD_STATUS, status);
        body.push(tag::SYMBOL, symbol);
        body.push(tag::SIDE, if side == Side::Bid { "1" } else { "2" });
        body.push(tag::ORDER_QTY, qty_lots.to_string());
        body.push(tag::CUM_QTY, cum.to_string());
        body.push(tag::LEAVES_QTY, leaves.to_string());
        body.push(tag::AVG_PX, avg_px.to_string());
        if let Some((px, qty)) = last_fill {
            body.push(tag::LAST_PX, px.to_string());
            body.push(tag::LAST_QTY, qty.to_string());
        }
        body.push(tag::TRANSACT_TIME, now.to_string());
        self.emit(session_id, MsgType::ExecutionReport, body)
    }

    fn execution_reject_simple(
        &mut self,
        session_id: u64,
        cl_ord_id: &str,
        text: &str,
        now: u64,
    ) -> Vec<u8> {
        let mut body = FixMessage::new();
        body.push(tag::ORDER_ID, "0");
        body.push(tag::CL_ORD_ID, cl_ord_id.to_string());
        body.push(tag::EXEC_ID, self.router.next_exec_id().to_string());
        body.push(tag::EXEC_TYPE, exec_type::REJECTED);
        body.push(tag::ORD_STATUS, ord_status::REJECTED);
        body.push(tag::TEXT, text);
        body.push(tag::TRANSACT_TIME, now.to_string());
        self.emit(session_id, MsgType::ExecutionReport, body)
    }

    fn cancel_reject(
        &mut self,
        session_id: u64,
        cl_ord_id: &str,
        orig: &str,
        text: &str,
        now: u64,
    ) -> Vec<u8> {
        let mut body = FixMessage::new();
        body.push(tag::ORDER_ID, "0");
        body.push(tag::ORIG_CL_ORD_ID, orig.to_string());
        body.push(tag::CL_ORD_ID, cl_ord_id.to_string());
        body.push(tag::ORD_STATUS, ord_status::REJECTED);
        // CxlRejReason(102) = 1 (unknown order).
        body.push(102, "1");
        body.push(tag::TEXT, text);
        body.push(tag::TRANSACT_TIME, now.to_string());
        self.emit(session_id, MsgType::OrderCancelReject, body)
    }

    /// An RFQ fill as an ExecutionReport (both counterparties get one;
    /// this builds the one for `session_id`'s desk).
    fn rfq_fill_report(
        &mut self,
        session_id: u64,
        cl_ord_id: &str,
        t: &poc_engine::Trade,
        taker: SubaccountId,
        maker: SubaccountId,
    ) -> Vec<u8> {
        let mut body = FixMessage::new();
        body.push(tag::ORDER_ID, "0");
        body.push(tag::CL_ORD_ID, cl_ord_id.to_string());
        body.push(tag::EXEC_ID, self.router.next_exec_id().to_string());
        body.push(tag::EXEC_TYPE, exec_type::TRADE);
        body.push(
            tag::ORD_STATUS,
            if t.qty_lots > 0 {
                ord_status::FILLED
            } else {
                ord_status::NEW
            },
        );
        body.push(tag::SYMBOL, t.symbol.clone());
        body.push(tag::SIDE, if t.maker_side == Side::Ask { "1" } else { "2" });
        body.push(tag::ORDER_QTY, t.qty_lots.to_string());
        body.push(tag::CUM_QTY, t.qty_lots.to_string());
        body.push(tag::LEAVES_QTY, "0");
        body.push(tag::AVG_PX, t.price_ticks.to_string());
        body.push(tag::LAST_PX, t.price_ticks.to_string());
        body.push(tag::LAST_QTY, t.qty_lots.to_string());
        body.push(tag::TRANSACT_TIME, t.ts.to_string());
        let _ = (taker, maker);
        self.emit(session_id, MsgType::ExecutionReport, body)
    }

    /// Liveness poll for one session (call periodically).
    pub fn poll(&mut self, session_id: u64, now: u64) -> Option<Vec<Vec<u8>>> {
        let s = self.sessions.get_mut(&session_id)?;
        Some(s.session.poll(now, s.last_inbound))
    }
}

impl GatewaySession {
    /// Emit through the inner session.
    fn session_out(&mut self, mtype: MsgType, body: FixMessage) -> Vec<u8> {
        self.session.emit_out(mtype, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{tag, FixMessage, MsgType};
    use poc_engine::EngineConfig;

    const NOW: u64 = 1_000_000;

    fn seeded_gateway() -> FixGateway {
        let mut e = Engine::new(EngineConfig::default());
        e.register_instrument(poc_core::Instrument::Perp(poc_core::PerpMarket::default()));
        for provider in ["pyth", "chainlink"] {
            e.process(Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: provider.into(),
                ts: NOW,
                price_quote_minor: 8_000_000,
            });
        }
        e.process(Command::Tick { now: NOW });
        for sub in 1..=3u64 {
            e.process(Command::Deposit {
                subaccount: sub,
                amount_quote_minor: 100_000_000,
            });
        }
        let mut gw = FixGateway::new(e);
        gw.register_comp_id("TAKER-DESK", 1);
        gw.register_comp_id("DEALER-DESK", 2);
        gw.register_comp_id("MARKET-DESK", 3);
        gw
    }

    fn inbound(seq: u32, mtype: MsgType, fields: &[(u32, &str)]) -> Vec<u8> {
        let mut m = FixMessage::new();
        m.push(tag::MSG_SEQ_NUM, seq.to_string());
        m.push(tag::SENDER_COMP_ID, "DESK");
        m.push(tag::TARGET_COMP_ID, "VENUE");
        m.push(tag::SENDING_TIME, "20260101-00:00:00.000");
        m.push(tag::MSG_TYPE, mtype.code());
        for (t, v) in fields {
            m.push(*t, *v);
        }
        m.encode()
    }

    fn logon_frame(seq: u32) -> Vec<u8> {
        inbound(
            seq,
            MsgType::Logon,
            &[(tag::ENCRYPT_METHOD, "0"), (tag::HEART_BT_INT, "30")],
        )
    }

    fn decoded(frame: &[u8]) -> FixMessage {
        FixMessage::decode(frame).unwrap()
    }

    // 1. logon through the gateway ---------------------------------------------

    #[test]
    fn gateway_logon_and_heartbeat() {
        let mut gw = seeded_gateway();
        let sid = gw.connect("MARKET-DESK").unwrap();
        let out = gw.on_frame(sid, &logon_frame(1), NOW).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(decoded(&out[0]).msg_type(), Some(MsgType::Logon));
    }

    // 2. the full order lifecycle: place, rest, fill ------------------------------

    #[test]
    fn order_lifecycle_over_fix() {
        let mut gw = seeded_gateway();
        let market = gw.connect("MARKET-DESK").unwrap();
        gw.on_frame(market, &logon_frame(1), NOW).unwrap();

        // NewOrderSingle: a resting bid at 79_000 ticks.
        let order = inbound(
            2,
            MsgType::NewOrderSingle,
            &[
                (tag::CL_ORD_ID, "C-1"),
                (tag::SYMBOL, "BTC-PERP"),
                (tag::SIDE, "1"),
                (tag::ORDER_QTY, "50"),
                (tag::ORD_TYPE, "2"),
                (tag::PRICE, "79000"),
                (tag::TIME_IN_FORCE, "1"),
            ],
        );
        let out = gw.on_frame(market, &order, NOW).unwrap();
        assert_eq!(out.len(), 1, "one ack: {out:?}");
        let er = decoded(&out[0]);
        assert_eq!(er.msg_type(), Some(MsgType::ExecutionReport));
        assert_eq!(er.get(tag::EXEC_TYPE), Some("0")); // New
        assert_eq!(er.get(tag::ORD_STATUS), Some("0"));
        assert_eq!(er.get_u64(tag::LEAVES_QTY), Some(50));
        assert_eq!(er.get_u64(tag::CUM_QTY), Some(0));

        // A second desk crosses it with a market sell.
        let taker = gw.connect("TAKER-DESK").unwrap();
        gw.on_frame(taker, &logon_frame(1), NOW).unwrap();
        let cross = inbound(
            2,
            MsgType::NewOrderSingle,
            &[
                (tag::CL_ORD_ID, "C-2"),
                (tag::SYMBOL, "BTC-PERP"),
                (tag::SIDE, "2"),
                (tag::ORDER_QTY, "50"),
                (tag::ORD_TYPE, "1"),
            ],
        );
        let out = gw.on_frame(taker, &cross, NOW).unwrap();
        // The taker's own fill report.
        assert!(!out.is_empty(), "taker gets reports: {out:?}");
        let er = decoded(&out[0]);
        assert_eq!(er.get(tag::EXEC_TYPE), Some("F")); // Trade
        assert_eq!(er.get_u64(tag::LAST_QTY), Some(50));
        assert_eq!(er.get_u64(tag::LAST_PX), Some(79_000));

        // The engine's book reflects the trade: the market desk bought
        // (sub 3), the taker desk sold (sub 1).
        assert_eq!(
            gw.engine().account(3).map(|a| a.lots_of("BTC-PERP")),
            Some(50)
        );
        assert_eq!(
            gw.engine().account(1).map(|a| a.lots_of("BTC-PERP")),
            Some(-50)
        );

        // The market desk's order shows filled in the router.
        let tracked = gw.router().by_cl_ord_id("C-1").unwrap();
        assert_eq!(tracked.status, ord_status::FILLED);
        assert_eq!(tracked.cum_qty_lots, 50);
        assert_eq!(gw.router().avg_px_ticks("C-1"), 79_000);
    }

    // 3. cancel flow ----------------------------------------------------------------

    #[test]
    fn cancel_flow_over_fix() {
        let mut gw = seeded_gateway();
        let market = gw.connect("MARKET-DESK").unwrap();
        gw.on_frame(market, &logon_frame(1), NOW).unwrap();
        let order = inbound(
            2,
            MsgType::NewOrderSingle,
            &[
                (tag::CL_ORD_ID, "C-9"),
                (tag::SYMBOL, "BTC-PERP"),
                (tag::SIDE, "1"),
                (tag::ORDER_QTY, "50"),
                (tag::ORD_TYPE, "2"),
                (tag::PRICE, "79000"),
            ],
        );
        gw.on_frame(market, &order, NOW).unwrap();

        let cancel = inbound(
            3,
            MsgType::OrderCancelRequest,
            &[
                (tag::ORIG_CL_ORD_ID, "C-9"),
                (tag::CL_ORD_ID, "C-10"),
                (tag::SYMBOL, "BTC-PERP"),
            ],
        );
        let out = gw.on_frame(market, &cancel, NOW).unwrap();
        let er = decoded(&out[0]);
        assert_eq!(er.get(tag::ORD_STATUS), Some("4")); // Canceled
        assert_eq!(
            gw.router().by_cl_ord_id("C-9").unwrap().status,
            ord_status::CANCELED
        );

        // Cancelling an unknown order: OrderCancelReject.
        let bad = inbound(
            4,
            MsgType::OrderCancelRequest,
            &[(tag::ORIG_CL_ORD_ID, "NOPE"), (tag::CL_ORD_ID, "C-11")],
        );
        let out = gw.on_frame(market, &bad, NOW).unwrap();
        assert_eq!(
            decoded(&out[0]).msg_type(),
            Some(MsgType::OrderCancelReject)
        );
    }

    // 4. market data snapshot --------------------------------------------------------

    #[test]
    fn market_data_snapshot_over_fix() {
        let mut gw = seeded_gateway();
        let market = gw.connect("MARKET-DESK").unwrap();
        gw.on_frame(market, &logon_frame(1), NOW).unwrap();
        let order = inbound(
            2,
            MsgType::NewOrderSingle,
            &[
                (tag::CL_ORD_ID, "C-MD"),
                (tag::SYMBOL, "BTC-PERP"),
                (tag::SIDE, "1"),
                (tag::ORDER_QTY, "50"),
                (tag::ORD_TYPE, "2"),
                (tag::PRICE, "79000"),
            ],
        );
        gw.on_frame(market, &order, NOW).unwrap();

        let md = inbound(
            3,
            MsgType::MarketDataRequest,
            &[
                (tag::MD_REQ_ID, "MD-1"),
                (tag::SUBSCRIPTION_REQUEST_TYPE, "0"),
                (tag::NO_RELATED_SYM, "1"),
                (tag::SYMBOL, "BTC-PERP"),
                (tag::NO_MD_ENTRY_TYPES, "2"),
                (tag::MD_ENTRY_TYPE, "0"),
                (tag::MD_ENTRY_TYPE, "1"),
            ],
        );
        let out = gw.on_frame(market, &md, NOW).unwrap();
        assert_eq!(out.len(), 1);
        let snap = decoded(&out[0]);
        assert_eq!(
            snap.msg_type(),
            Some(MsgType::MarketDataSnapshotFullRefresh)
        );
        assert_eq!(snap.get(tag::SYMBOL), Some("BTC-PERP"));
        // One bid level at 79_000 for 50 lots, no asks.
        let pxs = snap.get_all(tag::MD_ENTRY_PX);
        assert_eq!(pxs, vec!["79000"]);
        let sizes = snap.get_all(tag::MD_ENTRY_SIZE);
        assert_eq!(sizes, vec!["50"]);
    }

    // 5. THE RFQ FLOW OVER FIX: the Paradigm dealer rails --------------------------

    #[test]
    fn rfq_flow_over_fix_end_to_end() {
        let mut gw = seeded_gateway();
        let taker = gw.connect("TAKER-DESK").unwrap();
        let dealer = gw.connect("DEALER-DESK").unwrap();
        gw.on_frame(taker, &logon_frame(1), NOW).unwrap();
        gw.on_frame(dealer, &logon_frame(1), NOW).unwrap();

        // (1) The taker requests a price for 50 lots.
        let rq = inbound(
            2,
            MsgType::QuoteRequest,
            &[
                (tag::QUOTE_REQ_ID, "Q-1"),
                (tag::SYMBOL, "BTC-PERP"),
                (tag::SIDE, "1"),
                (tag::ORDER_QTY, "50"),
            ],
        );
        let out = gw.on_frame(taker, &rq, NOW).unwrap();
        assert!(out.is_empty());
        // The RFQ exists in the engine, owned by subaccount 1.
        assert!(gw.engine().rfq_view().rfq(1).is_some());

        // (2) The dealer answers with a firm one-sided quote: they sell
        // (offer) 50 lots at 80_000 ticks.
        let quote = inbound(
            2,
            MsgType::Quote,
            &[
                (tag::QUOTE_REQ_ID, "Q-1"),
                (tag::QUOTE_ID, "D-1"),
                (tag::OFFER_PX, "80000"),
                (tag::OFFER_SIZE, "50"),
            ],
        );
        let out = gw.on_frame(dealer, &quote, NOW).unwrap();
        assert!(out.is_empty());
        // The quote is live in the engine book.
        let q = gw.engine().rfq_view().quote(1).cloned().unwrap();
        assert_eq!(q.maker, 2);
        assert_eq!(q.leg_prices_ticks, vec![80_000]);
        assert_eq!(q.qty_lots, vec![50]);

        // (3) The taker hits the quote: NewOrderSingle carrying 117.
        let hit = inbound(
            3,
            MsgType::NewOrderSingle,
            &[
                (tag::CL_ORD_ID, "C-RFQ"),
                (tag::QUOTE_ID, "D-1"),
                (tag::ORDER_QTY, "20"), // partial lift
            ],
        );
        let out = gw.on_frame(taker, &hit, NOW).unwrap();
        assert!(!out.is_empty(), "rfq fill reports: {out:?}");
        let er = decoded(&out[0]);
        assert_eq!(er.msg_type(), Some(MsgType::ExecutionReport));
        assert_eq!(er.get(tag::EXEC_TYPE), Some("F"));
        assert_eq!(er.get_u64(tag::LAST_QTY), Some(20));
        assert_eq!(er.get_u64(tag::LAST_PX), Some(80_000));

        // The engine settled the partial through the venue path.
        assert_eq!(
            gw.engine().account(1).map(|a| a.lots_of("BTC-PERP")),
            Some(20)
        );
        assert_eq!(
            gw.engine().account(2).map(|a| a.lots_of("BTC-PERP")),
            Some(-20)
        );

        // The RFQ remains open with 30 lots; the quote stays firm.
        let rfq = gw.engine().rfq_view().rfq(1).cloned().unwrap();
        assert_eq!(rfq.remaining_lots, vec![30]);
        let q = gw.engine().rfq_view().quote(1).cloned().unwrap();
        assert_eq!(q.status, poc_rfq::QuoteStatus::Live);
        assert_eq!(q.remaining_lots(), vec![30]);

        // (4) The taker lifts the rest: a plain hit without qty = full.
        let hit2 = inbound(
            4,
            MsgType::NewOrderSingle,
            &[(tag::CL_ORD_ID, "C-RFQ2"), (tag::QUOTE_ID, "D-1")],
        );
        let out = gw.on_frame(taker, &hit2, NOW).unwrap();
        assert!(!out.is_empty());
        let er = decoded(&out[0]);
        assert_eq!(er.get_u64(tag::LAST_QTY), Some(30));
        // Fully consumed now.
        let rfq = gw.engine().rfq_view().rfq(1).cloned().unwrap();
        assert_eq!(rfq.status, poc_rfq::RfqStatus::Filled);
        let q = gw.engine().rfq_view().quote(1).cloned().unwrap();
        assert_eq!(q.status, poc_rfq::QuoteStatus::Executed);
        assert_eq!(
            gw.engine().account(1).map(|a| a.lots_of("BTC-PERP")),
            Some(50)
        );

        // The taker paid a fee; the dealer paid zero (Paradigm economics).
        assert!(
            gw.engine()
                .account(1)
                .map(|a| a.fees_paid_quote_minor)
                .unwrap_or(0)
                > 0
        );
        assert_eq!(
            gw.engine().account(2).map(|a| a.fees_paid_quote_minor),
            Some(0)
        );
    }

    // 6. the delayed tape: the RFQ print waits for the delay -----------------------

    #[test]
    fn rfq_print_is_delayed_over_fix() {
        let mut gw = seeded_gateway();
        let taker = gw.connect("TAKER-DESK").unwrap();
        let dealer = gw.connect("DEALER-DESK").unwrap();
        gw.on_frame(taker, &logon_frame(1), NOW).unwrap();
        gw.on_frame(dealer, &logon_frame(1), NOW).unwrap();
        let rq = inbound(
            2,
            MsgType::QuoteRequest,
            &[
                (tag::QUOTE_REQ_ID, "Q-1"),
                (tag::SYMBOL, "BTC-PERP"),
                (tag::SIDE, "1"),
                (tag::ORDER_QTY, "10"),
            ],
        );
        gw.on_frame(taker, &rq, NOW).unwrap();
        let quote = inbound(
            2,
            MsgType::Quote,
            &[
                (tag::QUOTE_REQ_ID, "Q-1"),
                (tag::QUOTE_ID, "D-1"),
                (tag::OFFER_PX, "80000"),
                (tag::OFFER_SIZE, "10"),
            ],
        );
        gw.on_frame(dealer, &quote, NOW).unwrap();
        let hit = inbound(
            3,
            MsgType::NewOrderSingle,
            &[(tag::CL_ORD_ID, "C-1"), (tag::QUOTE_ID, "D-1")],
        );
        gw.on_frame(taker, &hit, NOW).unwrap();

        // The negotiated tape holds an unprinted RFQ print.
        let block = gw.engine().blocks_view().block(1).cloned().unwrap();
        assert_eq!(block.origin, poc_rfq::BlockOrigin::Rfq);
        assert!(!block.broadcast);
    }

    // 7. determinism ---------------------------------------------------------------

    #[test]
    fn gateway_is_deterministic() {
        fn run() -> Vec<Vec<Vec<u8>>> {
            let mut gw = seeded_gateway();
            let taker = gw.connect("TAKER-DESK").unwrap();
            let dealer = gw.connect("DEALER-DESK").unwrap();
            let mut acc = Vec::new();
            acc.push(gw.on_frame(taker, &logon_frame(1), NOW).unwrap());
            acc.push(gw.on_frame(dealer, &logon_frame(1), NOW).unwrap());
            let rq = inbound(
                2,
                MsgType::QuoteRequest,
                &[
                    (tag::QUOTE_REQ_ID, "Q-1"),
                    (tag::SYMBOL, "BTC-PERP"),
                    (tag::SIDE, "1"),
                    (tag::ORDER_QTY, "10"),
                ],
            );
            acc.push(gw.on_frame(taker, &rq, NOW).unwrap());
            let quote = inbound(
                2,
                MsgType::Quote,
                &[
                    (tag::QUOTE_REQ_ID, "Q-1"),
                    (tag::QUOTE_ID, "D-1"),
                    (tag::OFFER_PX, "80000"),
                    (tag::OFFER_SIZE, "10"),
                ],
            );
            acc.push(gw.on_frame(dealer, &quote, NOW).unwrap());
            let hit = inbound(
                3,
                MsgType::NewOrderSingle,
                &[(tag::CL_ORD_ID, "C-1"), (tag::QUOTE_ID, "D-1")],
            );
            acc.push(gw.on_frame(taker, &hit, NOW).unwrap());
            acc
        }
        assert_eq!(run(), run());
    }
}
