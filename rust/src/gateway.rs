//! The network gateway: connection lifecycle, REST routing and the
//! WebSocket session protocol (see `docs/0DTE_API.md`).
//!
//! Connection model: one reader thread per TCP connection (parses HTTP,
//! routes REST, runs the WS session) plus one writer thread that drains a
//! bounded outbox of pre-serialized WS frames and enforces ping deadlines.
//! REST responses are written directly under the stream's write lock, so a
//! request's answer never queues behind market-data fan-out.
//!
//! Abuse resistance (every knob enforced, not documented-only):
//! * slowloris: header-read deadline;
//! * oversized requests: 413 (body cap) / 400 (header cap);
//! * replayed/stale/forged auth: 401 with the specific reason;
//! * key brute force: constant-time MAC compare + counted failures;
//! * WS frame attacks: RFC-validated codec (mask, RSV, control rules,
//!   fragmentation caps) — violations close 1002/1009;
//! * slow consumers: bounded outbox → disconnect;
//! * message floods: per-connection token bucket;
//! * subscription floods: cap + unknown-key rejection;
//! * pipelining/smuggling: rejected at the HTTP parser.

use crate::auth::{AuthErr, Identity, Scope};
use crate::engine::Cmd;
use crate::http::{query_pairs, response, HttpParser, Method, Parsed, Request};
use crate::json::{parse as json_parse, Json};
use crate::types::*;
use crate::venue::{chan, Await, SubKey, VenueCtx, NANOS_PER_MS};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Serve one connection to completion (reader thread body).
pub fn serve_conn(
    ctx: Arc<VenueCtx>,
    stream: TcpStream,
    ip: IpAddr,
    ip_counts: Arc<Mutex<HashMap<IpAddr, u16>>>,
) {
    ctx.metrics.conns.fetch_add(1, Ordering::Relaxed);
    let _guard = ConnGuard {
        ctx: Some(ctx.clone()),
        ip,
        ip_counts: Some(ip_counts.clone()),
    };
    let stream = match stream.set_nodelay(true) {
        Ok(_) => stream,
        Err(_) => return,
    };
    let mut reader = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let writer = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let write_lock = Arc::new(Mutex::new(writer));
    // Outbox for WS frames (bounded).
    let (tx_outbox, rx_outbox) = sync_channel::<Vec<u8>>(ctx.cfg.ws_outbox_depth);
    // Writer thread: drains outbox + pings.
    let ctx_w = ctx.clone();
    let wl = write_lock.clone();
    let writer_handle = std::thread::spawn(move || writer_loop(ctx_w, wl, rx_outbox));
    let result = conn_loop(&ctx, &mut reader, &write_lock, &tx_outbox, ip);
    drop(tx_outbox);
    let _ = writer_handle.join();
    let _ = result;
}

/// RAII connection accounting.
struct ConnGuard {
    ctx: Option<Arc<VenueCtx>>,
    ip: IpAddr,
    ip_counts: Option<Arc<Mutex<HashMap<IpAddr, u16>>>>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if let Some(ctx) = self.ctx.take() {
            ctx.metrics.conns.fetch_sub(1, Ordering::Relaxed);
        }
        if let Some(counts) = self.ip_counts.take() {
            if let Ok(mut g) = counts.lock() {
                if let Some(c) = g.get_mut(&self.ip) {
                    *c = c.saturating_sub(1);
                    if *c == 0 {
                        g.remove(&self.ip);
                    }
                }
            }
        }
    }
}

/// The per-connection protocol loop: keep-alive REST or one WS session.
fn conn_loop(
    ctx: &Arc<VenueCtx>,
    reader: &mut TcpStream,
    write_lock: &Arc<Mutex<TcpStream>>,
    tx_outbox: &SyncSender<Vec<u8>>,
    ip: IpAddr,
) -> std::io::Result<()> {
    let mut parser = HttpParser::new();
    let mut buf = [0u8; 8 * 1024];
    loop {
        if ctx.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        // Header deadline while waiting for a request.
        reader.set_read_timeout(Some(Duration::from_millis(ctx.cfg.header_deadline_ms)))?;
        // Byte-at-a-time would be slow; read in chunks but stop feeding once
        // a request completes (the parser tells us).
        let mut got = match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(_) => return Ok(()), // timeout / reset
        };
        loop {
            match parser.feed(&buf[..got]) {
                Parsed::NeedMore => {
                    reader.set_read_timeout(Some(Duration::from_millis(ctx.cfg.header_deadline_ms)))?;
                    got = match reader.read(&mut buf) {
                        Ok(0) => return Ok(()),
                        Ok(n) => n,
                        Err(_) => return Ok(()),
                    };
                }
                Parsed::Bad(why) => {
                    let body = format!("{{\"error\":\"Malformed\",\"msg\":\"{why}\"}}");
                    write_all(
                        write_lock,
                        &response(400, "application/json", body.as_bytes(), &[], true),
                    )?;
                    return Ok(());
                }
                Parsed::Done(req) => {
                    ctx.metrics.rest_requests.fetch_add(1, Ordering::Relaxed);
                    let close = req.close;
                    // WebSocket upgrade?
                    if is_ws_upgrade(&req) {
                        return ws_session(ctx, reader, write_lock, tx_outbox, *req, ip);
                    }
                    let (status, body, extra) = route(ctx, *req);
                    let extra_owned: Vec<String> = extra.into_iter().map(|s| s.to_string()).collect();
                    let close_hdr = close
                        || status == 429
                        || ctx.draining.load(Ordering::Acquire);
                    write_all(
                        write_lock,
                        &response(status, "application/json", body.as_bytes(), &extra_owned, close_hdr),
                    )?;
                    if close_hdr {
                        return Ok(());
                    }
                    break;
                }
            }
        }
    }
}

/// Raw write under the connection's write lock.
fn write_all(lock: &Arc<Mutex<TcpStream>>, bytes: &[u8]) -> std::io::Result<()> {
    let mut w = lock.lock().unwrap();
    w.write_all(bytes)
}

fn is_ws_upgrade(req: &Request) -> bool {
    let up = req
        .header("upgrade")
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    let conn = req
        .header("connection")
        .map(|v| v.to_ascii_lowercase().contains("upgrade"))
        .unwrap_or(false);
    up && conn && req.path() == "/v1/ws"
}

// ----------------------------------------------------------------------
// REST routing
// ----------------------------------------------------------------------

/// Route one request: (status, body, extra headers).
fn route(ctx: &Arc<VenueCtx>, req: Request) -> (u16, String, Vec<&'static str>) {
    let method = req.method;
    let path = req.path().to_string();

    // ---- no-auth endpoints ----
    if method == Method::Options {
        return (200, "{\"ok\":true}".into(), vec!["Access-Control-Max-Age: 600"]);
    }
    if method == Method::Get && path == "/health" {
        let mut w = crate::json::Writer::new();
        w.begin_obj();
        w.key("ok");
        w.bool(true);
        w.comma();
        w.key("uptime_ms");
        w.uint(ctx.uptime_ms());
        w.comma();
        w.key("shards");
        w.uint(ctx.queues.len() as u64);
        w.comma();
        w.key("draining");
        w.bool(ctx.draining());
        w.end_obj();
        return (200, w.finish(), vec![]);
    }
    if method == Method::Get && path == "/v1/time" {
        let mut w = crate::json::Writer::new();
        w.begin_obj();
        w.key("ms");
        w.uint(ctx.now_ms());
        w.comma();
        w.key("nanos");
        w.uint(ctx.now());
        w.end_obj();
        return (200, w.finish(), vec![]);
    }

    if ctx.draining() {
        return (
            503,
            "{\"error\":\"Draining\",\"msg\":\"venue is shutting down\"}".into(),
            vec![],
        );
    }

    // ---- authenticated endpoints ----
    // Scope requirement by route class: admin surfaces need Admin, any
    // state-changing /v1 operation needs Trade, reads need Read.
    let want_scope = if path.starts_with("/admin") || path == "/metrics" {
        Scope::Admin
    } else if matches!(method, Method::Post | Method::Delete) {
        Scope::Trade
    } else {
        Scope::Read
    };
    let id = match authenticate(ctx, &req, want_scope) {
        Ok(id) => id,
        Err(e) => {
            ctx.metrics.auth_fails.fetch_add(1, Ordering::Relaxed);
            let body = format!("{{\"error\":\"{}\",\"msg\":\"{}\"}}", e.name(), e.name());
            let extra: Vec<&'static str> = if e.status() == 429 {
                vec!["Retry-After: 1"]
            } else {
                vec![]
            };
            if e.status() == 429 {
                ctx.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
            }
            return (e.status(), body, extra);
        }
    };

    match (method, path.as_str()) {
        (Method::Get, "/v1/symbols") => (200, symbols_json(ctx), vec![]),
        (Method::Get, "/v1/book") => book_json(ctx, &req),
        (Method::Get, "/v1/tape") => tape_json(ctx, &req),
        (Method::Get, "/v1/account") => (200, account_json(ctx, id.account), vec![]),
        (Method::Post, "/v1/orders") => {
            let parsed = parse_order_body(&req.body, ctx, id.account, 1);
            match parsed {
                Ok(mut list) => {
                    let r = submit_orders(ctx, &mut list, id.account);
                    (r.0, r.1, vec![])
                }
                Err(body) => (400, body, vec![]),
            }
        }
        (Method::Post, "/v1/orders/batch") => {
            let parsed = parse_order_body(&req.body, ctx, id.account, 64);
            match parsed {
                Ok(mut list) => {
                    let r = submit_orders(ctx, &mut list, id.account);
                    (r.0, r.1, vec![])
                }
                Err(body) => (400, body, vec![]),
            }
        }
        (Method::Post, "/v1/orders/replace") => {
            replace_order(ctx, &req, id.account)
        }
        (Method::Delete, p) if p.starts_with("/v1/orders/") => {
            cancel_order(ctx, &req, p, id.account)
        }
        (Method::Get, "/v1/ws") => {
            // WS upgrade must come through the HTTP layer; a plain GET here
            // means missing upgrade headers.
            (
                400,
                "{\"error\":\"UpgradeRequired\",\"msg\":\"websocket upgrade required\"}".into(),
                vec![],
            )
        }
        (Method::Post, "/admin/keys") => admin_mint_key(ctx, &req),
        (Method::Post, "/admin/accounts") => admin_create_account(ctx, &req),
        (Method::Post, "/admin/deposit") => admin_funding(ctx, &req, true),
        (Method::Post, "/admin/withdraw") => admin_funding(ctx, &req, false),
        (Method::Post, "/admin/oracle") => admin_oracle(ctx, &req),
        (Method::Post, "/admin/sessions") => admin_sessions(ctx, &req),
        (Method::Post, "/admin/halt") => admin_halt_resume(ctx, &req, true),
        (Method::Post, "/admin/resume") => admin_halt_resume(ctx, &req, false),
        (Method::Post, "/admin/shutdown") => {
            ctx.draining.store(true, Ordering::Release);
            ctx.stopping.store(true, Ordering::Release);
            for q in &ctx.queues {
                q.close();
            }
            (200, "{\"ok\":true}".into(), vec![])
        }
        (Method::Get, "/admin/audit") => (200, audit_json(ctx), vec![]),
        (Method::Get, "/metrics") => (200, metrics_json(ctx), vec![]),
        (m, p) => {
            // Distinguish 404 from 405 on known paths.
            let known = matches!(
                p,
                "/v1/symbols"
                    | "/v1/book"
                    | "/v1/tape"
                    | "/v1/account"
                    | "/v1/orders"
                    | "/v1/orders/batch"
                    | "/v1/orders/replace"
                    | "/v1/ws"
                    | "/admin/keys"
                    | "/admin/accounts"
                    | "/admin/deposit"
                    | "/admin/withdraw"
                    | "/admin/oracle"
                    | "/admin/sessions"
                    | "/admin/halt"
                    | "/admin/resume"
                    | "/admin/audit"
                    | "/metrics"
            ) || (p.starts_with("/v1/orders/") && p.len() > "/v1/orders/".len());
            if known {
                (
                    405,
                    format!("{{\"error\":\"MethodNotAllowed\",\"msg\":\"{} not allowed\"}}", m.name()),
                    vec![],
                )
            } else {
                (
                    404,
                    "{\"error\":\"NotFound\",\"msg\":\"unknown route\"}".into(),
                    vec![],
                )
            }
        }
    }
}

/// Verify request authentication (headers or WS query fallback).
fn authenticate(ctx: &Arc<VenueCtx>, req: &Request, want: Scope) -> Result<Identity, AuthErr> {
    let key = req.header("x-api-key");
    let ts = req.header("x-timestamp");
    let nonce = req.header("x-nonce");
    let sig = req.header("x-signature");
    let (key, ts, nonce, sig) = match (key, ts, nonce, sig) {
        (Some(k), Some(t), Some(n), Some(s)) => (k.to_string(), t.to_string(), n.to_string(), s.to_string()),
        _ => {
            // WS query-string fallback (browsers cannot set upgrade headers).
            // Per the contract (§1), the signature covers the BARE path
            // (no auth query params) — the query carries the credentials.
            let q = query_pairs(req.query());
            let get = |name: &str| {
                q.iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            };
            match (get("key"), get("ts"), get("nonce"), get("sig")) {
                (Some(k), Some(t), Some(n), Some(s)) => (k, t, n, s),
                _ => return Err(AuthErr::UnknownKey),
            }
        }
    };
    let ts_ms: u64 = ts.parse().map_err(|_| AuthErr::StaleTimestamp)?;
    let nonce: u64 = nonce.parse().map_err(|_| AuthErr::BadNonce)?;
    let path_for_sig = if req.header("x-api-key").is_some() {
        req.target.clone()
    } else {
        req.path().to_string()
    };
    ctx.keyring.verify(
        &key,
        ts_ms,
        nonce,
        req.method.name(),
        &path_for_sig,
        &req.body,
        &sig,
        ctx.now_ms(),
        want,
    )
}

// ----------------------------------------------------------------------
// REST response builders
// ----------------------------------------------------------------------

fn err_body(code: &str, msg: &str) -> String {
    let mut w = crate::json::Writer::new();
    w.begin_obj();
    w.key("error");
    w.str(code);
    w.comma();
    w.key("msg");
    w.str(msg);
    w.end_obj();
    w.finish()
}

fn ok_body() -> String {
    "{\"ok\":true}".into()
}

fn symbols_json(ctx: &Arc<VenueCtx>) -> String {
    let mut w = crate::json::Writer::new();
    w.begin_obj();
    w.key("underlyings");
    w.begin_arr();
    for (i, u) in ctx.replica.underlyings().iter().enumerate() {
        if i > 0 {
            w.comma();
        }
        w.begin_obj();
        w.key("underlying");
        w.uint(u.underlying as u64);
        w.comma();
        w.key("mark");
        w.uint(u.mark);
        w.comma();
        w.key("ref");
        w.uint(u.reference);
        w.comma();
        w.key("fast");
        w.bool(u.fast);
        w.comma();
        w.key("halted");
        w.bool(u.halted);
        w.end_obj();
    }
    w.end_arr();
    w.comma();
    w.key("symbols");
    w.begin_arr();
    for (i, s) in ctx.replica.symbols().iter().enumerate() {
        if i > 0 {
            w.comma();
        }
        w.begin_obj();
        w.key("symbol");
        w.uint(s.symbol as u64);
        w.comma();
        w.key("underlying");
        w.uint(s.listing.underlying as u64);
        w.comma();
        w.key("kind");
        w.str(if s.listing.kind == crate::symbol::OptKind::Call { "call" } else { "put" });
        w.comma();
        w.key("strike");
        w.uint(s.listing.strike);
        w.comma();
        w.key("expiry_min");
        w.uint(s.listing.expiry_min as u64);
        w.comma();
        w.key("expiry_ms");
        w.uint(s.listing.expiry_min as u64 * 60_000);
        w.comma();
        w.key("phase");
        w.str(crate::replica::phase_name(s.phase));
        w.comma();
        w.key("mark");
        w.uint(s.mark);
        w.comma();
        w.key("last");
        w.uint(s.mark);
        w.comma();
        w.key("vol");
        w.uint(s.vol);
        w.comma();
        w.key("bid");
        w.uint(s.bbo.0);
        w.comma();
        w.key("bid_qty");
        w.uint(s.bbo.1);
        w.comma();
        w.key("ask");
        w.uint(s.bbo.2);
        w.comma();
        w.key("ask_qty");
        w.uint(s.bbo.3);
        w.comma();
        w.key("open");
        w.uint(s.open);
        w.comma();
        w.key("high");
        w.uint(s.high);
        w.comma();
        w.key("low");
        w.uint(s.low);
        w.comma();
        w.key("n_trades");
        w.uint(s.n_trades);
        w.end_obj();
    }
    w.end_arr();
    w.end_obj();
    w.finish()
}

fn query_u64(req: &Request, name: &str) -> Option<u64> {
    query_pairs(req.query())
        .iter()
        .find(|(k, _)| k == name)
        .and_then(|(_, v)| v.parse().ok())
}

fn book_json(ctx: &Arc<VenueCtx>, req: &Request) -> (u16, String, Vec<&'static str>) {
    let Some(symbol) = query_u64(req, "symbol") else {
        return (400, err_body("Malformed", "symbol required"), vec![]);
    };
    let depth = query_u64(req, "depth").unwrap_or(10).clamp(1, 25) as usize;
    let Some(s) = ctx.replica.symbol(symbol as SymbolId) else {
        return (404, err_body("NotFound", "unknown symbol"), vec![]);
    };
    let mut w = crate::json::Writer::new();
    w.begin_obj();
    w.key("symbol");
    w.uint(symbol);
    w.comma();
    w.key("bids");
    w.begin_arr();
    for (i, (p, q)) in s.depth_bids.iter().take(depth).enumerate() {
        if i > 0 {
            w.comma();
        }
        w.begin_arr();
        w.uint(*p);
        w.comma();
        w.uint(*q);
        w.end_arr();
    }
    w.end_arr();
    w.comma();
    w.key("asks");
    w.begin_arr();
    for (i, (p, q)) in s.depth_asks.iter().take(depth).enumerate() {
        if i > 0 {
            w.comma();
        }
        w.begin_arr();
        w.uint(*p);
        w.comma();
        w.uint(*q);
        w.end_arr();
    }
    w.end_arr();
    w.end_obj();
    (200, w.finish(), vec![])
}

fn tape_json(ctx: &Arc<VenueCtx>, req: &Request) -> (u16, String, Vec<&'static str>) {
    let Some(symbol) = query_u64(req, "symbol") else {
        return (400, err_body("Malformed", "symbol required"), vec![]);
    };
    let n = query_u64(req, "n").unwrap_or(50).clamp(1, 200) as usize;
    let Some(s) = ctx.replica.symbol(symbol as SymbolId) else {
        return (404, err_body("NotFound", "unknown symbol"), vec![]);
    };
    let mut w = crate::json::Writer::new();
    w.begin_obj();
    w.key("symbol");
    w.uint(symbol);
    w.comma();
    w.key("trades");
    w.begin_arr();
    let rows: Vec<&crate::replica::TapeRow> = s.tape.iter().rev().take(n).collect();
    for (i, t) in rows.iter().enumerate() {
        if i > 0 {
            w.comma();
        }
        w.begin_obj();
        w.key("price");
        w.uint(t.price);
        w.comma();
        w.key("qty");
        w.uint(t.qty as u64);
        w.comma();
        w.key("aggressor");
        w.str(match t.aggressor {
            0 => "buy",
            1 => "sell",
            _ => "auction",
        });
        w.comma();
        w.key("ms");
        w.uint(t.ts / NANOS_PER_MS);
        w.end_obj();
    }
    w.end_arr();
    w.end_obj();
    (200, w.finish(), vec![])
}

fn account_json(ctx: &Arc<VenueCtx>, account: AcctId) -> String {
    let row = ctx.replica.account(account);
    let mut balances: Vec<(&u16, &crate::replica::BalanceRow)> = row.balances.iter().collect();
    balances.sort_by_key(|(u, _)| **u);
    let mut w = crate::json::Writer::new();
    w.begin_obj();
    w.key("account");
    w.uint(account as u64);
    w.comma();
    w.key("tier");
    w.str("base");
    w.comma();
    w.key("balances");
    w.begin_arr();
    for (i, (u, b)) in balances.iter().enumerate() {
        if i > 0 {
            w.comma();
        }
        w.begin_obj();
        w.key("underlying");
        w.uint(**u as u64);
        w.comma();
        w.key("cash");
        w.cash(b.cash);
        w.comma();
        w.key("hold");
        w.cash(b.hold);
        w.comma();
        w.key("margin_required");
        w.uint(b.margin_required);
        w.comma();
        w.key("margin_reserved");
        w.uint(b.margin_reserved);
        w.comma();
        w.key("available");
        w.cash(b.cash - b.hold - b.margin_required as Cash - b.margin_reserved as Cash);
        w.comma();
        w.key("fees_paid");
        w.uint(b.fees_paid);
        w.comma();
        w.key("rebates_recv");
        w.uint(b.rebates_recv);
        w.end_obj();
    }
    w.end_arr();
    w.comma();
    w.key("positions");
    w.begin_arr();
    for (i, p) in row.positions.iter().enumerate() {
        if i > 0 {
            w.comma();
        }
        w.begin_obj();
        w.key("symbol");
        w.uint(p.symbol as u64);
        w.comma();
        w.key("qty");
        w.int(p.qty as i64);
        w.comma();
        w.key("margin");
        w.uint(p.margin);
        w.comma();
        w.key("delta_notional");
        w.int(p.delta_notional);
        w.end_obj();
    }
    w.end_arr();
    w.comma();
    w.key("orders");
    w.begin_arr();
    for (i, o) in ctx.replica.open_orders(account).iter().enumerate() {
        if i > 0 {
            w.comma();
        }
        w.begin_obj();
        w.key("order");
        w.uint(o.order);
        w.comma();
        w.key("symbol");
        w.uint(o.symbol as u64);
        w.comma();
        w.key("side");
        w.str(if o.buy { "buy" } else { "sell" });
        w.comma();
        w.key("price");
        w.uint(o.price);
        w.comma();
        w.key("qty");
        w.uint(o.qty as u64);
        w.comma();
        w.key("leaves");
        w.uint(o.leaves as u64);
        w.comma();
        w.key("tif");
        w.str(match o.tif {
            0 => "gtc",
            1 => "ioc",
            _ => "fok",
        });
        w.comma();
        w.key("kind");
        w.str(if o.kind == 0 { "limit" } else { "market" });
        w.comma();
        w.key("post_only");
        w.bool(o.post_only);
        w.comma();
        w.key("ms");
        w.uint(o.ts);
        w.end_obj();
    }
    w.end_arr();
    w.end_obj();
    w.finish()
}

// ----------------------------------------------------------------------
// Order entry
// ----------------------------------------------------------------------

/// Strictly parse and validate order request bodies. `max` = 1 for a single
/// order object, up to 64 for `{"orders":[...]}`. Returns validated orders
/// (account forced to the authenticated account) or an error body.
fn parse_order_body(
    body: &[u8],
    ctx: &Arc<VenueCtx>,
    account: AcctId,
    max: usize,
) -> Result<Vec<NewOrder>, String> {
    if body.len() > crate::http::MAX_BODY {
        return Err(err_body("TooLarge", "body exceeds limit"));
    }
    let doc = json_parse(body).map_err(|e| err_body("Malformed", &e.to_string()))?;
    let order_jsons: Vec<&Json> = if max == 1 {
        match &doc {
            Json::Obj(_) => vec![&doc],
            _ => return Err(err_body("Malformed", "object expected")),
        }
    } else {
        let arr = doc
            .get("orders")
            .and_then(|o| o.as_arr())
            .ok_or_else(|| err_body("Malformed", "orders array expected"))?;
        if arr.is_empty() || arr.len() > max {
            return Err(err_body("Malformed", "orders must be 1..=64 items"));
        }
        arr.iter().collect()
    };
    let max_qty = ctx.cfg.shard.max_order_qty;
    let mut out = Vec::with_capacity(order_jsons.len());
    for oj in order_jsons {
        // Unknown-field rejection (strict schema). "op" is the WS envelope
        // verb ({"op":"order",...}) and is ignored here.
        const FIELDS: [&str; 9] = [
            "op", "symbol", "side", "qty", "price", "tif", "kind", "post_only", "reduce_only",
        ];
        if let Json::Obj(fields) = oj {
            for (k, _) in fields {
                if !FIELDS.contains(&k.as_str()) {
                    return Err(err_body("Malformed", &format!("unknown field {k}")));
                }
            }
        } else {
            return Err(err_body("Malformed", "order object expected"));
        }
        let symbol = oj
            .get("symbol")
            .and_then(|v| v.as_u64())
            .filter(|&s| s <= u32::MAX as u64)
            .ok_or_else(|| err_body("Malformed", "symbol must be an integer"))? as SymbolId;
        if ctx.replica.symbol(symbol).is_none() {
            return Err(err_body("NotFound", "unknown symbol"));
        }
        let side = match oj.get("side").and_then(|v| v.as_str()) {
            Some("buy") => Side::Buy,
            Some("sell") => Side::Sell,
            _ => return Err(err_body("Malformed", "side must be buy|sell")),
        };
        let qty = oj
            .get("qty")
            .and_then(|v| v.as_u64())
            .filter(|&q| q >= 1 && q <= max_qty as u64)
            .ok_or_else(|| {
                err_body("Malformed", &format!("qty must be 1..={max_qty}"))
            })? as Qty;
        let price = oj
            .get("price")
            .and_then(|v| v.as_u64())
            .filter(|&p| (1..=crate::symbol::MAX_STRIKE_TICKS).contains(&p))
            .ok_or_else(|| err_body("Malformed", "price out of range"))?;
        let tif = match oj.get("tif").and_then(|v| v.as_str()) {
            Some("gtc") => Tif::Gtc,
            Some("ioc") => Tif::Ioc,
            Some("fok") => Tif::Fok,
            _ => return Err(err_body("Malformed", "tif must be gtc|ioc|fok")),
        };
        let kind = match oj.get("kind").and_then(|v| v.as_str()) {
            Some("limit") => Kind::Limit,
            Some("market") => Kind::Market,
            _ => return Err(err_body("Malformed", "kind must be limit|market")),
        };
        if kind == Kind::Market && tif != Tif::Ioc {
            return Err(err_body("Malformed", "market orders require tif ioc"));
        }
        let mut flags = 0u8;
        if oj.get("post_only").and_then(|v| v.as_bool()).unwrap_or(false) {
            flags |= crate::types::flags::POST_ONLY;
        }
        if oj.get("reduce_only").and_then(|v| v.as_bool()).unwrap_or(false) {
            flags |= crate::types::flags::REDUCE_ONLY;
        }
        out.push(NewOrder {
            symbol,
            account, // FORCED — clients cannot choose an account
            side,
            price,
            qty,
            tif,
            kind,
            flags,
            oco: NO_ID,
        });
    }
    Ok(out)
}

/// Submit validated orders: register awaits, enqueue, collect results in
/// request order. Returns (status, body).
fn submit_orders(ctx: &Arc<VenueCtx>, orders: &mut [NewOrder], _account: AcctId) -> (u16, String) {
    let timeout = Duration::from_millis(ctx.cfg.rest_timeout_ms);
    let mut waiters = Vec::with_capacity(orders.len());
    for o in orders.iter_mut() {
        let (tx, rx) = sync_channel(1);
        let await_id = ctx.register_await(Await::Rest(Mutex::new(Some(tx))));
        // Global -> local symbol id for the engine.
        let local = crate::platform::symbol_local(o.symbol);
        let ok = ctx.enqueue(
            Cmd::Place(NewOrder {
                symbol: local,
                account: o.account,
                side: o.side,
                price: o.price,
                qty: o.qty,
                tif: o.tif,
                kind: o.kind,
                flags: o.flags,
                oco: o.oco,
            }),
            o.symbol,
            await_id,
        );
        if !ok {
            ctx.drop_await(await_id);
            return (500, err_body("Internal", "shard unavailable"));
        }
        waiters.push(rx);
    }
    let single = orders.len() == 1;
    let mut results = String::new();
    if !single {
        results.push_str("{\"results\":[");
    }
    for (i, rx) in waiters.into_iter().enumerate() {
        let body = match rx.recv_timeout(timeout) {
            Ok(b) => b,
            Err(_) => {
                return (503, err_body("Timeout", "engine did not acknowledge"));
            }
        };
        if i > 0 {
            results.push(',');
        }
        results.push_str(&body);
    }
    if !single {
        results.push_str("]}");
    }
    (200, results)
}

fn cancel_order(
    ctx: &Arc<VenueCtx>,
    req: &Request,
    path: &str,
    account: AcctId,
) -> (u16, String, Vec<&'static str>) {
    let id_str = &path["/v1/orders/".len()..];
    let order: u64 = match id_str.parse() {
        Ok(v) => v,
        Err(_) => return (400, err_body("Malformed", "bad order id"), vec![]),
    };
    let Some(symbol) = query_u64(req, "symbol") else {
        return (400, err_body("Malformed", "symbol required"), vec![]);
    };
    // Edge-side ownership pre-check (engine re-verifies — defense in depth).
    if let Some(row) = ctx.replica.order(order) {
        if row.account != account {
            return (403, err_body("NotOwner", "order belongs to another account"), vec![]);
        }
    }
    let (tx, rx) = sync_channel(1);
    let await_id = ctx.register_await(Await::Rest(Mutex::new(Some(tx))));
    let ok = ctx.enqueue(
        Cmd::Cancel {
            order,
            symbol: crate::platform::symbol_local(symbol as SymbolId),
            account,
        },
        symbol as SymbolId,
        await_id,
    );
    if !ok {
        ctx.drop_await(await_id);
        return (500, err_body("Internal", "shard unavailable"), vec![]);
    }
    match rx.recv_timeout(Duration::from_millis(ctx.cfg.rest_timeout_ms)) {
        Ok(body) => {
            // UnknownOrder engine reject -> 409 for REST semantics.
            if body.contains("\"reason\":\"UnknownOrder\"") {
                (409, err_body("UnknownOrder", "no such order"), vec![])
            } else if body.contains("\"reason\":\"NotOwner\"") {
                (403, err_body("NotOwner", "order belongs to another account"), vec![])
            } else {
                let mut full = "{\"ok\":true,\"order_id\":".to_string();
                full.push_str(&order.to_string());
                full.push(',');
                full.push_str(&body[1..]);
                (200, full, vec![])
            }
        }
        Err(_) => (503, err_body("Timeout", "engine did not acknowledge"), vec![]),
    }
}

fn replace_order(
    ctx: &Arc<VenueCtx>,
    req: &Request,
    account: AcctId,
) -> (u16, String, Vec<&'static str>) {
    let doc = match json_parse(&req.body) {
        Ok(d) => d,
        Err(e) => return (400, err_body("Malformed", &e.to_string()), vec![]),
    };
    const FIELDS: [&str; 4] = ["order", "symbol", "price", "qty"];
    if let Json::Obj(fields) = &doc {
        for (k, _) in fields {
            if !FIELDS.contains(&k.as_str()) {
                return (400, err_body("Malformed", &format!("unknown field {k}")), vec![]);
            }
        }
    } else {
        return (400, err_body("Malformed", "object expected"), vec![]);
    }
    let order = doc
        .get("order")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| err_body("Malformed", "order required").to_string());
    let order = match order {
        Ok(o) => o,
        Err(e) => return (400, e, vec![]),
    };
    let symbol = match doc
        .get("symbol")
        .and_then(|v| v.as_u64())
        .filter(|&s| s <= u32::MAX as u64)
    {
        Some(s) => s as SymbolId,
        None => return (400, err_body("Malformed", "symbol required"), vec![]),
    };
    let price = match doc.get("price").and_then(|v| v.as_u64()).filter(|&p| p >= 1) {
        Some(p) => p,
        None => return (400, err_body("Malformed", "price required"), vec![]),
    };
    let qty = match doc
        .get("qty")
        .and_then(|v| v.as_u64())
        .filter(|&q| q >= 1 && q <= ctx.cfg.shard.max_order_qty as u64)
    {
        Some(q) => q as Qty,
        None => return (400, err_body("Malformed", "qty out of range"), vec![]),
    };
    if let Some(row) = ctx.replica.order(order) {
        if row.account != account {
            return (403, err_body("NotOwner", "order belongs to another account"), vec![]);
        }
    }
    let (tx, rx) = sync_channel(1);
    let await_id = ctx.register_await(Await::Rest(Mutex::new(Some(tx))));
    let ok = ctx.enqueue(
        Cmd::Replace {
            order,
            symbol: crate::platform::symbol_local(symbol),
            price,
            qty,
            account,
        },
        symbol,
        await_id,
    );
    if !ok {
        ctx.drop_await(await_id);
        return (500, err_body("Internal", "shard unavailable"), vec![]);
    }
    match rx.recv_timeout(Duration::from_millis(ctx.cfg.rest_timeout_ms)) {
        Ok(body) => {
            if body.contains("\"reason\":\"UnknownOrder\"") {
                (409, err_body("UnknownOrder", "no such order"), vec![])
            } else if body.contains("\"reason\":\"NotOwner\"") {
                (403, err_body("NotOwner", "order belongs to another account"), vec![])
            } else {
                (200, body, vec![])
            }
        }
        Err(_) => (503, err_body("Timeout", "engine did not acknowledge"), vec![]),
    }
}

// ----------------------------------------------------------------------
// Admin endpoints
// ----------------------------------------------------------------------

fn parse_obj(body: &[u8]) -> Result<Json, String> {
    json_parse(body).map_err(|e| err_body("Malformed", &e.to_string()))
}

fn admin_mint_key(ctx: &Arc<VenueCtx>, req: &Request) -> (u16, String, Vec<&'static str>) {
    let doc = match parse_obj(&req.body) {
        Ok(d) => d,
        Err(e) => return (400, e, vec![]),
    };
    let account = match doc.get("account").and_then(|v| v.as_u64()) {
        Some(a) if a <= u32::MAX as u64 => a as AcctId,
        _ => return (400, err_body("Malformed", "account required"), vec![]),
    };
    let scope = match doc.get("scope").and_then(|v| v.as_str()) {
        Some(s) => match Scope::parse(s) {
            Some(sc) => sc,
            None => return (400, err_body("Malformed", "scope must be read|trade|admin"), vec![]),
        },
        None => return (400, err_body("Malformed", "scope required"), vec![]),
    };
    if !ctx.known_accounts.lock().unwrap().contains(&account) {
        return (404, err_body("NotFound", "unknown account"), vec![]);
    }
    match ctx.keyring.mint(account, scope) {
        Ok((key_id, secret)) => {
            let mut w = crate::json::Writer::new();
            w.begin_obj();
            w.key("key_id");
            w.str(&crate::auth::key_id_hex(key_id));
            w.comma();
            w.key("secret");
            w.str(&secret);
            w.comma();
            w.key("account");
            w.uint(account as u64);
            w.comma();
            w.key("scope");
            w.str(scope.name());
            w.end_obj();
            (200, w.finish(), vec![])
        }
        Err(e) => (500, err_body("Internal", e.name()), vec![]),
    }
}

fn admin_create_account(ctx: &Arc<VenueCtx>, req: &Request) -> (u16, String, Vec<&'static str>) {
    let doc = match parse_obj(&req.body) {
        Ok(d) => d,
        Err(e) => return (400, e, vec![]),
    };
    let account = match doc.get("account").and_then(|v| v.as_u64()) {
        Some(a) if (1..u32::MAX as u64).contains(&a) => a as AcctId,
        _ => return (400, err_body("Malformed", "account must be 1..=u32::MAX-1"), vec![]),
    };
    let tier = match doc.get("tier").and_then(|v| v.as_str()) {
        Some("base") => crate::fees::Tier::Base,
        Some("silver") => crate::fees::Tier::Silver,
        Some("gold") => crate::fees::Tier::Gold,
        Some("prime") => crate::fees::Tier::Prime,
        _ => return (400, err_body("Malformed", "tier must be base|silver|gold|prime"), vec![]),
    };
    if ctx.known_accounts.lock().unwrap().contains(&account) {
        return (409, err_body("Exists", "account already exists"), vec![]);
    }
    // Create on every shard (accounts are global; cash is per underlying).
    let (tx, rx) = sync_channel(1);
    let await_id = ctx.register_await(Await::Rest(Mutex::new(Some(tx))));
    let mut ok_all = true;
    for u in ctx.underlyings.clone() {
        if !ctx.enqueue_underlying(
            Cmd::CreateAccount { account, tier },
            u,
            await_id,
        ) {
            ok_all = false;
        }
    }
    if !ok_all {
        ctx.drop_await(await_id);
        return (500, err_body("Internal", "shard unavailable"), vec![]);
    }
    let _ = rx.recv_timeout(Duration::from_millis(ctx.cfg.rest_timeout_ms));
    ctx.known_accounts.lock().unwrap().insert(account);
    (200, ok_body(), vec![])
}

fn admin_funding(
    ctx: &Arc<VenueCtx>,
    req: &Request,
    deposit: bool,
) -> (u16, String, Vec<&'static str>) {
    let doc = match parse_obj(&req.body) {
        Ok(d) => d,
        Err(e) => return (400, e, vec![]),
    };
    let account = match doc.get("account").and_then(|v| v.as_u64()) {
        Some(a) if a <= u32::MAX as u64 => a as AcctId,
        _ => return (400, err_body("Malformed", "account required"), vec![]),
    };
    let underlying = match doc.get("underlying").and_then(|v| v.as_u64()) {
        Some(u) if u <= u16::MAX as u64 => u as UnderlyingId,
        _ => return (400, err_body("Malformed", "underlying required"), vec![]),
    };
    let amount = match doc.get("amount").and_then(|v| v.as_u64()) {
        Some(a) if (1..=(1u64 << 62)).contains(&a) => a as Cash,
        _ => return (400, err_body("Malformed", "amount must be a positive integer"), vec![]),
    };
    if !ctx.known_accounts.lock().unwrap().contains(&account) {
        return (404, err_body("NotFound", "unknown account"), vec![]);
    }
    if !ctx.underlyings.contains(&underlying) {
        return (404, err_body("NotFound", "unknown underlying"), vec![]);
    }
    let cmd = if deposit {
        Cmd::Deposit { account, amount }
    } else {
        Cmd::Withdraw { account, amount }
    };
    let (tx, rx) = sync_channel(1);
    let await_id = ctx.register_await(Await::Rest(Mutex::new(Some(tx))));
    if !ctx.enqueue_underlying(cmd, underlying, await_id) {
        ctx.drop_await(await_id);
        return (500, err_body("Internal", "shard unavailable"), vec![]);
    }
    match rx.recv_timeout(Duration::from_millis(ctx.cfg.rest_timeout_ms)) {
        Ok(body) => {
            if body.contains("\"ok\":true") {
                (200, ok_body(), vec![])
            } else {
                // Engine declined (e.g. insufficient available for withdraw).
                (409, err_body("Declined", "engine declined the operation"), vec![])
            }
        }
        Err(_) => (503, err_body("Timeout", "engine did not acknowledge"), vec![]),
    }
}

fn admin_oracle(ctx: &Arc<VenueCtx>, req: &Request) -> (u16, String, Vec<&'static str>) {
    let doc = match parse_obj(&req.body) {
        Ok(d) => d,
        Err(e) => return (400, e, vec![]),
    };
    let underlying = match doc.get("underlying").and_then(|v| v.as_u64()) {
        Some(u) if u <= u16::MAX as u64 => u as UnderlyingId,
        _ => return (400, err_body("Malformed", "underlying required"), vec![]),
    };
    let price = match doc
        .get("price")
        .and_then(|v| v.as_u64())
        .filter(|&p| (1..=crate::symbol::MAX_STRIKE_TICKS * 1_000).contains(&p))
    {
        Some(p) => p,
        None => return (400, err_body("Malformed", "price out of range"), vec![]),
    };
    if !ctx.underlyings.contains(&underlying) {
        return (404, err_body("NotFound", "unknown underlying"), vec![]);
    }
    let (tx, rx) = sync_channel(1);
    let await_id = ctx.register_await(Await::Rest(Mutex::new(Some(tx))));
    if !ctx.enqueue_underlying(Cmd::Oracle { price }, underlying, await_id) {
        ctx.drop_await(await_id);
        return (500, err_body("Internal", "shard unavailable"), vec![]);
    }
    let _ = rx.recv_timeout(Duration::from_millis(ctx.cfg.rest_timeout_ms));
    (200, ok_body(), vec![])
}

fn admin_sessions(ctx: &Arc<VenueCtx>, req: &Request) -> (u16, String, Vec<&'static str>) {
    let doc = match parse_obj(&req.body) {
        Ok(d) => d,
        Err(e) => return (400, e, vec![]),
    };
    let underlying = match doc.get("underlying").and_then(|v| v.as_u64()) {
        Some(u) if u <= u16::MAX as u64 => u as UnderlyingId,
        _ => return (400, err_body("Malformed", "underlying required"), vec![]),
    };
    if !ctx.underlyings.contains(&underlying) {
        return (404, err_body("NotFound", "unknown underlying"), vec![]);
    }
    let expiry_ms = match doc.get("expiry_ms").and_then(|v| v.as_u64()) {
        Some(e) if e > ctx.now_ms() + 60_000 => e,
        _ => {
            return (
                400,
                err_body("Malformed", "expiry_ms must be > 60s in the future"),
                vec![],
            )
        }
    };
    let strikes = match doc.get("strikes").and_then(|v| v.as_arr()) {
        Some(a) if !a.is_empty() && a.len() <= 256 => a.to_vec(),
        _ => return (400, err_body("Malformed", "strikes must be 1..=256 ticks"), vec![]),
    };
    let kinds = match doc.get("kinds").and_then(|v| v.as_arr()) {
        Some(a) if !a.is_empty() && a.len() <= 2 => a.to_vec(),
        _ => return (400, err_body("Malformed", "kinds must be [\"call\"] and/or [\"put\"]"), vec![]),
    };
    let expiry_min = (expiry_ms / 60_000) as u32;
    if expiry_min > crate::symbol::MAX_EXPIRY_MIN {
        return (400, err_body("Malformed", "expiry too far out"), vec![]);
    }
    let mut listings = Vec::new();
    for s in &strikes {
        let strike = match s.as_u64() {
            Some(v) if (1..=crate::symbol::MAX_STRIKE_TICKS).contains(&v) => v,
            _ => return (400, err_body("Malformed", "bad strike"), vec![]),
        };
        for k in &kinds {
            let kind = match k.as_str() {
                Some("call") => crate::symbol::OptKind::Call,
                Some("put") => crate::symbol::OptKind::Put,
                _ => return (400, err_body("Malformed", "kind must be call|put"), vec![]),
            };
            listings.push(crate::symbol::Listing {
                underlying,
                expiry_min,
                kind,
                strike,
            });
        }
    }
    // Enqueue each listing; collect symbol ids via awaits.
    let mut waiters = Vec::new();
    for l in &listings {
        let (tx, rx) = sync_channel(1);
        let await_id = ctx.register_await(Await::Rest(Mutex::new(Some(tx))));
        if !ctx.enqueue_underlying(Cmd::CreateSymbol { listing: *l }, underlying, await_id) {
            ctx.drop_await(await_id);
            return (500, err_body("Internal", "shard unavailable"), vec![]);
        }
        waiters.push(rx);
    }
    let mut symbols = String::from("[");
    let timeout = Duration::from_millis(ctx.cfg.rest_timeout_ms);
    for (i, rx) in waiters.into_iter().enumerate() {
        match rx.recv_timeout(timeout) {
            Ok(body) => {
                // body: {"ok":true,"symbols":[gsym]}
                if let Some(pos) = body.find('[') {
                    let arr = &body[pos + 1..body.len() - 2];
                    if i > 0 {
                        symbols.push(',');
                    }
                    symbols.push_str(arr);
                }
            }
            Err(_) => return (503, err_body("Timeout", "engine did not acknowledge"), vec![]),
        }
    }
    symbols.push(']');
    let mut out = String::from("{\"ok\":true,\"symbols\":");
    out.push_str(&symbols);
    out.push('}');
    (200, out, vec![])
}

fn admin_halt_resume(
    ctx: &Arc<VenueCtx>,
    req: &Request,
    halt: bool,
) -> (u16, String, Vec<&'static str>) {
    let doc = match parse_obj(&req.body) {
        Ok(d) => d,
        Err(e) => return (400, e, vec![]),
    };
    let underlying = match doc.get("underlying").and_then(|v| v.as_u64()) {
        Some(u) if u <= u16::MAX as u64 => u as UnderlyingId,
        _ => return (400, err_body("Malformed", "underlying required"), vec![]),
    };
    if !ctx.underlyings.contains(&underlying) {
        return (404, err_body("NotFound", "unknown underlying"), vec![]);
    }
    let cmd = if halt { Cmd::Halt } else { Cmd::ResumeNow };
    let (tx, rx) = sync_channel(1);
    let await_id = ctx.register_await(Await::Rest(Mutex::new(Some(tx))));
    if !ctx.enqueue_underlying(cmd, underlying, await_id) {
        ctx.drop_await(await_id);
        return (500, err_body("Internal", "shard unavailable"), vec![]);
    }
    match rx.recv_timeout(Duration::from_millis(ctx.cfg.rest_timeout_ms)) {
        Ok(body) if body.contains("\"ok\":true") => (200, ok_body(), vec![]),
        Ok(_) => (200, ok_body(), vec![]),
        Err(_) => (503, err_body("Timeout", "engine did not acknowledge"), vec![]),
    }
}

fn audit_json(ctx: &Arc<VenueCtx>) -> String {
    let mut w = crate::json::Writer::new();
    w.begin_obj();
    w.key("conservation");
    w.bool(ctx.replica.conservation());
    w.comma();
    w.key("shards");
    w.begin_arr();
    for (i, m) in ctx.replica.shard_metrics().iter().enumerate() {
        if i > 0 {
            w.comma();
        }
        w.begin_obj();
        w.key("shard");
        w.uint(m.shard as u64);
        w.comma();
        w.key("underlying");
        w.uint(m.underlying as u64);
        w.comma();
        w.key("orders");
        w.uint(m.orders);
        w.comma();
        w.key("rejects");
        w.uint(m.rejects);
        w.comma();
        w.key("trades");
        w.uint(m.trades);
        w.comma();
        w.key("volume");
        w.uint(m.volume);
        w.comma();
        w.key("fees");
        w.uint(m.fees);
        w.comma();
        w.key("cancels");
        w.uint(m.cancels);
        w.comma();
        w.key("cmds");
        w.uint(m.cmds);
        w.comma();
        w.key("expiries");
        w.uint(m.expiries);
        w.comma();
        w.key("auctions");
        w.uint(m.auctions);
        w.comma();
        w.key("queue_depth");
        w.uint(m.queue_depth as u64);
        w.comma();
        w.key("journal_bytes");
        w.uint(m.journal_bytes);
        w.comma();
        w.key("tps_ema");
        w.raw(&format!("{:.1}", m.tps_ema));
        w.comma();
        w.key("state_hash");
        w.str(&format!("{:016x}", m.state_hash));
        w.end_obj();
    }
    w.end_arr();
    w.comma();
    w.key("gateway");
    gateway_metrics_obj(ctx, &mut w);
    w.end_obj();
    w.finish()
}

fn metrics_json(ctx: &Arc<VenueCtx>) -> String {
    audit_json(ctx)
}

fn gateway_metrics_obj(ctx: &Arc<VenueCtx>, w: &mut crate::json::Writer) {
    w.begin_obj();
    w.key("conns");
    w.uint(ctx.metrics.conns.load(Ordering::Relaxed) as u64);
    w.comma();
    w.key("ws");
    w.uint(ctx.metrics.ws_conns.load(Ordering::Relaxed) as u64);
    w.comma();
    w.key("rest_requests");
    w.uint(ctx.metrics.rest_requests.load(Ordering::Relaxed));
    w.comma();
    w.key("ws_messages");
    w.uint(ctx.metrics.ws_messages.load(Ordering::Relaxed));
    w.comma();
    w.key("rx");
    w.uint(
        ctx.metrics.rest_requests.load(Ordering::Relaxed)
            + ctx.metrics.ws_messages.load(Ordering::Relaxed),
    );
    w.comma();
    w.key("tx");
    w.uint(ctx.metrics.tx_frames.load(Ordering::Relaxed));
    w.comma();
    w.key("auth_fails");
    w.uint(
        ctx.metrics.auth_fails.load(Ordering::Relaxed)
            + ctx.keyring.total_auth_fails(),
    );
    w.comma();
    w.key("rate_limited");
    w.uint(ctx.metrics.rate_limited.load(Ordering::Relaxed));
    w.comma();
    w.key("slow_dropped");
    w.uint(ctx.metrics.slow_dropped.load(Ordering::Relaxed));
    w.comma();
    w.key("malformed");
    w.uint(ctx.metrics.malformed.load(Ordering::Relaxed));
    w.end_obj();
}

// ----------------------------------------------------------------------
// WebSocket session
// ----------------------------------------------------------------------

/// Run an authenticated WS session to completion.
fn ws_session(
    ctx: &Arc<VenueCtx>,
    reader: &mut TcpStream,
    write_lock: &Arc<Mutex<TcpStream>>,
    tx_outbox: &SyncSender<Vec<u8>>,
    req: Request,
    _ip: IpAddr,
) -> std::io::Result<()> {
    // Authenticate (headers, or query fallback for browsers).
    let id = match authenticate(ctx, &req, Scope::Read) {
        Ok(id) => id,
        Err(e) => {
            ctx.metrics.auth_fails.fetch_add(1, Ordering::Relaxed);
            let body = format!("{{\"error\":\"{}\"}}", e.name());
            write_all(
                write_lock,
                &response(e.status(), "application/json", body.as_bytes(), &[], true),
            )?;
            return Ok(());
        }
    };
    // Validate the handshake.
    let key = match req.header("sec-websocket-key") {
        Some(k) if k.len() >= 16 && k.len() <= 64 => k.to_string(),
        _ => {
            write_all(
                write_lock,
                &response(400, "text/plain", b"missing sec-websocket-key", &[], true),
            )?;
            return Ok(());
        }
    };
    let accept = crate::crypto::ws_accept_key(&key);
    let handshake = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nAccess-Control-Allow-Origin: *\r\n\r\n"
    );
    if write_all(write_lock, handshake.as_bytes()).is_err() {
        return Ok(());
    }

    // Register the connection with the router.
    let conn = ctx.alloc_conn();
    {
        let mut conns = ctx.conns.lock().unwrap();
        conns.insert(
            conn,
            crate::venue::ConnEntry {
                account: id.account,
                outbox: tx_outbox.clone(),
                ws: true,
                alive: std::sync::atomic::AtomicBool::new(true),
            },
        );
    }
    {
        let mut by_acct = ctx.by_account.lock().unwrap();
        by_acct.entry(id.account).or_default().push(conn);
    }
    ctx.metrics.ws_conns.fetch_add(1, Ordering::Relaxed);
    let _ws_guard = WsGuard {
        ctx: Some(ctx.clone()),
        conn,
    };

    // Hello frame.
    let mut hello = crate::json::Writer::new();
    hello.begin_obj();
    hello.key("t");
    hello.str("hello");
    hello.comma();
    hello.key("account");
    hello.uint(id.account as u64);
    hello.comma();
    hello.key("scope");
    hello.str(id.scope.name());
    hello.comma();
    hello.key("ms");
    hello.uint(ctx.now_ms());
    hello.end_obj();
    let _ = tx_outbox.try_send(crate::ws::encode_text(&hello.finish()));

    // Session state.
    let mut parser_buf: Vec<u8> = Vec::with_capacity(4 * 1024);
    let mut msg_buf = crate::ws::MessageBuf::default();
    let mut read_buf = [0u8; 4 * 1024];
    let mut msg_times: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
    let idle = Duration::from_millis(ctx.cfg.ws_idle_ms);
    let max_subs = ctx.cfg.max_subs;
    let mut n_subs = 0usize;

    reader.set_read_timeout(Some(idle))?;
    loop {
        if ctx.stopping.load(Ordering::Acquire) {
            let _ = tx_outbox.try_send(crate::ws::encode_close(1001));
            return Ok(());
        }
        let n = match reader.read(&mut read_buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(_) => return Ok(()), // idle timeout or reset
        };
        parser_buf.extend_from_slice(&read_buf[..n]);
        // Decode as many frames as available.
        loop {
            match crate::ws::decode(&parser_buf) {
                crate::ws::Decoded::NeedMore => break,
                crate::ws::Decoded::Violation(code, _why) => {
                    let _ = tx_outbox.try_send(crate::ws::encode_close(code));
                    return Ok(());
                }
                crate::ws::Decoded::Frame(frame, used) => {
                    parser_buf.drain(..used);
                    match msg_buf.feed(frame) {
                        crate::ws::MsgResult::NeedMore => {}
                        crate::ws::MsgResult::Message(_binary, data) => {
                            ctx.metrics.ws_messages.fetch_add(1, Ordering::Relaxed);
                            // Message-rate budget: max N per 10 s.
                            let now_ms = ctx.now_ms();
                            while msg_times.front().is_some_and(|&t| now_ms.saturating_sub(t) > 10_000) {
                                msg_times.pop_front();
                            }
                            if msg_times.len() >= ctx.cfg.ws_msg_per_10s as usize {
                                ctx.metrics.rate_limited.fetch_add(1, Ordering::Relaxed);
                                let _ = tx_outbox.try_send(crate::ws::encode_close(1008));
                                return Ok(());
                            }
                            msg_times.push_back(now_ms);
                            handle_ws_message(
                                ctx, conn, id, &data, tx_outbox, max_subs, &mut n_subs,
                            );
                        }
                        crate::ws::MsgResult::Control(f) => {
                            match f.opcode {
                                crate::ws::Opcode::Ping => {
                                    let _ = tx_outbox.try_send(crate::ws::encode(
                                        crate::ws::Opcode::Pong,
                                        &f.payload,
                                    ));
                                }
                                crate::ws::Opcode::Close => {
                                    let _ = tx_outbox.try_send(crate::ws::encode_close(1000));
                                    return Ok(());
                                }
                                _ => {}
                            }
                        }
                        crate::ws::MsgResult::Closed => {
                            let _ = tx_outbox.try_send(crate::ws::encode_close(1000));
                            return Ok(());
                        }
                        crate::ws::MsgResult::Violation(code, _) => {
                            let _ = tx_outbox.try_send(crate::ws::encode_close(code));
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

/// Deregister a WS session on exit.
struct WsGuard {
    ctx: Option<Arc<VenueCtx>>,
    conn: u64,
}

impl Drop for WsGuard {
    fn drop(&mut self) {
        if let Some(ctx) = self.ctx.take() {
            ctx.metrics.ws_conns.fetch_sub(1, Ordering::Relaxed);
            ctx.deregister_conn(self.conn);
        }
    }
}

/// Handle one decoded WS client message (JSON text).
fn handle_ws_message(
    ctx: &Arc<VenueCtx>,
    conn: u64,
    id: Identity,
    data: &[u8],
    tx_outbox: &SyncSender<Vec<u8>>,
    max_subs: usize,
    n_subs: &mut usize,
) {
    if data.len() > crate::ws::MAX_PAYLOAD {
        let _ = tx_outbox.try_send(crate::ws::encode_close(1009));
        return;
    }
    let doc = match json_parse(data) {
        Ok(d) => d,
        Err(e) => {
            ctx.metrics.malformed.fetch_add(1, Ordering::Relaxed);
            let mut w = crate::json::Writer::new();
            w.begin_obj();
            w.key("t");
            w.str("error");
            w.comma();
            w.key("msg");
            w.str(&e.to_string());
            w.end_obj();
            let _ = tx_outbox.try_send(crate::ws::encode_text(&w.finish()));
            return;
        }
    };
    let op = doc.get("op").and_then(|v| v.as_str()).unwrap_or("");
    match op {
        "ping" => {
            let _ = tx_outbox.try_send(crate::ws::encode_text("{\"t\":\"pong\"}"));
        }
        "sub" | "unsub" => {
            let chan_name = doc.get("channel").and_then(|v| v.as_str()).unwrap_or("");
            let chan_id = match chan_name {
                "bbo" => chan::BBO,
                "trade" => chan::TRADE,
                "depth" => chan::DEPTH,
                "phase" => chan::PHASE,
                "mark" => chan::MARK,
                "underlying" => chan::UNDERLYING,
                _ => {
                    send_ws_error(tx_outbox, "unknown channel");
                    return;
                }
            };
            let key = if chan_id == chan::UNDERLYING {
                match doc
                    .get("underlying")
                    .and_then(|v| v.as_u64())
                    .filter(|&u| ctx.underlyings.contains(&(u as UnderlyingId)))
                {
                    Some(u) => u as u32,
                    None => {
                        send_ws_error(tx_outbox, "unknown underlying");
                        return;
                    }
                }
            } else {
                match doc
                    .get("symbol")
                    .and_then(|v| v.as_u64())
                    .filter(|&s| ctx.replica.symbol(s as SymbolId).is_some())
                {
                    Some(s) => s as u32,
                    None => {
                        send_ws_error(tx_outbox, "unknown symbol");
                        return;
                    }
                }
            };
            let sub_key = SubKey { chan: chan_id, key };
            if op == "sub" {
                let mut subs = ctx.subs.lock().unwrap();
                let entry = subs.entry(sub_key).or_default();
                if entry.contains(&conn) {
                    return; // idempotent
                }
                if *n_subs >= max_subs {
                    send_ws_error(tx_outbox, "subscription limit reached");
                    return;
                }
                entry.push(conn);
                *n_subs += 1;
                drop(subs);
                send_snapshot(ctx, tx_outbox, sub_key);
            } else {
                let mut subs = ctx.subs.lock().unwrap();
                if let Some(v) = subs.get_mut(&sub_key) {
                    v.retain(|&c| c != conn);
                    if v.is_empty() {
                        subs.remove(&sub_key);
                    }
                }
                if *n_subs > 0 {
                    *n_subs -= 1;
                }
            }
        }
        "order" => {
            let parsed = parse_order_body(data, ctx, id.account, 1);
            match parsed {
                Ok(mut list) => {
                    if let Some(o) = list.first_mut() {
                        let await_id = ctx.register_await(Await::Ws(conn));
                        let local = crate::platform::symbol_local(o.symbol);
                        let ok = ctx.enqueue(
                            Cmd::Place(NewOrder {
                                symbol: local,
                                account: id.account,
                                side: o.side,
                                price: o.price,
                                qty: o.qty,
                                tif: o.tif,
                                kind: o.kind,
                                flags: o.flags,
                                oco: o.oco,
                            }),
                            o.symbol,
                            await_id,
                        );
                        if !ok {
                            ctx.drop_await(await_id);
                            send_ws_error(tx_outbox, "shard unavailable");
                        }
                    }
                }
                Err(body) => {
                    ctx.metrics.malformed.fetch_add(1, Ordering::Relaxed);
                    let _ = tx_outbox.try_send(crate::ws::encode_text(&body));
                }
            }
        }
        "cancel" => {
            let order = doc.get("order").and_then(|v| v.as_u64());
            let symbol = doc
                .get("symbol")
                .and_then(|v| v.as_u64())
                .filter(|&s| s <= u32::MAX as u64)
                .map(|s| s as SymbolId);
            match (order, symbol) {
                (Some(order), Some(symbol)) => {
                    if let Some(row) = ctx.replica.order(order) {
                        if row.account != id.account {
                            send_ws_error(tx_outbox, "NotOwner");
                            return;
                        }
                    }
                    let await_id = ctx.register_await(Await::Ws(conn));
                    let ok = ctx.enqueue(
                        Cmd::Cancel {
                            order,
                            symbol: crate::platform::symbol_local(symbol),
                            account: id.account,
                        },
                        symbol,
                        await_id,
                    );
                    if !ok {
                        ctx.drop_await(await_id);
                        send_ws_error(tx_outbox, "shard unavailable");
                    }
                }
                _ => send_ws_error(tx_outbox, "order and symbol required"),
            }
        }
        _ => {
            send_ws_error(tx_outbox, "unknown op");
        }
    }
}

fn send_ws_error(tx_outbox: &SyncSender<Vec<u8>>, msg: &str) {
    let mut w = crate::json::Writer::new();
    w.begin_obj();
    w.key("t");
    w.str("error");
    w.comma();
    w.key("msg");
    w.str(msg);
    w.end_obj();
    let _ = tx_outbox.try_send(crate::ws::encode_text(&w.finish()));
}

/// Send the current state snapshot for a fresh subscription.
fn send_snapshot(ctx: &Arc<VenueCtx>, tx_outbox: &SyncSender<Vec<u8>>, key: SubKey) {
    match key.chan {
        chan::UNDERLYING => {
            if let Some(u) = ctx.replica.underlying(key.key as UnderlyingId) {
                let mut w = crate::json::Writer::new();
                w.begin_obj();
                w.key("t");
                w.str("underlying");
                w.comma();
                w.key("underlying");
                w.uint(u.underlying as u64);
                w.comma();
                w.key("price");
                w.uint(u.mark);
                w.comma();
                w.key("ref");
                w.uint(u.reference);
                w.comma();
                w.key("fast");
                w.bool(u.fast);
                w.comma();
                w.key("halted");
                w.bool(u.halted);
                w.end_obj();
                let _ = tx_outbox.try_send(crate::ws::encode_text(&w.finish()));
            }
        }
        _ => {
            if let Some(s) = ctx.replica.symbol(key.key) {
                // BBO snapshot.
                let mut w = crate::json::Writer::new();
                w.begin_obj();
                w.key("t");
                w.str("bbo");
                w.comma();
                w.key("symbol");
                w.uint(s.symbol as u64);
                w.comma();
                w.key("bid");
                w.uint(s.bbo.0);
                w.comma();
                w.key("bid_qty");
                w.uint(s.bbo.1);
                w.comma();
                w.key("ask");
                w.uint(s.bbo.2);
                w.comma();
                w.key("ask_qty");
                w.uint(s.bbo.3);
                w.end_obj();
                let _ = tx_outbox.try_send(crate::ws::encode_text(&w.finish()));
                // Phase snapshot.
                let mut w2 = crate::json::Writer::new();
                w2.begin_obj();
                w2.key("t");
                w2.str("phase");
                w2.comma();
                w2.key("symbol");
                w2.uint(s.symbol as u64);
                w2.comma();
                w2.key("phase");
                w2.str(crate::replica::phase_name(s.phase));
                w2.end_obj();
                let _ = tx_outbox.try_send(crate::ws::encode_text(&w2.finish()));
                // Depth snapshot.
                let mut w3 = crate::json::Writer::new();
                w3.begin_obj();
                w3.key("t");
                w3.str("depth");
                w3.comma();
                w3.key("symbol");
                w3.uint(s.symbol as u64);
                w3.comma();
                w3.key("bids");
                w3.begin_arr();
                let mut first = true;
                for (p, q) in s.depth_bids.iter().take(10) {
                    if !first {
                        w3.comma();
                    }
                    first = false;
                    w3.begin_arr();
                    w3.uint(*p);
                    w3.comma();
                    w3.uint(*q);
                    w3.end_arr();
                }
                w3.end_arr();
                w3.comma();
                w3.key("asks");
                w3.begin_arr();
                let mut first = true;
                for (p, q) in s.depth_asks.iter().take(10) {
                    if !first {
                        w3.comma();
                    }
                    first = false;
                    w3.begin_arr();
                    w3.uint(*p);
                    w3.comma();
                    w3.uint(*q);
                    w3.end_arr();
                }
                w3.end_arr();
                w3.end_obj();
                let _ = tx_outbox.try_send(crate::ws::encode_text(&w3.finish()));
            }
        }
    }
}

// ----------------------------------------------------------------------
// Writer loop (outbox drain + ping)
// ----------------------------------------------------------------------

fn writer_loop(
    ctx: Arc<VenueCtx>,
    write_lock: Arc<Mutex<TcpStream>>,
    rx: Receiver<Vec<u8>>,
) {
    let ping_every = Duration::from_millis(ctx.cfg.ping_ms);
    let mut last_ping = std::time::Instant::now();
    let ping_frame = crate::ws::encode(crate::ws::Opcode::Ping, b"ns-venue");
    loop {
        // Drain available frames without blocking indefinitely.
        match rx.recv_timeout(ping_every) {
            Ok(frame) => {
                if write_all(&write_lock, &frame).is_err() {
                    break;
                }
                ctx.metrics.tx_frames.fetch_add(1, Ordering::Relaxed);
                // Opportunistically drain more.
                loop {
                    match rx.try_recv() {
                        Ok(frame) => {
                            if write_all(&write_lock, &frame).is_err() {
                                return;
                            }
                            ctx.metrics.tx_frames.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if write_all(&write_lock, &ping_frame).is_err() {
                    break;
                }
                let _ = last_ping;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        last_ping = std::time::Instant::now();
    }
}
