//! The production venue: assembles the engine shards, the WAL, the clock,
//! the market-data fan-out and the network gateway into one process.
//!
//! Threading model (all zero-dependency `std`):
//!
//! ```text
//! acceptor thread ──accept──▶ conn reader thread ──REST──▶ await registry
//!        │                        │                           │
//!        │                        └──WS frames──▶ subs         ▼
//!        │                                          shard queues (per underlying)
//!        │                                           │  (Cmd, await_id)
//!        ▼                                           ▼
//! conn writer thread ◀──outbox frames── router ◀── shard workers
//!   (ping/pong, slow-consumer drop)          (execute → events → replica
//!                                              + fan-out + await completion)
//! clock thread ──Advance(1–10 ms)──▶ shard queues
//! sim-feed thread (optional) ──Oracle──▶ shard queues
//! ```
//!
//! * Every event is routed from the worker that produced it — single
//!   writer per shard, so the read replica can never disagree with the
//!   journal-derived truth.
//! * REST request/response rides the same command stream: the gateway
//!   registers an awaiter, the worker completes it with the final JSON
//!   after the engine executed the command (response ordering is queue
//!   ordering — no races).
//! * The WS path is the low-latency ingress: order/cancel ops enqueue with
//!   the connection as awaiter; acks, fills and cancels stream back on the
//!   private channel.

use crate::auth::{Keyring, Scope};
use crate::engine::{Cmd, Shard, ShardConfig};
use crate::fees::Tier;
use crate::journal::{CmdLog, Event, EvBuf};
use crate::platform::{global_symbol, symbol_local, symbol_shard};
use crate::replica::{
    BalanceRow, OrderRow, PositionRow, Replica, ShardMetrics, TapeRow, UnderlyingRow,
};
use crate::symbol::Listing;
use crate::types::*;
use std::collections::{HashMap, HashSet};
use std::io::Write as IoWrite;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Nanoseconds per millisecond.
pub const NANOS_PER_MS: u64 = 1_000_000;
/// Nanoseconds per minute.
pub const NANOS_PER_MIN: u64 = 60 * NANOS_PER_MS * 1_000;

/// A seed API key from configuration.
#[derive(Clone, Debug)]
pub struct SeedKey {
    /// Key id as 8 hex chars.
    pub key_id: u32,
    /// 32-byte secret as 64 hex chars.
    pub secret_hex: String,
    /// Owning account.
    pub account: AcctId,
    /// Scope.
    pub scope: Scope,
}

/// Auto-listed session ladder configuration.
#[derive(Clone, Debug)]
pub struct LadderCfg {
    /// Initial underlying marks (one per underlying, ticks).
    pub marks: Vec<Price>,
    /// How many upcoming hourly sessions to list.
    pub sessions: u32,
    /// Strike increment in ticks.
    pub strike_step: Price,
    /// Span around the mark in percent (e.g. 3 = ±3%).
    pub span_pct: u32,
}

/// Venue-wide configuration (validated at boot; nonsense values abort).
#[derive(Clone, Debug)]
pub struct VenueConfig {
    /// Bind address, host:port.
    pub bind: String,
    /// Underlying ids to trade (one shard each).
    pub underlyings: Vec<UnderlyingId>,
    /// Per-shard engine configuration.
    pub shard: ShardConfig,
    /// WAL directory (file persistence + boot replay). None = memory only.
    pub wal_dir: Option<PathBuf>,
    /// Maximum simultaneous TCP connections.
    pub max_conns: usize,
    /// Maximum connections per client IP.
    pub per_ip_conns: u16,
    /// WS ping interval.
    pub ping_ms: u64,
    /// WS idle timeout (no frames → drop).
    pub ws_idle_ms: u64,
    /// HTTP header-read deadline (slowloris defense).
    pub header_deadline_ms: u64,
    /// REST keep-alive idle timeout.
    pub rest_idle_ms: u64,
    /// REST engine-await timeout.
    pub rest_timeout_ms: u64,
    /// WS client-message budget per 10 s.
    pub ws_msg_per_10s: u64,
    /// Maximum subscriptions per WS connection.
    pub max_subs: usize,
    /// WS outbox depth before slow-consumer disconnect.
    pub ws_outbox_depth: usize,
    /// Seed keys installed at boot.
    pub seed_keys: Vec<SeedKey>,
    /// Accounts created at boot: (id, tier).
    pub seed_accounts: Vec<(AcctId, Tier)>,
    /// Deposits at boot: (account, underlying, ticks).
    pub seed_deposits: Vec<(AcctId, UnderlyingId, Cash)>,
    /// Auto-listed session ladder.
    pub ladder: Option<LadderCfg>,
    /// Run the synthetic mark feed.
    pub sim_feed: bool,
    /// Load-test tuning: raises engine rate limits (benchmarking only).
    pub loadtest: bool,
    /// Clock tick granularity.
    pub clock_ms: u64,
    /// Auto-drain after this duration (used by tests and demos).
    pub drain_after: Option<Duration>,
}

impl Default for VenueConfig {
    fn default() -> Self {
        VenueConfig {
            bind: "127.0.0.1:8080".into(),
            underlyings: vec![0],
            shard: ShardConfig::default_config(),
            wal_dir: None,
            max_conns: 512,
            per_ip_conns: 16,
            ping_ms: 15_000,
            ws_idle_ms: 30_000,
            header_deadline_ms: 10_000,
            rest_idle_ms: 30_000,
            rest_timeout_ms: 250,
            ws_msg_per_10s: 100,
            max_subs: 64,
            ws_outbox_depth: 1024,
            seed_keys: Vec::new(),
            seed_accounts: Vec::new(),
            seed_deposits: Vec::new(),
            ladder: None,
            sim_feed: false,
            loadtest: false,
            clock_ms: 10,
            drain_after: None,
        }
    }
}

impl VenueConfig {
    /// Validate; returns a human-readable error for nonsense values.
    pub fn validate(&self) -> Result<(), String> {
        if self.underlyings.is_empty() || self.underlyings.len() > 4096 {
            return Err("underlyings must be 1..=4096".into());
        }
        if self.underlyings.contains(&u16::MAX) {
            return Err("underlying id u16::MAX is reserved".into());
        }
        let ids: HashSet<_> = self.underlyings.iter().collect();
        if ids.len() != self.underlyings.len() {
            return Err("duplicate underlyings".into());
        }
        if self.to_addr().is_err() {
            return Err(format!("bad bind address: {}", self.bind));
        }
        if self.max_conns == 0 || self.max_conns > 16_384 {
            return Err("max_conns must be 1..=16384".into());
        }
        if self.per_ip_conns == 0 {
            return Err("per_ip_conns must be >= 1".into());
        }
        if self.clock_ms == 0 || self.clock_ms > 1_000 {
            return Err("clock_ms must be 1..=1000".into());
        }
        if self.rest_timeout_ms == 0 || self.rest_timeout_ms > 5_000 {
            return Err("rest_timeout_ms must be 1..=5000".into());
        }
        if self.ws_msg_per_10s == 0 {
            return Err("ws_msg_per_10s must be >= 1".into());
        }
        if self.max_subs == 0 || self.max_subs > 1_024 {
            return Err("max_subs must be 1..=1024".into());
        }
        if self.shard.max_order_qty == 0 {
            return Err("max_order_qty must be >= 1".into());
        }
        if self.shard.max_open_orders == 0 {
            return Err("max_open_orders must be >= 1".into());
        }
        if let Some(l) = &self.ladder {
            if l.marks.len() != self.underlyings.len() {
                return Err("ladder.marks must align with underlyings".into());
            }
            if l.sessions == 0 || l.sessions > 24 {
                return Err("ladder.sessions must be 1..=24".into());
            }
            if l.strike_step == 0 {
                return Err("ladder.strike_step must be >= 1".into());
            }
        }
        for k in &self.seed_keys {
            if crate::auth::parse_key_id(&crate::auth::key_id_hex(k.key_id)).is_none() {
                return Err("bad seed key id".into());
            }
            if k.secret_hex.len() != 64 || crate::crypto::unhex(&k.secret_hex).is_none() {
                return Err("bad seed key secret (need 64 hex chars)".into());
            }
        }
        Ok(())
    }

    fn to_addr(&self) -> Result<std::net::SocketAddr, String> {
        self.bind
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .next()
            .ok_or_else(|| "unresolvable bind".into())
    }
}

/// Gateway-level live metrics (lock-free counters).
#[derive(Default)]
pub struct GatewayMetrics {
    /// Live connections.
    pub conns: AtomicUsize,
    /// Live WS connections.
    pub ws_conns: AtomicUsize,
    /// REST requests served.
    pub rest_requests: AtomicU64,
    /// WS client messages processed.
    pub ws_messages: AtomicU64,
    /// Frames pushed to clients.
    pub tx_frames: AtomicU64,
    /// 429 responses.
    pub rate_limited: AtomicU64,
    /// Authentication failures.
    pub auth_fails: AtomicU64,
    /// Slow consumers dropped.
    pub slow_dropped: AtomicU64,
    /// Messages rejected as malformed.
    pub malformed: AtomicU64,
}

/// How an in-flight command's outcome is delivered.
pub enum Await {
    /// REST waiter: receives the final JSON body.
    Rest(Mutex<Option<SyncSender<String>>>),
    /// WS connection: receives WS frames on its outbox.
    Ws(u64),
}

/// One live connection registered with the router.
pub struct ConnEntry {
    /// Owning account (private-event routing).
    pub account: AcctId,
    /// WS frames outbox (bounded; full = slow consumer).
    pub outbox: SyncSender<Vec<u8>>,
    /// Is a WS session?
    pub ws: bool,
    /// Still alive?
    pub alive: AtomicBool,
}

/// Subscription key: (channel, symbol-or-underlying).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SubKey {
    /// Channel id (see `docs/0DTE_API.md` §3).
    pub chan: u8,
    /// Symbol (global) or underlying id.
    pub key: u32,
}

/// Channel ids.
pub mod chan {
    /// Best bid/offer.
    pub const BBO: u8 = 1;
    /// Public trades tape.
    pub const TRADE: u8 = 2;
    /// Throttled depth.
    pub const DEPTH: u8 = 3;
    /// Phase transitions.
    pub const PHASE: u8 = 4;
    /// Option mark.
    pub const MARK: u8 = 5;
    /// Underlying state.
    pub const UNDERLYING: u8 = 6;
}

/// The shared venue context every component holds.
pub struct VenueCtx {
    /// Configuration (as started).
    pub cfg: VenueConfig,
    /// API keys.
    pub keyring: Keyring,
    /// Read replica.
    pub replica: Replica,
    /// Pending command awaiters.
    pub awaits: Mutex<HashMap<u64, Await>>,
    /// Live connections.
    pub conns: Mutex<HashMap<u64, ConnEntry>>,
    /// account -> conn ids.
    pub by_account: Mutex<HashMap<AcctId, Vec<u64>>>,
    /// subscription -> conn ids.
    pub subs: Mutex<HashMap<SubKey, Vec<u64>>>,
    /// Per-shard command queues: (cmd, await_id).
    pub queues: Vec<std::sync::Arc<VenueQueue>>,
    /// Underlying per shard index.
    pub underlyings: Vec<UnderlyingId>,
    /// Accounts known to exist (created via admin or boot).
    pub known_accounts: Mutex<HashSet<AcctId>>,
    /// Logical now (nanos).
    pub now_ns: AtomicU64,
    /// Wall-clock boot (for uptime).
    pub boot: SystemTime,
    /// Gateway metrics.
    pub metrics: GatewayMetrics,
    /// Draining (no new requests).
    pub draining: AtomicBool,
    /// Stop signal for workers.
    pub stopping: AtomicBool,
    /// Monotonic await ids.
    next_await: AtomicU64,
    /// Monotonic conn ids.
    next_conn: AtomicU64,
}

impl VenueCtx {
    /// Next await id.
    pub fn alloc_await(&self) -> u64 {
        self.next_await.fetch_add(1, Ordering::Relaxed) + 1
    }
    /// Next conn id.
    pub fn alloc_conn(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed) + 1
    }
    /// Engine now (nanos).
    pub fn now(&self) -> u64 {
        self.now_ns.load(Ordering::Acquire)
    }
    /// Engine now (ms).
    pub fn now_ms(&self) -> u64 {
        self.now() / NANOS_PER_MS
    }
    /// Uptime in ms.
    pub fn uptime_ms(&self) -> u64 {
        self.boot
            .elapsed()
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    /// Is the venue draining?
    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Register an awaiter; returns its id.
    pub fn register_await(&self, a: Await) -> u64 {
        let id = self.alloc_await();
        self.awaits.lock().unwrap().insert(id, a);
        id
    }

    /// Drop an awaiter (timeout path).
    pub fn drop_await(&self, id: u64) {
        self.awaits.lock().unwrap().remove(&id);
    }

    /// Enqueue one command on the right shard. `symbol` is a GLOBAL symbol
    /// id for order commands.
    pub fn enqueue(&self, cmd: Cmd, symbol: SymbolId, await_id: u64) -> bool {
        let shard = symbol_shard(symbol) as usize;
        if shard >= self.queues.len() {
            return false;
        }
        self.queues[shard].push((cmd, await_id));
        true
    }

    /// Enqueue an admin command addressed by underlying.
    pub fn enqueue_underlying(&self, cmd: Cmd, underlying: UnderlyingId, await_id: u64) -> bool {
        match self.underlyings.iter().position(|&u| u == underlying) {
            Some(i) => {
                self.queues[i].push((cmd, await_id));
                true
            }
            None => false,
        }
    }

    /// Push an event frame to one connection (bounded; drops slow ones).
    pub fn push_frame(&self, conn: u64, frame: Vec<u8>) {
        let entry = {
            let conns = self.conns.lock().unwrap();
            conns.get(&conn).map(|c| (c.outbox.clone(), c.alive.load(Ordering::Acquire)))
        };
        if let Some((outbox, alive)) = entry {
            if !alive {
                return;
            }
            if outbox.try_send(frame).is_err() {
                self.metrics.slow_dropped.fetch_add(1, Ordering::Relaxed);
                self.kill_conn(conn);
            } else {
                self.metrics.tx_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Push to every connection subscribed to (chan, key).
    pub fn push_subscribed(&self, key: SubKey, frame: &[u8]) {
        let targets: Vec<u64> = {
            let subs = self.subs.lock().unwrap();
            match subs.get(&key) {
                Some(v) => v.clone(),
                None => return,
            }
        };
        for conn in targets {
            self.push_frame(conn, frame.to_vec());
        }
    }

    /// Push a private frame to every live connection of an account.
    pub fn push_account(&self, account: AcctId, frame: &[u8]) {
        let targets: Vec<u64> = {
            let by_acct = self.by_account.lock().unwrap();
            match by_acct.get(&account) {
                Some(v) => v.clone(),
                None => return,
            }
        };
        for conn in targets {
            self.push_frame(conn, frame.to_vec());
        }
    }

    /// Mark a connection dead and deregister it.
    pub fn kill_conn(&self, conn: u64) {
        if let Some(entry) = self.conns.lock().unwrap().get(&conn) {
            entry.alive.store(false, Ordering::Release);
        }
        self.deregister_conn(conn);
    }

    /// Remove a connection from all router indexes (idempotent).
    pub fn deregister_conn(&self, conn: u64) {
        let account = {
            let mut conns = self.conns.lock().unwrap();
            conns.remove(&conn).map(|c| c.account)
        };
        if let Some(account) = account {
            if let Some(by_acct) = self.by_account.lock().unwrap().get_mut(&account) {
                by_acct.retain(|&c| c != conn);
            }
        }
        let mut subs = self.subs.lock().unwrap();
        let empty: Vec<SubKey> = subs
            .iter_mut()
            .filter_map(|(k, v)| {
                v.retain(|&c| c != conn);
                if v.is_empty() {
                    Some(*k)
                } else {
                    None
                }
            })
            .collect();
        for k in empty {
            subs.remove(&k);
        }
    }
}

// ----------------------------------------------------------------------
// Command queue with await metadata
// ----------------------------------------------------------------------

/// MPSC command queue: `(Cmd, await_id)` pairs, condvar-woken.
pub struct VenueQueue {
    inner: Mutex<Vec<(Cmd, u64)>>,
    cv: Condvar,
    closed: AtomicBool,
    depth: AtomicUsize,
}

impl Default for VenueQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl VenueQueue {
    /// New queue.
    pub fn new() -> VenueQueue {
        VenueQueue {
            inner: Mutex::new(Vec::with_capacity(1024)),
            cv: Condvar::new(),
            closed: AtomicBool::new(false),
            depth: AtomicUsize::new(0),
        }
    }

    /// Push one item.
    pub fn push(&self, item: (Cmd, u64)) {
        let mut g = self.inner.lock().unwrap();
        g.push(item);
        self.depth.store(g.len(), Ordering::Relaxed);
        drop(g);
        self.cv.notify_one();
    }

    /// Wait for items (blocks until one arrives or the queue closes).
    pub fn wait_drain(&self, max: usize) -> Vec<(Cmd, u64)> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if !g.is_empty() {
                let n = g.len().min(max);
                let out: Vec<(Cmd, u64)> = g.drain(..n).collect();
                self.depth.store(g.len(), Ordering::Relaxed);
                return out;
            }
            if self.closed.load(Ordering::Acquire) {
                return Vec::new();
            }
            g = self.cv.wait(g).unwrap();
        }
    }

    /// Drain up to `max` without blocking.
    pub fn try_drain(&self, max: usize) -> Vec<(Cmd, u64)> {
        let mut g = self.inner.lock().unwrap();
        let n = g.len().min(max);
        let out: Vec<(Cmd, u64)> = g.drain(..n).collect();
        self.depth.store(g.len(), Ordering::Relaxed);
        out
    }

    /// Pending depth (approximate).
    pub fn depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }

    /// Close (workers exit once empty).
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cv.notify_all();
    }
}

// ----------------------------------------------------------------------
// WAL sink with periodic fsync
// ----------------------------------------------------------------------

/// A file writer that fsyncs every `sync_every` flushes.
pub struct SyncWriter {
    file: std::fs::File,
    sync_every: u64,
    count: u64,
}

impl SyncWriter {
    /// Open (append) a WAL file with fsync every N flushes.
    pub fn open(path: &std::path::Path, sync_every: u64) -> std::io::Result<SyncWriter> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(SyncWriter {
            file,
            sync_every,
            count: 0,
        })
    }
}

impl IoWrite for SyncWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        self.count += 1;
        if self.sync_every > 0 && self.count % self.sync_every == 0 {
            self.file.sync_all()?;
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------
// Event routing: engine events -> replica + fan-out + await completion
// ----------------------------------------------------------------------

/// Fill summary for REST acks.
#[derive(Clone, Copy, Debug)]
pub struct FillLite {
    /// Execution price.
    pub price: Price,
    /// Quantity.
    pub qty: u32,
    /// Fee charged to the taker.
    pub fee: u64,
}

/// The outcome of one awaited command.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// A place (or the place leg of a replace).
    Place {
        /// Accepted?
        ok: bool,
        /// Resting order id (0 if fully filled / rejected).
        order: u64,
        /// Leaves resting.
        leaves: u32,
        /// Fills (price, qty, taker fee).
        fills: Vec<FillLite>,
        /// Reject code + reason (when !ok).
        code: u8,
        /// Reject reason name.
        reason: &'static str,
    },
    /// A cancel.
    Cancel {
        /// Cancelled?
        ok: bool,
        /// Leaves at cancel.
        leaves: u32,
        /// Reject code (when !ok).
        code: u8,
        /// Reject reason name.
        reason: &'static str,
    },
    /// A replace: cancel leg + place leg.
    Replace {
        /// Old order cancelled.
        canceled: bool,
        /// The place outcome.
        place: Box<Outcome>,
    },
    /// Session ladder listing.
    Sessions {
        /// Global symbol ids in order.
        symbols: Vec<SymbolId>,
    },
    /// Deposit/withdraw/halt/resume/etc.
    Generic {
        /// Acknowledged by the engine?
        ok: bool,
    },
}

fn reason_name(code: u8) -> &'static str {
    match code {
        0 => "UnknownSymbol",
        1 => "UnknownAccount",
        2 => "PriceBand",
        3 => "PostOnlyCross",
        4 => "ReduceOnlyWouldIncrease",
        5 => "InsufficientFunds",
        6 => "PositionLimit",
        7 => "RateLimit",
        8 => "Malformed",
        9 => "NotContinuous",
        10 => "FokDepth",
        11 => "DuplicateOrder",
        12 => "UnknownOrder",
        13 => "Halted",
        14 => "DeltaLimit",
        15 => "TooManyOrders",
        16 => "QtyTooLarge",
        17 => "NotOwner",
        _ => "Unknown",
    }
}

/// Extract the outcome of the command just executed (events belong wholly
/// to that command — the worker clears the buffer per command).
pub fn extract_outcome(cmd: &Cmd, ev: &EvBuf) -> Outcome {
    match cmd {
        Cmd::Place(_) => place_outcome(ev),
        Cmd::Cancel { .. } => {
            for e in &ev.events {
                match e {
                    Event::Canceled { leaves, reason, .. } if *reason == 0 => {
                        return Outcome::Cancel {
                            ok: true,
                            leaves: *leaves,
                            code: 0,
                            reason: "",
                        };
                    }
                    Event::Reject { code, .. } => {
                        return Outcome::Cancel {
                            ok: false,
                            leaves: 0,
                            code: *code,
                            reason: reason_name(*code),
                        };
                    }
                    _ => {}
                }
            }
            Outcome::Cancel {
                ok: false,
                leaves: 0,
                code: 12,
                reason: "UnknownOrder",
            }
        }
        Cmd::Replace { .. } => {
            let mut canceled = false;
            let mut cutoff = ev.events.len();
            for (i, e) in ev.events.iter().enumerate() {
                match e {
                    Event::Canceled { reason: 5, .. } => {
                        canceled = true;
                    }
                    Event::Ack { .. } | Event::Reject { .. } => {
                        // Place leg starts at the first Ack/Reject after the
                        // cancel leg.
                        cutoff = cutoff.min(i);
                        break;
                    }
                    _ => {}
                }
            }
            let place_ev = EvBuf {
                events: ev.events[cutoff.min(ev.events.len())..].to_vec(),
            };
            Outcome::Replace {
                canceled,
                place: Box::new(place_outcome(&place_ev)),
            }
        }
        Cmd::CreateSymbol { .. } => {
            let mut symbols = Vec::new();
            for e in &ev.events {
                if let Event::SymbolListed { symbol, .. } = e {
                    symbols.push(*symbol);
                }
            }
            Outcome::Sessions { symbols }
        }
        Cmd::Deposit { .. } | Cmd::Withdraw { .. } | Cmd::Halt | Cmd::ResumeNow => {
            let ok = ev.events.iter().any(|e| {
                matches!(
                    e,
                    Event::Deposited { .. }
                        | Event::Withdrawn { .. }
                        | Event::Halted { .. }
                        | Event::Resumed { .. }
                )
            });
            Outcome::Generic { ok }
        }
        _ => Outcome::Generic { ok: true },
    }
}

fn place_outcome(ev: &EvBuf) -> Outcome {
    let mut fills = Vec::new();
    let mut order = 0u64;
    let mut leaves = 0u32;
    for e in &ev.events {
        match e {
            Event::Trade {
                price, qty, taker_fee, ..
            } => fills.push(FillLite {
                price: *price,
                qty: *qty,
                fee: *taker_fee,
            }),
            Event::Ack {
                order: o, leaves: l, ..
            } => {
                order = *o;
                leaves = *l;
            }
            Event::Reject { code, .. } => {
                return Outcome::Place {
                    ok: false,
                    order: 0,
                    leaves: 0,
                    fills,
                    code: *code,
                    reason: reason_name(*code),
                };
            }
            _ => {}
        }
    }
    Outcome::Place {
        ok: true,
        order,
        leaves,
        fills,
        code: 0,
        reason: "",
    }
}

/// Serialize an outcome to its REST/WS JSON body.
pub fn outcome_json(o: &Outcome) -> String {
    let mut w = crate::json::Writer::new();
    match o {
        Outcome::Place {
            ok,
            order,
            leaves,
            fills,
            code,
            reason,
        } => {
            w.begin_obj();
            w.key("ok");
            w.bool(*ok);
            w.comma();
            w.key("order_id");
            w.uint(*order);
            w.comma();
            w.key("leaves");
            w.uint(*leaves as u64);
            w.comma();
            w.key("fills");
            w.begin_arr();
            for (i, f) in fills.iter().enumerate() {
                if i > 0 {
                    w.comma();
                }
                w.begin_obj();
                w.key("price");
                w.uint(f.price);
                w.comma();
                w.key("qty");
                w.uint(f.qty as u64);
                w.comma();
                w.key("fee");
                w.uint(f.fee);
                w.comma();
                w.key("rebate");
                w.uint(0);
                w.end_obj();
            }
            w.end_arr();
            if !ok {
                w.comma();
                w.key("code");
                w.uint(*code as u64);
                w.comma();
                w.key("reason");
                w.str(reason);
            }
            w.end_obj();
        }
        Outcome::Cancel {
            ok,
            leaves,
            code,
            reason,
        } => {
            w.begin_obj();
            w.key("ok");
            w.bool(*ok);
            w.comma();
            w.key("leaves");
            w.uint(*leaves as u64);
            if !ok {
                w.comma();
                w.key("code");
                w.uint(*code as u64);
                w.comma();
                w.key("reason");
                w.str(reason);
            }
            w.end_obj();
        }
        Outcome::Replace { canceled, place } => {
            let inner = match place.as_ref() {
                p @ Outcome::Place { .. } => outcome_json_place_body(p),
                _ => String::new(),
            };
            w.begin_obj();
            w.key("canceled");
            w.bool(*canceled);
            w.comma();
            w.raw(&inner);
            w.end_obj();
        }
        Outcome::Sessions { symbols } => {
            w.begin_obj();
            w.key("ok");
            w.bool(true);
            w.comma();
            w.key("symbols");
            w.begin_arr();
            for (i, s) in symbols.iter().enumerate() {
                if i > 0 {
                    w.comma();
                }
                w.uint(*s as u64);
            }
            w.end_arr();
            w.end_obj();
        }
        Outcome::Generic { ok } => {
            w.begin_obj();
            w.key("ok");
            w.bool(*ok);
            w.end_obj();
        }
    }
    w.finish()
}

/// The place body without the outer braces (for replace composition).
fn outcome_json_place_body(p: &Outcome) -> String {
    let full = outcome_json(p);
    full.trim_start_matches('{').trim_end_matches('}').to_string()
}

/// Deliver a command outcome to its awaiter. REST waiters receive the
/// resource-style JSON body; WS connections receive the event-style frames
/// from the contract (§3): `ack` / `reject` / `canceled`.
pub fn complete_await(ctx: &VenueCtx, await_id: u64, ev: &EvBuf, cmd: &Cmd, shard_idx: u32) {
    if await_id == 0 {
        return;
    }
    let awaiter = ctx.awaits.lock().unwrap().remove(&await_id);
    let Some(a) = awaiter else { return };
    let outcome = extract_outcome(cmd, ev);
    // Fill in global symbol ids for session listings.
    let outcome = match outcome {
        Outcome::Sessions { symbols } => Outcome::Sessions {
            symbols: symbols
                .into_iter()
                .map(|s| global_symbol(shard_idx, s))
                .collect(),
        },
        o => o,
    };
    match a {
        Await::Rest(tx) => {
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.try_send(outcome_json(&outcome));
            }
        }
        Await::Ws(conn) => {
            for frame in outcome_ws_frames(&outcome, cmd, shard_idx) {
                ctx.push_frame(conn, frame);
            }
        }
    }
}

/// Event-style WS frames for a completed awaiter (private channel).
fn outcome_ws_frames(outcome: &Outcome, cmd: &Cmd, shard_idx: u32) -> Vec<Vec<u8>> {
    let gs = |s: SymbolId| global_symbol(shard_idx, s);
    let ms = crate::venue::NANOS_PER_MS;
    let now_ms = 0u64; // filled per-frame below by callers with ctx time
    let _ = (ms, now_ms);
    let mut out = Vec::new();
    match outcome {
        Outcome::Place {
            ok,
            order,
            leaves,
            code,
            reason,
            ..
        } => {
            let symbol = match cmd {
                Cmd::Place(o) => gs(o.symbol),
                _ => 0,
            };
            let mut w = crate::json::Writer::new();
            w.begin_obj();
            if *ok {
                w.key("t");
                w.str("ack");
                w.comma();
                w.key("order");
                w.uint(*order);
                w.comma();
                w.key("symbol");
                w.uint(symbol as u64);
                w.comma();
                w.key("leaves");
                w.uint(*leaves as u64);
            } else {
                w.key("t");
                w.str("reject");
                w.comma();
                w.key("order");
                w.uint(*order);
                w.comma();
                w.key("symbol");
                w.uint(symbol as u64);
                w.comma();
                w.key("code");
                w.uint(*code as u64);
                w.comma();
                w.key("reason");
                w.str(reason);
            }
            w.end_obj();
            out.push(crate::ws::encode_text(&w.finish()));
        }
        Outcome::Cancel {
            ok,
            leaves,
            code,
            reason,
        } => {
            let (order, symbol) = match cmd {
                Cmd::Cancel { order, symbol, .. } => (*order, gs(*symbol)),
                _ => (0, 0),
            };
            let mut w = crate::json::Writer::new();
            w.begin_obj();
            if *ok {
                w.key("t");
                w.str("canceled");
                w.comma();
                w.key("order");
                w.uint(order);
                w.comma();
                w.key("symbol");
                w.uint(symbol as u64);
                w.comma();
                w.key("leaves");
                w.uint(*leaves as u64);
                w.comma();
                w.key("reason");
                w.str("user");
            } else {
                w.key("t");
                w.str("reject");
                w.comma();
                w.key("order");
                w.uint(order);
                w.comma();
                w.key("symbol");
                w.uint(symbol as u64);
                w.comma();
                w.key("code");
                w.uint(*code as u64);
                w.comma();
                w.key("reason");
                w.str(reason);
            }
            w.end_obj();
            out.push(crate::ws::encode_text(&w.finish()));
        }
        Outcome::Replace { canceled, place } => {
            if *canceled {
                let (order, symbol) = match cmd {
                    Cmd::Replace { order, symbol, .. } => (*order, gs(*symbol)),
                    _ => (0, 0),
                };
                let mut w = crate::json::Writer::new();
                w.begin_obj();
                w.key("t");
                w.str("canceled");
                w.comma();
                w.key("order");
                w.uint(order);
                w.comma();
                w.key("symbol");
                w.uint(symbol as u64);
                w.comma();
                w.key("reason");
                w.str("replace");
                w.end_obj();
                out.push(crate::ws::encode_text(&w.finish()));
            }
            out.extend(outcome_ws_frames(place, cmd, shard_idx));
        }
        _ => {
            // Sessions/generic have no private WS event; REST covers them.
        }
    }
    out
}

/// Route one command's events into the replica and the fan-out. Also
/// returns the set of accounts touched (for balance snapshots).
pub fn apply_events(
    ctx: Option<&VenueCtx>,
    replica: &Replica,
    shard_idx: u32,
    underlying: UnderlyingId,
    ev: &EvBuf,
    now_ns: u64,
    touched: &mut HashSet<AcctId>,
) -> Vec<SymbolId> {
    let g = |s: SymbolId| global_symbol(shard_idx, s);
    let mut dirty_symbols = Vec::new();
    let ms = now_ns / NANOS_PER_MS;
    for e in &ev.events {
        match e {
            Event::SymbolListed { symbol, listing_key } => {
                replica.upsert_symbol(g(*symbol), Listing::unpack(*listing_key));
            }
            Event::Ack {
                order, account, symbol, leaves, buy, price, qty, tif, kind, flags,
            } => {
                touched.insert(*account);
                if *order != 0 && *leaves > 0 {
                    replica.order_rest(OrderRow {
                        order: *order,
                        symbol: g(*symbol),
                        account: *account,
                        buy: *buy,
                        price: *price,
                        qty: *qty,
                        leaves: *leaves,
                        tif: *tif,
                        kind: *kind,
                        post_only: *flags & crate::types::flags::POST_ONLY != 0,
                        reduce_only: *flags & crate::types::flags::REDUCE_ONLY != 0,
                        ts: ms,
                    });
                }
            }
            Event::Reject { account, .. } => {
                touched.insert(*account);
            }
            Event::Canceled { account, order, .. } => {
                touched.insert(*account);
                replica.order_gone(*order);
            }
            Event::Trade {
                symbol,
                price,
                qty,
                taker,
                maker,
                taker_acct,
                maker_acct,
                aggressor,
                taker_fee,
                maker_fee,
                maker_rebate,
            } => {
                touched.insert(*taker_acct);
                touched.insert(*maker_acct);
                replica.order_fill(*maker, *qty);
                replica.order_fill(*taker, *qty);
                replica.push_tape(
                    g(*symbol),
                    TapeRow {
                        price: *price,
                        qty: *qty,
                        aggressor: *aggressor,
                        ts: now_ns,
                    },
                );
                if let Some(ctx) = ctx {
                    let gs = g(*symbol);
                    // Public trade frame.
                    let aggr = match aggressor {
                        0 => "buy",
                        1 => "sell",
                        _ => "auction",
                    };
                    let mut w = crate::json::Writer::new();
                    w.begin_obj();
                    w.key("t");
                    w.str("trade");
                    w.comma();
                    w.key("symbol");
                    w.uint(gs as u64);
                    w.comma();
                    w.key("price");
                    w.uint(*price);
                    w.comma();
                    w.key("qty");
                    w.uint(*qty as u64);
                    w.comma();
                    w.key("aggressor");
                    w.str(aggr);
                    w.comma();
                    w.key("ms");
                    w.uint(ms);
                    w.end_obj();
                    ctx.push_subscribed(
                        SubKey { chan: chan::TRADE, key: gs },
                        &crate::ws::encode_text(&w.finish()),
                    );
                    // Private fills.
                    let mut wt = crate::json::Writer::new();
                    wt.begin_obj();
                    wt.key("t");
                    wt.str("fill");
                    wt.comma();
                    wt.key("symbol");
                    wt.uint(gs as u64);
                    wt.comma();
                    wt.key("price");
                    wt.uint(*price);
                    wt.comma();
                    wt.key("qty");
                    wt.uint(*qty as u64);
                    wt.comma();
                    wt.key("order");
                    wt.uint(*taker);
                    wt.comma();
                    wt.key("role");
                    wt.str("taker");
                    wt.comma();
                    wt.key("fee");
                    wt.uint(*taker_fee);
                    wt.comma();
                    wt.key("rebate");
                    wt.uint(0);
                    wt.comma();
                    wt.key("ms");
                    wt.uint(ms);
                    wt.end_obj();
                    ctx.push_account(*taker_acct, &crate::ws::encode_text(&wt.finish()));
                    let mut wm = crate::json::Writer::new();
                    wm.begin_obj();
                    wm.key("t");
                    wm.str("fill");
                    wm.comma();
                    wm.key("symbol");
                    wm.uint(gs as u64);
                    wm.comma();
                    wm.key("price");
                    wm.uint(*price);
                    wm.comma();
                    wm.key("qty");
                    wm.uint(*qty as u64);
                    wm.comma();
                    wm.key("order");
                    wm.uint(*maker);
                    wm.comma();
                    wm.key("role");
                    wm.str("maker");
                    wm.comma();
                    wm.key("fee");
                    wm.uint(if *maker_rebate { 0 } else { *maker_fee });
                    wm.comma();
                    wm.key("rebate");
                    wm.uint(if *maker_rebate { *maker_fee } else { 0 });
                    wm.comma();
                    wm.key("ms");
                    wm.uint(ms);
                    wm.end_obj();
                    ctx.push_account(*maker_acct, &crate::ws::encode_text(&wm.finish()));
                }
            }
            Event::Mark { symbol, price } => {
                replica.set_mark(g(*symbol), *price);
                if let Some(ctx) = ctx {
                    let gs = g(*symbol);
                    let mut w = crate::json::Writer::new();
                    w.begin_obj();
                    w.key("t");
                    w.str("mark");
                    w.comma();
                    w.key("symbol");
                    w.uint(gs as u64);
                    w.comma();
                    w.key("price");
                    w.uint(*price);
                    w.end_obj();
                    ctx.push_subscribed(
                        SubKey { chan: chan::MARK, key: gs },
                        &crate::ws::encode_text(&w.finish()),
                    );
                }
            }
            Event::Bbo {
                symbol,
                bid,
                bid_qty,
                ask,
                ask_qty,
            } => {
                replica.set_bbo(g(*symbol), (*bid, *bid_qty, *ask, *ask_qty));
                dirty_symbols.push(g(*symbol));
                if let Some(ctx) = ctx {
                    let gs = g(*symbol);
                    let mut w = crate::json::Writer::new();
                    w.begin_obj();
                    w.key("t");
                    w.str("bbo");
                    w.comma();
                    w.key("symbol");
                    w.uint(gs as u64);
                    w.comma();
                    w.key("bid");
                    w.uint(*bid);
                    w.comma();
                    w.key("bid_qty");
                    w.uint(*bid_qty);
                    w.comma();
                    w.key("ask");
                    w.uint(*ask);
                    w.comma();
                    w.key("ask_qty");
                    w.uint(*ask_qty);
                    w.end_obj();
                    ctx.push_subscribed(
                        SubKey { chan: chan::BBO, key: gs },
                        &crate::ws::encode_text(&w.finish()),
                    );
                }
            }
            Event::AuctionOpened { symbol } => {
                replica.set_phase(g(*symbol), crate::engine::Phase::AuctionOpen);
                push_phase(ctx, g(*symbol), "auction");
            }
            Event::AuctionCleared { symbol, .. } => {
                // After a clear the symbol returns to continuous (or expires
                // immediately after — the Expired event supersedes).
                replica.set_phase(g(*symbol), crate::engine::Phase::Continuous);
                push_phase(ctx, g(*symbol), "continuous");
            }
            Event::Expired { symbol, .. } => {
                replica.set_phase(g(*symbol), crate::engine::Phase::Expired);
                push_phase(ctx, g(*symbol), "expired");
            }
            Event::Halted { underlying: u, level } => {
                replica.set_underlying(UnderlyingRow {
                    underlying: *u,
                    mark: 0,
                    reference: 0,
                    fast: false,
                    halted: true,
                });
                if let Some(ctx) = ctx {
                    let mut w = crate::json::Writer::new();
                    w.begin_obj();
                    w.key("t");
                    w.str("halt");
                    w.comma();
                    w.key("underlying");
                    w.uint(*u as u64);
                    w.comma();
                    w.key("level");
                    w.uint(*level as u64);
                    w.end_obj();
                    ctx.push_subscribed(
                        SubKey { chan: chan::UNDERLYING, key: *u as u32 },
                        &crate::ws::encode_text(&w.finish()),
                    );
                }
            }
            Event::Resumed { underlying: u } => {
                replica.set_underlying(UnderlyingRow {
                    underlying: *u,
                    mark: 0,
                    reference: 0,
                    fast: false,
                    halted: false,
                });
                if let Some(ctx) = ctx {
                    let mut w = crate::json::Writer::new();
                    w.begin_obj();
                    w.key("t");
                    w.str("resumed");
                    w.comma();
                    w.key("underlying");
                    w.uint(*u as u64);
                    w.end_obj();
                    ctx.push_subscribed(
                        SubKey { chan: chan::UNDERLYING, key: *u as u32 },
                        &crate::ws::encode_text(&w.finish()),
                    );
                }
            }
            Event::FastMarket { underlying: u, on } => {
                replica.set_underlying(UnderlyingRow {
                    underlying: *u,
                    mark: 0,
                    reference: 0,
                    fast: *on,
                    halted: false,
                });
                push_underlying(ctx, replica, *u);
            }
            Event::UnderlyingMark { underlying: u, price } => {
                replica.set_underlying(UnderlyingRow {
                    underlying: *u,
                    mark: *price,
                    reference: 0,
                    fast: false,
                    halted: false,
                });
                push_underlying(ctx, replica, *u);
            }
            Event::Exercised {
                symbol,
                account,
                qty,
                intrinsic,
                fee,
                cash_flow,
            } => {
                touched.insert(*account);
                if let Some(ctx) = ctx {
                    let gs = g(*symbol);
                    let mut w = crate::json::Writer::new();
                    w.begin_obj();
                    w.key("t");
                    w.str("exercised");
                    w.comma();
                    w.key("symbol");
                    w.uint(gs as u64);
                    w.comma();
                    w.key("qty");
                    w.uint(*qty as u64);
                    w.comma();
                    w.key("intrinsic");
                    w.uint(*intrinsic);
                    w.comma();
                    w.key("cash_flow");
                    w.cash(*cash_flow);
                    w.comma();
                    w.key("fee");
                    w.uint(*fee);
                    w.comma();
                    w.key("ms");
                    w.uint(ms);
                    w.end_obj();
                    ctx.push_account(*account, &crate::ws::encode_text(&w.finish()));
                }
            }
            Event::Deposited { account, amount } => {
                touched.insert(*account);
                push_balance(ctx, replica, *account, underlying, ms, *amount, true);
            }
            Event::Withdrawn { account, amount } => {
                touched.insert(*account);
                push_balance(ctx, replica, *account, underlying, ms, *amount, false);
            }
            Event::MarginDeficit { account, .. } => {
                touched.insert(*account);
            }
        }
    }
    dirty_symbols
}

fn push_phase(ctx: Option<&VenueCtx>, symbol: SymbolId, phase: &str) {
    if let Some(ctx) = ctx {
        let mut w = crate::json::Writer::new();
        w.begin_obj();
        w.key("t");
        w.str("phase");
        w.comma();
        w.key("symbol");
        w.uint(symbol as u64);
        w.comma();
        w.key("phase");
        w.str(phase);
        w.end_obj();
        ctx.push_subscribed(
            SubKey { chan: chan::PHASE, key: symbol },
            &crate::ws::encode_text(&w.finish()),
        );
    }
}

fn push_underlying(ctx: Option<&VenueCtx>, replica: &Replica, u: UnderlyingId) {
    if let Some(ctx) = ctx {
        if let Some(row) = replica.underlying(u) {
            let mut w = crate::json::Writer::new();
            w.begin_obj();
            w.key("t");
            w.str("underlying");
            w.comma();
            w.key("underlying");
            w.uint(u as u64);
            w.comma();
            w.key("price");
            w.uint(row.mark);
            w.comma();
            w.key("ref");
            w.uint(row.reference);
            w.comma();
            w.key("fast");
            w.bool(row.fast);
            w.comma();
            w.key("halted");
            w.bool(row.halted);
            w.end_obj();
            ctx.push_subscribed(
                SubKey { chan: chan::UNDERLYING, key: u as u32 },
                &crate::ws::encode_text(&w.finish()),
            );
        }
    }
}

fn push_balance(
    ctx: Option<&VenueCtx>,
    replica: &Replica,
    account: AcctId,
    underlying: UnderlyingId,
    ms: u64,
    _amount: Cash,
    _dep: bool,
) {
    if let Some(ctx) = ctx {
        let row = replica.account(account);
        if let Some(b) = row.balances.get(&underlying) {
            let mut w = crate::json::Writer::new();
            w.begin_obj();
            w.key("t");
            w.str("balance");
            w.comma();
            w.key("underlying");
            w.uint(underlying as u64);
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
            w.key("ms");
            w.uint(ms);
            w.end_obj();
            ctx.push_account(account, &crate::ws::encode_text(&w.finish()));
        }
    }
}

// ----------------------------------------------------------------------
// Shard worker
// ----------------------------------------------------------------------

/// State carried by one shard worker.
pub struct WorkerState {
    /// Shard index.
    pub shard_idx: u32,
    /// Underlying traded.
    pub underlying: UnderlyingId,
}

/// The shard worker loop: drain the queue, execute, route, snapshot.
pub fn shard_worker(
    mut shard: Shard,
    queue: std::sync::Arc<VenueQueue>,
    ctx: Arc<VenueCtx>,
    state: WorkerState,
) {
    let shard_idx = state.shard_idx;
    let underlying = state.underlying;
    let mut ev = EvBuf::with_capacity(256);
    let mut touched: HashSet<AcctId> = HashSet::new();
    let mut depth_dirty: Vec<SymbolId> = Vec::new();
    let mut last_depth_ns: HashMap<SymbolId, u64> = HashMap::new();
    let mut last_metrics_ns: u64 = 0;
    let mut tps_ema: f64 = 0.0;
    let mut last_orders: u64 = 0;
    let mut last_tps_ns: u64 = 0;
    let mut bids = [(0u64, 0u64); 10];
    let mut asks = [(0u64, 0u64); 10];
    loop {
        if ctx.stopping.load(Ordering::Acquire) && queue.depth() == 0 {
            break;
        }
        let batch = queue.wait_drain(256);
        if batch.is_empty() {
            if ctx.stopping.load(Ordering::Acquire) {
                break;
            }
            continue;
        }
        let now_ns = ctx.now();
        for (cmd, await_id) in batch {
            ev.clear();
            shard.execute(cmd, &mut ev);
            let dirty = apply_events(
                Some(&ctx),
                &ctx.replica,
                shard_idx,
                underlying,
                &ev,
                now_ns,
                &mut touched,
            );
            depth_dirty.extend(dirty);
            complete_await(&ctx, await_id, &ev, &last_cmd_hack(&cmd), shard_idx);
        }
        // Balance/position snapshots for touched accounts (engine truth).
        let mut touched_list: Vec<AcctId> = touched.drain().collect();
        touched_list.sort_unstable();
        touched_list.truncate(512);
        for account in touched_list {
            snapshot_one(&ctx, &mut shard, account, underlying, shard_idx);
        }
        // Throttled depth refresh for dirty symbols.
        let now = ctx.now();
        depth_dirty.retain(|&gsym| {
            let last = *last_depth_ns.get(&gsym).unwrap_or(&0);
            if now.saturating_sub(last) < 100 * NANOS_PER_MS {
                return true; // still dirty, retry next batch
            }
            let local = symbol_local(gsym);
            if local as usize >= shard.symbols.len() {
                return false;
            }
            let nb = shard.symbols[local as usize].book.depth(crate::types::Side::Buy, 10, &mut bids);
            let na = shard.symbols[local as usize].book.depth(crate::types::Side::Sell, 10, &mut asks);
            let bv: Vec<(Price, u64)> = bids[..nb].to_vec();
            let av: Vec<(Price, u64)> = asks[..na].to_vec();
            ctx.replica.set_depth(gsym, bv, av);
            // Broadcast the depth frame.
            let mut w = crate::json::Writer::new();
            w.begin_obj();
            w.key("t");
            w.str("depth");
            w.comma();
            w.key("symbol");
            w.uint(gsym as u64);
            w.comma();
            w.key("bids");
            w.begin_arr();
            write_levels(&mut w, &bids[..nb]);
            w.end_arr();
            w.comma();
            w.key("asks");
            w.begin_arr();
            write_levels(&mut w, &asks[..na]);
            w.end_arr();
            w.end_obj();
            ctx.push_subscribed(
                SubKey { chan: chan::DEPTH, key: gsym },
                &crate::ws::encode_text(&w.finish()),
            );
            last_depth_ns.insert(gsym, now);
            false
        });
        // TPS EMA + periodic full metrics.
        let orders_now = shard.stats.orders;
        let t = ctx.now();
        if last_tps_ns == 0 {
            last_tps_ns = t;
            last_orders = orders_now;
        } else if t.saturating_sub(last_tps_ns) >= 200 * NANOS_PER_MS {
            let dt_s = (t - last_tps_ns) as f64 / 1e9;
            let rate = (orders_now - last_orders) as f64 / dt_s.max(1e-9);
            tps_ema = if tps_ema == 0.0 { rate } else { tps_ema * 0.8 + rate * 0.2 };
            last_tps_ns = t;
            last_orders = orders_now;
        }
        if now.saturating_sub(last_metrics_ns) >= 500 * NANOS_PER_MS {
            last_metrics_ns = now;
            let m = ShardMetrics {
                shard: shard_idx,
                underlying,
                orders: shard.stats.orders,
                rejects: shard.stats.rejects,
                trades: shard.stats.trades,
                volume: shard.stats.volume,
                fees: shard.stats.fees,
                cancels: shard.stats.cancels,
                cmds: shard.stats.cmds,
                expiries: shard.stats.expiries,
                auctions: shard.stats.auctions,
                queue_depth: queue.depth() as u32,
                journal_bytes: shard.journal.written(),
                state_hash: shard.state_hash(),
                conservation: shard.conservation(),
                tps_ema,
            };
            let mut all = ctx.replica.shard_metrics();
            while all.len() <= shard_idx as usize {
                all.push(ShardMetrics::default());
            }
            all[shard_idx as usize] = m;
            ctx.replica.set_shard_metrics(all);
        }
        // Flush the WAL each batch (auto-batched internally at 256 KiB).
        shard.flush_journal();
    }
    // Final flush + metrics on drain.
    shard.flush_journal();
    let m = ShardMetrics {
        shard: shard_idx,
        underlying,
        orders: shard.stats.orders,
        rejects: shard.stats.rejects,
        trades: shard.stats.trades,
        volume: shard.stats.volume,
        fees: shard.stats.fees,
        cancels: shard.stats.cancels,
        cmds: shard.stats.cmds,
        expiries: shard.stats.expiries,
        auctions: shard.stats.auctions,
        queue_depth: 0,
        journal_bytes: shard.journal.written(),
        state_hash: shard.state_hash(),
        conservation: shard.conservation(),
        tps_ema,
    };
    let mut all = ctx.replica.shard_metrics();
    while all.len() <= shard_idx as usize {
        all.push(ShardMetrics::default());
    }
    all[shard_idx as usize] = m;
    ctx.replica.set_shard_metrics(all);
}

/// The await completion needs the command that produced the events; the
/// batch loop owns it, so we thread a copy through.
fn last_cmd_hack(cmd: &Cmd) -> Cmd {
    *cmd
}

fn write_levels(w: &mut crate::json::Writer, levels: &[(Price, u64)]) {
    for (i, (p, q)) in levels.iter().enumerate() {
        if i > 0 {
            w.comma();
        }
        w.begin_arr();
        w.uint(*p);
        w.comma();
        w.uint(*q);
        w.end_arr();
    }
}

/// Copy one account's engine truth into the replica, then push a fresh
/// private balance frame for it.
fn snapshot_one(
    ctx: &VenueCtx,
    shard: &mut Shard,
    account: AcctId,
    underlying: UnderlyingId,
    shard_idx: u32,
) {
    let idx = account as usize;
    if idx >= shard.accounts.len() || shard.accounts[idx].id == crate::book::NIL {
        return;
    }
    let a = &shard.accounts[idx];
    let balance = BalanceRow {
        cash: a.cash,
        hold: a.hold,
        margin_required: a.margin_required,
        margin_reserved: a.margin_reserved,
        fees_paid: a.fees_paid,
        rebates_recv: a.rebates_recv,
    };
    let positions = a
        .pos
        .iter()
        .filter_map(|&(local_sym, slot)| {
            let p = shard.positions.get(slot as usize).copied()?;
            if p.qty == 0 {
                return None;
            }
            Some(PositionRow {
                symbol: global_symbol(shard_idx, local_sym),
                qty: p.qty,
                margin: p.margin,
                delta_notional: p.delta_notional,
            })
        })
        .collect();
    ctx.replica
        .snapshot_account(account, underlying, shard_idx, balance, positions);
    push_balance(Some(ctx), &ctx.replica, account, underlying, ctx.now() / NANOS_PER_MS, 0, true);
}

// ----------------------------------------------------------------------
// The venue
// ----------------------------------------------------------------------

/// A running venue instance.
pub struct Venue {
    /// Shared context.
    pub ctx: Arc<VenueCtx>,
    /// Worker join handles (in shard order).
    workers: Vec<std::thread::JoinHandle<()>>,
    /// Clock thread handle.
    clock: Option<std::thread::JoinHandle<()>>,
    /// Sim-feed thread handle.
    sim: Option<std::thread::JoinHandle<()>>,
    /// Acceptor thread handle.
    acceptor: Option<std::thread::JoinHandle<()>>,
}

impl Venue {
    /// Build and start a venue: replay WAL if present, seed state, spawn
    /// workers, clock, optional sim feed, and the network acceptor.
    pub fn start(mut cfg: VenueConfig) -> Result<Venue, String> {
        cfg.validate()?;
        if cfg.loadtest {
            // Benchmark-only tuning: lift engine admission ceilings and the
            // per-key REST budget (production venues keep these tight).
            cfg.shard.risk.rate_per_sec = 4_000_000;
            cfg.shard.max_open_orders = 1_000_000;
        }
        if cfg.loadtest {
            cfg.ws_msg_per_10s = 100_000_000;
            cfg.ws_outbox_depth = 65_536;
        }
        let keyring = if cfg.loadtest {
            Keyring::new(5_000, 4_000_000, 8_000_000)
        } else {
            Keyring::new(5_000, 100, 200)
        };
        let now0 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let n = cfg.underlyings.len();
        let queues: Vec<std::sync::Arc<VenueQueue>> =
            (0..n).map(|_| std::sync::Arc::new(VenueQueue::new())).collect();
        let ctx = Arc::new(VenueCtx {
            cfg: cfg.clone(),
            keyring,
            replica: Replica::new(),
            awaits: Mutex::new(HashMap::new()),
            conns: Mutex::new(HashMap::new()),
            by_account: Mutex::new(HashMap::new()),
            subs: Mutex::new(HashMap::new()),
            queues,
            underlyings: cfg.underlyings.clone(),
            known_accounts: Mutex::new(HashSet::new()),
            now_ns: AtomicU64::new(now0),
            boot: SystemTime::now(),
            metrics: GatewayMetrics::default(),
            draining: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            next_await: AtomicU64::new(0),
            next_conn: AtomicU64::new(0),
        });

        // ---------------- WAL replay ----------------
        let mut shards: Vec<Shard> = (0..n)
            .map(|i| {
                let mut s = Shard::new(i as u32, cfg.underlyings[i], cfg.shard.clone());
                s.clock = now0;
                s
            })
            .collect();
        let mut wal_files = Vec::new();
        if let Some(dir) = &cfg.wal_dir {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            for (i, path) in (0..n).map(|i| dir.join(format!("shard-{i}.wal"))).enumerate() {
                if path.exists() {
                    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                    let mut replayed = 0usize;
                    let crc = crate::journal::Crc32::new();
                    let mut off = 0usize;
                    let mut ev = EvBuf::with_capacity(256);
                    let mut touched: HashSet<AcctId> = HashSet::new();
                    while off < bytes.len() {
                        match crate::journal::WalBatch::parse(&bytes[off..], &crc) {
                            Some((payload, used)) => {
                                // Decode all commands in this batch.
                                let cmds = decode_batch(payload);
                                for c in cmds {
                                    shards[i].journaling = false;
                                    shards[i].execute(c, &mut ev);
                                    shards[i].journaling = true;
                                    replayed += 1;
                                    apply_events(
                                        None,
                                        &ctx.replica,
                                        i as u32,
                                        cfg.underlyings[i],
                                        &ev,
                                        shards[i].clock,
                                        &mut touched,
                                    );
                                    ev.clear();
                                }
                                off += used;
                            }
                            None => {
                                // Torn tail frame (crash mid-write): stop here.
                                break;
                            }
                        }
                    }
                    // Restore accounts touched during replay.
                    ctx.known_accounts
                        .lock()
                        .unwrap()
                        .extend(touched.iter().copied());
                    eprintln!(
                        "venue: replayed {replayed} commands from {}",
                        path.display()
                    );
                }
                wal_files.push(path);
            }
        }

        // ---------------- Attach WAL sinks ----------------
        if cfg.wal_dir.is_some() {
            for (i, path) in wal_files.iter().enumerate() {
                let w = SyncWriter::open(path, 64).map_err(|e| e.to_string())?;
                // Replace the in-memory journal with the file sink, keeping
                // history (append mode).
                shards[i].journal = CmdLog::with_sink(1024 * 1024, Box::new(w));
                shards[i].journaling = true;
            }
        }

        // ---------------- Boot-time seeding (inline, pre-acceptor) ----------------
        // Seed commands are journaled (sinks are attached above), so a
        // restart replays them; after replay they become no-ops (accounts
        // exist, symbols listed). Skip anything the replay already did.
        let mut ev = EvBuf::with_capacity(64);
        for &(account, tier) in &cfg.seed_accounts {
            let already = shards
                .first()
                .map(|s| {
                    (account as usize) < s.accounts.len()
                        && s.accounts[account as usize].id != crate::book::NIL
                })
                .unwrap_or(false);
            if already {
                ctx.known_accounts.lock().unwrap().insert(account);
                continue;
            }
            for s in shards.iter_mut() {
                s.execute(Cmd::CreateAccount { account, tier }, &mut ev);
            }
            ctx.known_accounts.lock().unwrap().insert(account);
            apply_events(None, &ctx.replica, 0, 0, &ev, now0, &mut HashSet::new());
            ev.clear();
        }
        for &(account, underlying, amount) in &cfg.seed_deposits {
            // Skip when the replay already funded this account on this shard
            // (cash non-zero OR account was replayed — deposits are never
            // negative, so replays always leave a trace; a fresh boot with a
            // zero deposit is a no-op anyway).
            if let Some(i) = cfg.underlyings.iter().position(|&u| u == underlying) {
                let replayed = (account as usize) < shards[i].accounts.len()
                    && shards[i].accounts[account as usize].id != crate::book::NIL
                    && shards[i].accounts[account as usize].cash != 0;
                if replayed {
                    continue;
                }
                shards[i].execute(Cmd::Deposit { account, amount }, &mut ev);
                apply_events(
                    None,
                    &ctx.replica,
                    i as u32,
                    underlying,
                    &ev,
                    now0,
                    &mut HashSet::new(),
                );
                ev.clear();
            }
        }
        // Seed keys.
        for k in &cfg.seed_keys {
            ctx.keyring
                .insert(k.key_id, &k.secret_hex, k.account, k.scope)
                .map_err(|e| format!("seed key: {}", e.name()))?;
        }
        // Auto session ladder.
        if let Some(ladder) = &cfg.ladder {
            let now_min = now0 / NANOS_PER_MIN;
            for (i, &mark) in ladder.marks.iter().enumerate() {
                let underlying = cfg.underlyings[i];
                shards[i].execute(Cmd::Oracle { price: mark }, &mut ev);
                apply_events(
                    None,
                    &ctx.replica,
                    i as u32,
                    underlying,
                    &ev,
                    now0,
                    &mut HashSet::new(),
                );
                ev.clear();
                let lo = mark.saturating_mul(10_000 - ladder.span_pct as u64 * 100) / 10_000;
                let hi = mark.saturating_mul(10_000 + ladder.span_pct as u64 * 100) / 10_000;
                let mut strike = lo;
                while strike <= hi {
                    for s in 1..=ladder.sessions {
                        let expiry_min = ((now_min / 60) + s as u64) * 60;
                        for kind in [crate::symbol::OptKind::Call, crate::symbol::OptKind::Put] {
                            let listing = Listing {
                                underlying,
                                expiry_min: expiry_min as u32,
                                kind,
                                strike,
                            };
                            shards[i].execute(Cmd::CreateSymbol { listing }, &mut ev);
                            apply_events(
                                None,
                                &ctx.replica,
                                i as u32,
                                underlying,
                                &ev,
                                now0,
                                &mut HashSet::new(),
                            );
                            ev.clear();
                        }
                    }
                    strike += ladder.strike_step;
                }
            }
        }

        // ---------------- Bootstrap replica balances ----------------
        // Workers snapshot touched accounts per batch, but the boot path
        // (seeding + replay) runs before workers exist — copy engine truth
        // for every known account now.
        {
            let accounts: Vec<AcctId> = ctx.known_accounts.lock().unwrap().iter().copied().collect();
            for (i, shard) in shards.iter_mut().enumerate() {
                let underlying = cfg.underlyings[i];
                for &account in &accounts {
                    let idx = account as usize;
                    if idx >= shard.accounts.len()
                        || shard.accounts[idx].id == crate::book::NIL
                    {
                        continue;
                    }
                    let a = &shard.accounts[idx];
                    let balance = BalanceRow {
                        cash: a.cash,
                        hold: a.hold,
                        margin_required: a.margin_required,
                        margin_reserved: a.margin_reserved,
                        fees_paid: a.fees_paid,
                        rebates_recv: a.rebates_recv,
                    };
                    let positions = a
                        .pos
                        .iter()
                        .filter_map(|&(local_sym, slot)| {
                            let p = shard.positions.get(slot as usize).copied()?;
                            if p.qty == 0 {
                                return None;
                            }
                            Some(PositionRow {
                                symbol: global_symbol(i as u32, local_sym),
                                qty: p.qty,
                                margin: p.margin,
                                delta_notional: p.delta_notional,
                            })
                        })
                        .collect();
                    ctx.replica.snapshot_account(
                        account,
                        underlying,
                        i as u32,
                        balance,
                        positions,
                    );
                }
            }
        }

        // ---------------- Spawn workers ----------------
        let mut workers = Vec::new();
        for (i, queue) in ctx.queues.iter().enumerate() {
            let queue = queue.clone();
            let ctx_w = ctx.clone();
            let state = WorkerState {
                shard_idx: i as u32,
                underlying: cfg.underlyings[i],
            };
            let shard = std::mem::replace(
                &mut shards[i],
                Shard::new(i as u32, cfg.underlyings[i], cfg.shard.clone()),
            );
            let h = std::thread::Builder::new()
                .name(format!("ns-shard-{i}"))
                .spawn(move || shard_worker(shard, queue, ctx_w, state))
                .map_err(|e| e.to_string())?;
            workers.push(h);
        }

        // ---------------- Clock thread ----------------
        let ctx_c = ctx.clone();
        let clock_tick = cfg.clock_ms;
        let clock = std::thread::Builder::new()
            .name("ns-clock".into())
            .spawn(move || {
                let mut last = 0u64;
                loop {
                    std::thread::sleep(Duration::from_millis(clock_tick));
                    if ctx_c.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    if now <= last {
                        continue;
                    }
                    last = now;
                    ctx_c.now_ns.store(now, Ordering::Release);
                    for q in &ctx_c.queues {
                        q.push((Cmd::Advance { to: now }, 0));
                    }
                }
            })
            .map_err(|e| e.to_string())?;

        // ---------------- Sim feed (optional) ----------------
        let sim = if cfg.sim_feed {
            let ctx_s = ctx.clone();
            let mut rng = crate::sim::Rng::new(now0 ^ 0x5EED_0000_0000_0001);
            let marks: Vec<Price> = cfg
                .ladder
                .as_ref()
                .map(|l| l.marks.clone())
                .unwrap_or_else(|| vec![500_000; n]);
            Some(
                std::thread::Builder::new()
                    .name("ns-simfeed".into())
                    .spawn(move || {
                        let mut marks = marks;
                        loop {
                            std::thread::sleep(Duration::from_millis(100));
                            if ctx_s.stopping.load(Ordering::Acquire) {
                                break;
                            }
                            for (i, m) in marks.iter_mut().enumerate() {
                                let bump = rng.range(0, 16) as i64 - 8;
                                let s = *m as i64 + *m as i64 * bump / 100_000;
                                *m = s.max(10_000) as Price;
                                let _ = ctx_s.enqueue_underlying(
                                    Cmd::Oracle { price: *m },
                                    ctx_s.underlyings[i],
                                    0,
                                );
                            }
                        }
                    })
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };

        // ---------------- Acceptor ----------------
        let addr = cfg.to_addr()?;
        let listener = std::net::TcpListener::bind(addr).map_err(|e| e.to_string())?;
        // Blocking accept: instantaneous connection setup (a polling
        // acceptor would add its poll interval to every new connection).
        // Drain wakes it with a loopback self-connect.
        let ctx_a = ctx.clone();
        let max_conns = cfg.max_conns;
        let per_ip = cfg.per_ip_conns;
        let bind_for_wake = cfg.bind.clone();
        let acceptor = std::thread::Builder::new()
            .name("ns-accept".into())
            .spawn(move || {
                let ip_counts: Arc<Mutex<HashMap<std::net::IpAddr, u16>>> =
                    Arc::new(Mutex::new(HashMap::new()));
                loop {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            if ctx_a.stopping.load(Ordering::Acquire) {
                                break;
                            }
                            if ctx_a.metrics.conns.load(Ordering::Relaxed) >= max_conns {
                                drop(stream);
                                continue;
                            }
                            let ip = peer.ip();
                            {
                                let mut g = ip_counts.lock().unwrap();
                                let c = g.entry(ip).or_insert(0);
                                if *c >= per_ip {
                                    drop(stream);
                                    continue;
                                }
                                *c += 1;
                            }
                            let ctx_n = ctx_a.clone();
                            let ip_c = ip_counts.clone();
                            std::thread::Builder::new()
                                .name("ns-conn".into())
                                .spawn(move || {
                                    crate::gateway::serve_conn(ctx_n, stream, ip, ip_c);
                                })
                                .ok();
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        // Wake the acceptor when draining (self-connect unblocks accept()).
        {
            let ctx_w = ctx.clone();
            let mut last_stopping = false;
            std::thread::Builder::new()
                .name("ns-acceptwake".into())
                .spawn(move || loop {
                    std::thread::sleep(Duration::from_millis(20));
                    let s = ctx_w.stopping.load(Ordering::Acquire);
                    if s && !last_stopping {
                        // Best-effort wake; the acceptor exits on the next
                        // accept either way.
                        let addr = bind_for_wake.to_socket_addrs();
                        if let Ok(mut it) = addr {
                            if let Some(a) = it.next() {
                                let _ = std::net::TcpStream::connect_timeout(
                                    &a,
                                    Duration::from_millis(100),
                                );
                            }
                        }
                        return;
                    }
                    if !ctx_w.stopping.load(Ordering::Acquire) {
                        // keep watching until stop
                    }
                    last_stopping = s;
                    if s {
                        return;
                    }
                })
                .map_err(|e| e.to_string())?;
        }

        // ---------------- Auto-drain (tests/demos) ----------------
        if let Some(after) = cfg.drain_after {
            let ctx_d = ctx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(after);
                ctx_d.draining.store(true, Ordering::Release);
                ctx_d.stopping.store(true, Ordering::Release);
            });
        }

        let _ = &wal_files;
        Ok(Venue {
            ctx,
            workers,
            clock: Some(clock),
            sim,
            acceptor: Some(acceptor),
        })
    }

    /// Graceful drain: stop accepting, answer 503 to new requests, drain
    /// all queues, flush journals, join everything.
    pub fn drain(&mut self) {
        self.ctx.draining.store(true, Ordering::Release);
        self.ctx.stopping.store(true, Ordering::Release);
        for q in &self.ctx.queues {
            q.close();
        }
        // Wake the clock so workers observe stopping.
        if let Some(h) = self.clock.take() {
            let _ = h.join();
        }
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
        if let Some(h) = self.sim.take() {
            let _ = h.join();
        }
        if let Some(h) = self.acceptor.take() {
            let _ = h.join();
        }
        // Close remaining connections.
        let conns: Vec<u64> = self.ctx.conns.lock().unwrap().keys().copied().collect();
        for c in conns {
            self.ctx.kill_conn(c);
        }
    }

    /// The bind address (as resolved).
    pub fn local_addr(&self) -> String {
        self.ctx.cfg.bind.clone()
    }

    /// Number of shards.
    pub fn nshards(&self) -> usize {
        self.ctx.queues.len()
    }
}

/// Decode every command in a WAL batch payload.
fn decode_batch(payload: &[u8]) -> Vec<Cmd> {
    let mut out = Vec::new();
    let mut rest = payload;
    while !rest.is_empty() {
        match crate::engine::decode_cmd(rest) {
            Some(cmd) => {
                out.push(cmd);
                let len = record_len(rest);
                if len == 0 {
                    break;
                }
                rest = &rest[len..];
            }
            None => break,
        }
    }
    out
}

/// Record length per journal tag (mirrors the writer layout).
fn record_len(raw: &[u8]) -> usize {
    match raw.first().copied().unwrap_or(0) {
        1 => 1 + 4 + 4 + 4 + 8 + 4 + 8,   // Place
        2 => 1 + 8 + 4 + 4,                // Cancel
        3 => 1 + 8 + 4 + 8 + 4 + 4,        // Replace
        4 => 1 + 4 + 1,                    // CreateAccount
        5 | 6 => 1 + 4 + 16,               // Deposit / Withdraw
        7 => 1 + 8,                        // CreateSymbol
        8 => 1 + 8,                        // Oracle
        9 => 1 + 8,                        // Advance
        10 | 11 => 1,                       // Halt / ResumeNow
        _ => 0,
    }
}
