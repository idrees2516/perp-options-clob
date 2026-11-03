//! `venue-tps` — gateway-fed throughput benchmark.
//!
//! Measures real end-to-end ingress through the full production path
//! (HTTP parse → HMAC verify → JSON parse → validate → queue → engine →
//! ack → response), unlike `ns-tps` which measures pure engine capacity.
//!
//! Modes:
//! * `rest` — N connections doing signed sequential REST order placements
//!   (request/response round trips).
//! * `ws`   — N WebSocket connections streaming order-entry ops and
//!   draining the private ack stream (the low-latency ingress path).
//!
//! Run: `cargo run --release --bin venue-tps -- --mode ws --conns 8 --secs 10`

use ns_clob::auth::key_id_hex;
use ns_clob::crypto::{hex, hmac_sha256, sha256};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Time-based nonce base (like real clients): survives process restarts
// against a long-running venue.
static NONCE: AtomicU64 = AtomicU64::new(0);
static ERRS: AtomicU64 = AtomicU64::new(0);
thread_local! {
    static FIRST_ERR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}
static FIRST_ERRS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn next_nonce() -> u64 {
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        * 1_000;
    NONCE.fetch_max(base, Ordering::Relaxed);
    NONCE.fetch_add(1, Ordering::Relaxed) + 1
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn sign(key: &str, secret: &[u8], ts: u64, nonce: u64, method: &str, path: &str, body: &[u8]) -> String {
    let bh = hex(&sha256(body));
    let canonical = format!("{key}|{ts}|{nonce}|{method}|{path}|{bh}");
    hex(&hmac_sha256(secret, canonical.as_bytes()))
}

struct Args {
    port: u16,
    mode: String,
    conns: usize,
    secs: u64,
}

fn main() {
    let mut args = Args {
        port: 8080,
        mode: "ws".into(),
        conns: 8,
        secs: 10,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut v = |n: &str| it.next().unwrap_or_else(|| panic!("{n} needs a value"));
        match a.as_str() {
            "--port" => args.port = v("--port").parse().unwrap(),
            "--mode" => args.mode = v("--mode"),
            "--conns" => args.conns = v("--conns").parse().unwrap(),
            "--secs" => args.secs = v("--secs").parse().unwrap(),
            _ => {
                eprintln!("usage: venue-tps --port P --mode rest|ws --conns N --secs S");
                std::process::exit(2);
            }
        }
    }
    // The harness expects the venue started with --loadtest and a benchmark
    // key: id 00000bb8, secret 0xBB*32, on a funded account (see README).
    let key = "00000bb8";
    let secret: Vec<u8> = vec![0xBBu8; 32];
    let sym = discover_symbol(args.port, key, &secret);
    let counts: Vec<Arc<AtomicU64>> = (0..args.conns)
        .map(|_| Arc::new(AtomicU64::new(0)))
        .collect();
    // +1: main joins the barrier AFTER storing the deadline, so no worker
    // can observe the zero-initialized deadline.
    let barrier = Arc::new(Barrier::new(args.conns + 1));
    let deadline = Arc::new(AtomicU64::new(0));
    let mode = args.mode.clone();
    let port = args.port;

    let mut handles = Vec::new();
    for (i, count) in counts.iter().enumerate() {
        let (count, barrier, deadline) = (count.clone(), barrier.clone(), deadline.clone());
        let secret = secret.clone();
        let mode = mode.clone();
        handles.push(std::thread::spawn(move || {
            match mode.as_str() {
                "rest" => rest_worker(port, key, &secret, sym, &count, &barrier, &deadline, i),
                _ => ws_worker(port, key, &secret, sym, &count, &barrier, &deadline, i),
            }
        }));
    }
    std::thread::sleep(Duration::from_millis(500)); // let conns establish
    let start = Instant::now();
    deadline.store(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + args.secs * 1000,
        Ordering::Release,
    );
    barrier.wait(); // simultaneous start
    for h in handles {
        let _ = h.join();
    }
    let elapsed = start.elapsed().as_secs_f64();
    let total: u64 = counts.iter().map(|c| c.load(Ordering::Relaxed)).sum();
    let errs = ERRS.load(Ordering::Relaxed);
    // Surface one error body per worker for diagnosis.
    if let Ok(all) = FIRST_ERRS.lock() {
        for e in all.iter().take(3) {
            eprintln!("sample error: {e}");
        }
    }
    println!(
        "mode={} conns={} secs={:.1} orders={} errors={} throughput={:.0} orders/s",
        mode,
        args.conns,
        elapsed,
        total,
        errs,
        total as f64 / elapsed
    );
}

fn discover_symbol(port: u16, key: &str, secret: &[u8]) -> u64 {
    let (code, body) = rest_call(port, "GET", "/v1/symbols", b"", key, secret);
    assert_eq!(code, 200, "{body}");
    let pos = body.find("\"symbol\":").expect("symbol");
    let rest = &body[pos + 9..];
    let end = rest.find(',').unwrap_or(rest.len());
    rest[..end].parse().unwrap()
}

fn rest_call(
    port: u16,
    method: &str,
    path: &str,
    body: &[u8],
    key: &str,
    secret: &[u8],
) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_nodelay(true).unwrap();
    let ts = now_ms();
    let nonce = next_nonce();
    let sig = sign(key, secret, ts, nonce, method, path, body);
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: b\r\nContent-Length: {}\r\nX-Api-Key: {key}\r\nX-Timestamp: {ts}\r\nX-Nonce: {nonce}\r\nX-Signature: {sig}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let code: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body_start = text.find("\r\n\r\n").map(|p| p + 4).unwrap_or(text.len());
    (code, text[body_start..].to_string())
}

/// Sustainable equilibrium load: post-only IOC buys priced below any
/// plausible ask. Every order runs the full admission pipeline (auth,
/// validation, rate limit, price band, post-only gate, buying-power
/// commitment + release, event emission, journal write) and leaves the
/// book empty, so the run never saturates open-order caps.
fn order_body(sym: u64, _i: u64, ws: bool) -> String {
    let op = if ws { "\"op\":\"order\"," } else { "" };
    format!(
        "{{{op}\"symbol\":{sym},\"side\":\"buy\",\"qty\":1,\"price\":1,\"tif\":\"ioc\",\"kind\":\"limit\",\"post_only\":true}}"
    )
}

#[allow(clippy::too_many_arguments)]
fn rest_worker(
    port: u16,
    key: &str,
    secret: &[u8],
    sym: u64,
    count: &Arc<AtomicU64>,
    barrier: &Arc<Barrier>,
    deadline: &Arc<AtomicU64>,
    _i: usize,
) {
    barrier.wait();
    let mut i = 0u64;
    loop {
        if now_ms() >= deadline.load(Ordering::Acquire) {
            break;
        }
        let (code, body) = rest_call(port, "POST", "/v1/orders", order_body(sym, i, false).as_bytes(), key, secret);
        if code == 200 {
            count.fetch_add(1, Ordering::Relaxed);
        } else {
            ERRS.fetch_add(1, Ordering::Relaxed);
            FIRST_ERR.with(|c| {
                let mut g = c.borrow_mut();
                if g.is_none() {
                    *g = Some(format!("{code} {body}"));
                    if let Ok(mut all) = FIRST_ERRS.lock() {
                        if all.len() < 3 {
                            all.push(format!("{code} {body}"));
                        }
                    }
                }
            });
            std::thread::sleep(Duration::from_millis(1));
        }
        i += 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn ws_worker(
    port: u16,
    key: &str,
    secret: &[u8],
    sym: u64,
    count: &Arc<AtomicU64>,
    barrier: &Arc<Barrier>,
    deadline: &Arc<AtomicU64>,
    i: usize,
) {
    // WS handshake with query auth.
    let ts = now_ms();
    let nonce = next_nonce();
    let sig = sign(key, secret, ts, nonce, "GET", "/v1/ws", b"");
    let path = format!("/v1/ws?key={key}&ts={ts}&nonce={nonce}&sig={sig}");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_nodelay(true).unwrap();
    // Handshake phase: generous timeout.
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: b\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).unwrap();
    // Read handshake.
    let mut buf = [0u8; 2048];
    let mut acc = Vec::new();
    loop {
        let n = stream.read(&mut buf).expect("hs");
        acc.extend_from_slice(&buf[..n]);
        if acc.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    // Spam phase: short read timeout for opportunistic ack draining.
    stream
        .set_read_timeout(Some(Duration::from_micros(200)))
        .unwrap();
    let mask = [(0x30 + i as u8) & 0x7F, 0x5A, 0xA5, 0x3C];
    barrier.wait();
    let mut i = 0u64;
    let mut out = Vec::with_capacity(256);
    let mut rbuf = [0u8; 8192];
    let mut pending = Vec::new();
    loop {
        if now_ms() >= deadline.load(Ordering::Acquire) {
            break;
        }
        // Send an order op (masked text frame).
        let body = order_body(sym, i, true);
        out.clear();
        out.push(0x81);
        out.push(0x80 | body.len() as u8);
        out.extend_from_slice(&mask);
        for (j, b) in body.as_bytes().iter().enumerate() {
            out.push(b ^ mask[j & 3]);
        }
        if stream.write_all(&out).is_err() {
            break;
        }
        count.fetch_add(1, Ordering::Relaxed);
        i += 1;
        // Drain ack frames opportunistically (short timeout; a periodic
        // full drain prevents the socket buffer from filling).
        if i % 32 == 0 {
            stream
                .set_read_timeout(Some(Duration::from_millis(5)))
                .unwrap();
        } else {
            stream
                .set_read_timeout(Some(Duration::from_micros(200)))
                .unwrap();
        }
        loop {
            match stream.read(&mut rbuf) {
                Ok(0) => break,
                Ok(n) => {
                    pending.extend_from_slice(&rbuf[..n]);
                    let mut off = 0;
                    while off + 2 <= pending.len() {
                        let len = (pending[off + 1] & 0x7F) as usize;
                        let total = 2 + len;
                        if off + total > pending.len() {
                            break;
                        }
                        off += total;
                    }
                    pending.drain(..off);
                }
                Err(_) => break,
            }
            if pending.capacity() > 1 << 20 {
                pending = Vec::new();
            }
        }
    }
}

// Silence dead-code warning for the unused default key material path.
#[allow(dead_code)]
fn _unused() {
    let _ = key_id_hex(0);
}
