//! End-to-end venue tests over real TCP: authentication, order entry,
//! market data, admin operations, WebSocket sessions, and the full battery
//! of abuse/attack scenarios from the threat model.
//!
//! These tests boot complete `Venue` instances (engine shards + workers +
//! gateway threads) on ephemeral ports and drive them with a reference
//! test client implementing the exact wire contract in `docs/0DTE_API.md`.

use ns_clob::auth::Scope;
use ns_clob::crypto::{hex, hmac_sha256, sha256};
use ns_clob::fees::Tier;
use ns_clob::venue::{LadderCfg, SeedKey, Venue, VenueConfig};

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ----------------------------------------------------------------------
// Reference test client
// ----------------------------------------------------------------------

static NONCE: AtomicU64 = AtomicU64::new(1);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn sign(
    key: &str,
    secret_hex: &str,
    ts: u64,
    nonce: u64,
    method: &str,
    path: &str,
    body: &[u8],
) -> String {
    let secret = ns_clob::crypto::unhex(secret_hex).expect("hex secret");
    let bh = hex(&sha256(body));
    let canonical = format!("{key}|{ts}|{nonce}|{method}|{path}|{bh}");
    hex(&hmac_sha256(&secret, canonical.as_bytes()))
}

/// One HTTP request/response over a fresh connection.
fn http(
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    auth: Option<(&str, &str)>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_timeout_2s();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n", body.len());
    if let Some((key, secret)) = auth {
        let ts = now_ms();
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let sig = sign(key, secret, ts, nonce, method, path, body.as_bytes());
        req.push_str(&format!(
            "X-Api-Key: {key}\r\nX-Timestamp: {ts}\r\nX-Nonce: {nonce}\r\nX-Signature: {sig}\r\n"
        ));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(body.as_bytes()).unwrap();
    read_response(&mut stream)
}

/// Signed request with explicit ts/nonce/sig override (attack simulations).
#[allow(clippy::too_many_arguments)]
fn http_raw_auth(
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    key: &str,
    _secret: &str,
    ts: u64,
    nonce: u64,
    sig: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream.set_timeout_2s();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\nX-Api-Key: {key}\r\nX-Timestamp: {ts}\r\nX-Nonce: {nonce}\r\nX-Signature: {sig}\r\n\r\n",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(body.as_bytes()).unwrap();
    read_response(&mut stream)
}

trait SetTimeout {
    fn set_timeout_2s(&mut self);
}

impl SetTimeout for TcpStream {
    fn set_timeout_2s(&mut self) {
        self.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        self.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    }
}

fn read_response(stream: &mut TcpStream) -> (u16, String) {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).expect("read head");
        if n == 0 {
            return (0, String::new()); // closed
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = find(&buf, b"\r\n\r\n") {
            break p;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let clen: usize = head
        .to_ascii_lowercase()
        .lines()
        .find(|l| l.starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < clen {
        let n = stream.read(&mut chunk).expect("read body");
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(clen);
    (status, String::from_utf8_lossy(&body).to_string())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ----------------------------------------------------------------------
// WebSocket test client
// ----------------------------------------------------------------------

struct WsClient {
    stream: TcpStream,
}

impl WsClient {
    fn connect(port: u16, key: &str, secret: &str) -> WsClient {
        let ts = now_ms();
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let sig = sign(key, secret, ts, nonce, "GET", "/v1/ws", b"");
        let path = format!("/v1/ws?key={key}&ts={ts}&nonce={nonce}&sig={sig}");
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_timeout_2s();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: t\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).unwrap();
        // Read the 101 handshake.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let n = stream.read(&mut chunk).expect("handshake read");
            buf.extend_from_slice(&chunk[..n]);
            if find(&buf, b"\r\n\r\n").is_some() {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf);
        assert!(head.starts_with("HTTP/1.1 101"), "expected 101, got: {head}");
        assert!(head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        WsClient { stream }
    }

    fn send_text(&mut self, text: &str) {
        let frame = client_text_frame(text.as_bytes());
        self.stream.write_all(&frame).unwrap();
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
    }

    /// Read one server text frame (unmasked).
    fn read_frame(&mut self) -> Option<Vec<u8>> {
        let mut hdr = [0u8; 2];
        let mut got = 0;
        while got < 2 {
            let n = self.stream.read(&mut hdr[got..]).ok()?;
            if n == 0 {
                return None;
            }
            got += n;
        }
        let opcode = hdr[0] & 0x0F;
        let mut len = (hdr[1] & 0x7F) as usize;
        let mut ext = [0u8; 8];
        if len == 126 {
            self.stream.read_exact(&mut ext[..2]).ok()?;
            len = u16::from_be_bytes([ext[0], ext[1]]) as usize;
        } else if len == 127 {
            self.stream.read_exact(&mut ext).ok()?;
            len = u64::from_be_bytes(ext) as usize;
        }
        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload).ok()?;
        if opcode == 0x8 {
            return None; // close
        }
        if opcode == 0x9 {
            return self.read_frame(); // skip ping
        }
        Some(payload)
    }

    /// Read frames until one contains `needle`.
    fn wait_for(&mut self, needle: &str, tries: usize) -> String {
        for _ in 0..tries {
            if let Some(p) = self.read_frame() {
                let s = String::from_utf8_lossy(&p).to_string();
                if s.contains(needle) {
                    return s;
                }
            }
        }
        panic!("did not receive frame containing {needle:?}");
    }
}

fn client_text_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x81);
    if payload.len() < 126 {
        out.push(0x80 | payload.len() as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    let mask = [0x42, 0x37, 0x11, 0xC9];
    out.extend_from_slice(&mask);
    for (i, b) in payload.iter().enumerate() {
        out.push(b ^ mask[i & 3]);
    }
    out
}

// ----------------------------------------------------------------------
// Venue under test
// ----------------------------------------------------------------------

const ADMIN_KEY: &str = "a0000001";
const ADMIN_SECRET: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const TRADER_A: &str = "a0000002";
const SECRET_A: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const TRADER_B: &str = "a0000003";
const SECRET_B: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const READER: &str = "a0000004";
const READER_SECRET: &str = "4444444444444444444444444444444444444444444444444444444444444444";

struct TestVenue {
    #[allow(dead_code)]
    port: u16,
    venue: Venue,
}

#[allow(clippy::field_reassign_with_default)]
fn boot_venue(port: u16, wal: Option<&std::path::Path>) -> TestVenue {
    let mut cfg = VenueConfig::default();
    cfg.bind = format!("127.0.0.1:{port}");
    cfg.wal_dir = wal.map(|p| p.to_path_buf());
    cfg.clock_ms = 2;
    cfg.seed_keys = vec![
        SeedKey {
            key_id: 0xA000_0001,
            secret_hex: ADMIN_SECRET.into(),
            account: 1,
            scope: Scope::Admin,
        },
        SeedKey {
            key_id: 0xA000_0002,
            secret_hex: SECRET_A.into(),
            account: 2,
            scope: Scope::Trade,
        },
        SeedKey {
            key_id: 0xA000_0003,
            secret_hex: SECRET_B.into(),
            account: 3,
            scope: Scope::Trade,
        },
        SeedKey {
            key_id: 0xA000_0004,
            secret_hex: READER_SECRET.into(),
            account: 4,
            scope: Scope::Read,
        },
    ];
    cfg.seed_accounts = vec![(1, Tier::Prime), (2, Tier::Gold), (3, Tier::Gold), (4, Tier::Base)];
    cfg.seed_deposits = vec![
        (1, 0, 1_000_000_000),
        (2, 0, 50_000_000),
        (3, 0, 50_000_000),
        (4, 0, 1_000_000),
    ];
    cfg.ladder = Some(LadderCfg {
        marks: vec![500_000],
        sessions: 2,
        strike_step: 250,
        span_pct: 1,
    });
    let venue = Venue::start(cfg).expect("venue start");
    TestVenue { port, venue }
}

fn free_port() -> u16 {
    // Bind :0, read the assigned port, release.
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn pick_symbol(port: u16) -> (u64, u64) {
    // Returns (symbol, expiry_ms) of the first tradable call.
    let (code, body) = http(port, "GET", "/v1/symbols", "", Some((TRADER_A, SECRET_A)));
    assert_eq!(code, 200);
    // Find the first symbol id and expiry.
    let sym = extract_first_u64(&body, "\"symbol\":");
    let exp = extract_first_u64(&body, "\"expiry_ms\":");
    (sym, exp)
}

fn extract_first_u64(body: &str, key: &str) -> u64 {
    let pos = body.find(key).expect("key in body");
    let rest = &body[pos + key.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().expect("u64")
}

// ----------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------

#[test]
fn venue_public_health_and_time() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    let (c1, b1) = http(port, "GET", "/health", "", None);
    assert_eq!(c1, 200);
    assert!(b1.contains("\"ok\":true"));
    assert!(b1.contains("\"shards\":1"));
    let (c2, b2) = http(port, "GET", "/v1/time", "", None);
    assert_eq!(c2, 200);
    assert!(b2.contains("\"ms\":"));
    tv.venue.drain();
}

#[test]
fn auth_failures_are_specific() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    // Missing auth entirely.
    let (c, _) = http(port, "GET", "/v1/account", "", None);
    assert_eq!(c, 401);
    // Unknown key.
    let ts = now_ms();
    let (c, b) = http_raw_auth(port, "GET", "/v1/account", "", "deadbeef", "00", ts, 1, "00");
    assert_eq!(c, 401);
    assert!(b.contains("UnknownKey"));
    // Bad signature (tampered).
    let ts = now_ms();
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let good = sign(TRADER_A, SECRET_A, ts, nonce, "GET", "/v1/account", b"");
    let bad_sig = good[..63].to_string(); // wrong length
    let (c, b) = http_raw_auth(port, "GET", "/v1/account", "", TRADER_A, SECRET_A, ts, nonce, &bad_sig);
    assert_eq!(c, 401);
    assert!(b.contains("BadSignature"));
    // Tampered body vs signature (signature over different body).
    let ts = now_ms();
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let sig = sign(TRADER_A, SECRET_A, ts, nonce, "POST", "/v1/orders", b"{\"qty\":1}");
    let (c, b) = http_raw_auth(
        port, "POST", "/v1/orders", "{\"qty\":2}", TRADER_A, SECRET_A, ts, nonce, &sig,
    );
    assert_eq!(c, 401);
    assert!(b.contains("BadSignature"));
    // Stale timestamp.
    let old = now_ms() - 60_000;
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let sig = sign(TRADER_A, SECRET_A, old, nonce, "GET", "/v1/account", b"");
    let (c, b) = http_raw_auth(port, "GET", "/v1/account", "", TRADER_A, SECRET_A, old, nonce, &sig);
    assert_eq!(c, 401);
    assert!(b.contains("StaleTimestamp"));
    // Replay: same nonce twice.
    let ts = now_ms();
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let sig = sign(TRADER_A, SECRET_A, ts, nonce, "GET", "/v1/account", b"");
    let (c1, _) = http_raw_auth(port, "GET", "/v1/account", "", TRADER_A, SECRET_A, ts, nonce, &sig);
    assert_eq!(c1, 200);
    let (c2, b2) = http_raw_auth(port, "GET", "/v1/account", "", TRADER_A, SECRET_A, ts, nonce, &sig);
    assert_eq!(c2, 401);
    assert!(b2.contains("BadNonce"));
    // Scope enforcement: read-only key cannot trade.
    let (c, b) = http(port, "POST", "/v1/orders", "{\"symbol\":1,\"side\":\"buy\",\"qty\":1,\"price\":1,\"tif\":\"gtc\",\"kind\":\"limit\"}", Some((READER, READER_SECRET)));
    assert_eq!(c, 403);
    assert!(b.contains("ScopeDenied"));
    // Read-only key cannot admin.
    let (c, _) = http(port, "GET", "/metrics", "", Some((TRADER_A, SECRET_A)));
    assert_eq!(c, 403);
    tv.venue.drain();
}

#[test]
fn symbols_book_tape_and_account_queries() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    let (sym, _exp) = pick_symbol(port);
    // Global symbol 0 is valid (first symbol of the first shard).
    // Book query.
    let (c, b) = http(port, "GET", &format!("/v1/book?symbol={sym}&depth=5"), "", Some((READER, READER_SECRET)));
    assert_eq!(c, 200);
    assert!(b.contains("\"bids\":[") && b.contains("\"asks\":["));
    // Bad queries.
    let (c, _) = http(port, "GET", "/v1/book", "", Some((READER, READER_SECRET)));
    assert_eq!(c, 400);
    let (c, _) = http(port, "GET", "/v1/book?symbol=999999", "", Some((READER, READER_SECRET)));
    assert_eq!(c, 404);
    // Account query shows the seeded balance.
    let (c, b) = http(port, "GET", "/v1/account", "", Some((TRADER_A, SECRET_A)));
    assert_eq!(c, 200);
    assert!(b.contains("\"account\":2"));
    assert!(b.contains("\"cash\":50000000"));
    // Tape (empty initially).
    let (c, b) = http(port, "GET", &format!("/v1/tape?symbol={sym}"), "", Some((READER, READER_SECRET)));
    assert_eq!(c, 200);
    assert!(b.contains("\"trades\":[]"));
    tv.venue.drain();
}

#[test]
fn order_lifecycle_resting_fill_cancel_replace() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    let (sym, _exp) = pick_symbol(port);

    // A sells 5 @ 50.00.
    let (c, b) = http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"sell\",\"qty\":5,\"price\":5000,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 200, "{b}");
    assert!(b.contains("\"ok\":true"));
    assert!(b.contains("\"leaves\":5"));
    let sell_id = extract_first_u64(&b, "\"order_id\":");
    assert!(sell_id > 0);

    // A's balance shows margin reserved.
    let (_, acct) = http(port, "GET", "/v1/account", "", Some((TRADER_A, SECRET_A)));
    assert!(acct.contains("\"margin_reserved\":"));
    let mr_start = extract_first_u64(&acct, "\"margin_reserved\":");
    assert!(mr_start > 0);

    // B buys 5 @ 50.00 -> full cross.
    let (c, b) = http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":5,\"price\":5000,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
        Some((TRADER_B, SECRET_B)),
    );
    assert_eq!(c, 200, "{b}");
    assert!(b.contains("\"ok\":true"));
    assert!(b.contains("\"fills\":["));
    assert!(b.contains("\"price\":5000"));
    assert!(b.contains("\"qty\":5"));

    // Tape now has the trade; book is empty again.
    let (_, tape) = http(port, "GET", &format!("/v1/tape?symbol={sym}"), "", Some((READER, READER_SECRET)));
    assert!(tape.contains("\"price\":5000"));

    // Positions: A short 5, B long 5.
    let (_, acct_a) = http(port, "GET", "/v1/account", "", Some((TRADER_A, SECRET_A)));
    assert!(acct_a.contains("\"qty\":-5"));
    let (_, acct_b) = http(port, "GET", "/v1/account", "", Some((TRADER_B, SECRET_B)));
    assert!(acct_b.contains("\"qty\":5"));

    // A rests another order, then replaces it.
    let (_, b) = http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"sell\",\"qty\":3,\"price\":5900,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
        Some((TRADER_A, SECRET_A)),
    );
    let id2 = extract_first_u64(&b, "\"order_id\":");
    // Note: after the trade at 5000 the option mark is 5000 and the price
    // band is 4000..=6000; the replacement must stay in band.
    let (c, b) = http(
        port, "POST", "/v1/orders/replace",
        &format!("{{\"order\":{id2},\"symbol\":{sym},\"price\":5500,\"qty\":4}}"),
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 200, "{b}");
    assert!(b.contains("\"canceled\":true"));

    // B tries to cancel A's new order -> NotOwner.
    let new_id = extract_first_u64(&b, "\"order_id\":");
    let (c, b) = http(
        port, "DELETE", &format!("/v1/orders/{new_id}?symbol={sym}"), "",
        Some((TRADER_B, SECRET_B)),
    );
    assert_eq!(c, 403, "{b}");
    assert!(b.contains("NotOwner"));

    // A cancels their own.
    let (c, b) = http(
        port, "DELETE", &format!("/v1/orders/{new_id}?symbol={sym}"), "",
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 200, "{b}");
    assert!(b.contains("\"ok\":true"));

    // Cancel a nonexistent order -> 409.
    let (c, b) = http(
        port, "DELETE", &format!("/v1/orders/999999?symbol={sym}"), "",
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 409, "{b}");
    assert!(b.contains("UnknownOrder"));

    // Conservation still exact.
    let (_, audit) = http(port, "GET", "/admin/audit", "", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert!(audit.contains("\"conservation\":true"));
    tv.venue.drain();
}

#[test]
fn batch_orders_and_validation() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    let (sym, _) = pick_symbol(port);
    // Batch of two resting orders.
    let body = format!(
        "{{\"orders\":[{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":1,\"price\":100,\"tif\":\"gtc\",\"kind\":\"limit\"}},{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":2,\"price\":90,\"tif\":\"gtc\",\"kind\":\"limit\"}}]}}"
    );
    let (c, b) = http(port, "POST", "/v1/orders/batch", &body, Some((TRADER_A, SECRET_A)));
    assert_eq!(c, 200, "{b}");
    assert!(b.contains("\"results\":["));
    assert_eq!(b.matches("\"ok\":true").count(), 2);

    // Malformed: float price.
    let (c, b) = http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":1,\"price\":10.5,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 400, "{b}");
    // Malformed: unknown field.
    let (c, b) = http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":1,\"price\":10,\"tif\":\"gtc\",\"kind\":\"limit\",\"account\":3}}"),
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 400, "{b}");
    assert!(b.contains("unknown field account"), "{b}");
    // Malformed: duplicate keys.
    let (c, _) = http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"symbol\":{sym},\"side\":\"buy\",\"qty\":1,\"price\":10,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 400);
    // qty 0.
    let (c, _) = http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":0,\"price\":10,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 400);
    // qty over the cap (10_000).
    let (c, b) = http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":10001,\"price\":10,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 400, "{b}");
    // market without ioc.
    let (c, _) = http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":1,\"price\":10,\"tif\":\"gtc\",\"kind\":\"market\"}}"),
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 400);
    // unknown symbol.
    let (c, _) = http(
        port, "POST", "/v1/orders",
        "{\"symbol\":999999,\"side\":\"buy\",\"qty\":1,\"price\":10,\"tif\":\"gtc\",\"kind\":\"limit\"}",
        Some((TRADER_A, SECRET_A)),
    );
    assert_eq!(c, 400);
    // Broken JSON.
    let (c, _) = http(port, "POST", "/v1/orders", "{\"qty\":", Some((TRADER_A, SECRET_A)));
    assert_eq!(c, 400);
    tv.venue.drain();
}

#[test]
fn admin_operations_and_key_minting() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    // Create account 7.
    let (c, b) = http(port, "POST", "/admin/accounts", "{\"account\":7,\"tier\":\"silver\"}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 200, "{b}");
    // Duplicate -> 409.
    let (c, _) = http(port, "POST", "/admin/accounts", "{\"account\":7,\"tier\":\"silver\"}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 409);
    // Deposit.
    let (c, b) = http(port, "POST", "/admin/deposit", "{\"account\":7,\"underlying\":0,\"amount\":1000000}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 200, "{b}");
    // Negative amount rejected.
    let (c, _) = http(port, "POST", "/admin/deposit", "{\"account\":7,\"underlying\":0,\"amount\":-5}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 400);
    // Withdraw more than available -> declined.
    let (c, b) = http(port, "POST", "/admin/withdraw", "{\"account\":7,\"underlying\":0,\"amount\":99999999}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 409, "{b}");
    // Unknown account deposit -> 404.
    let (c, _) = http(port, "POST", "/admin/deposit", "{\"account\":99,\"underlying\":0,\"amount\":1}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 404);
    // Mint a key for account 7.
    let (c, b) = http(port, "POST", "/admin/keys", "{\"account\":7,\"scope\":\"trade\"}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 200, "{b}");
    let key = extract_str(&b, "\"key_id\":\"");
    let secret = extract_str(&b, "\"secret\":\"");
    assert_eq!(key.len(), 8);
    assert_eq!(secret.len(), 64);
    // The minted key works immediately.
    let (c, _) = http(port, "GET", "/v1/account", "", Some((key.as_str(), secret.as_str())));
    assert_eq!(c, 200);
    // Halt + resume.
    let (c, _) = http(port, "POST", "/admin/halt", "{\"underlying\":0}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 200);
    let (_, symbols) = http(port, "GET", "/v1/symbols", "", Some((ADMIN_KEY, ADMIN_SECRET)));
    let (_, ul) = http(port, "GET", "/v1/symbols", "", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert!(ul.contains("\"halted\":true") || symbols.contains("\"halted\":true"), "{ul}");
    let (c, _) = http(port, "POST", "/admin/resume", "{\"underlying\":0}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 200);
    // Oracle set.
    let (c, _) = http(port, "POST", "/admin/oracle", "{\"underlying\":0,\"price\":501000}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 200);
    // Metrics + audit.
    let (c, m) = http(port, "GET", "/metrics", "", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 200);
    assert!(m.contains("\"gateway\""));
    assert!(m.contains("\"conns\":"));
    let (c, a) = http(port, "GET", "/admin/audit", "", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 200);
    assert!(a.contains("\"conservation\":true"));
    tv.venue.drain();
}

fn extract_str(body: &str, prefix: &str) -> String {
    let pos = body.find(prefix).expect("prefix");
    let rest = &body[pos + prefix.len()..];
    let end = rest.find('"').unwrap();
    rest[..end].to_string()
}

#[test]
fn websocket_session_full_flow() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    let (sym, _) = pick_symbol(port);

    // Auth failure on upgrade first.
    {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_timeout_2s();
        let req = "GET /v1/ws?key=a0000002&ts=1&nonce=1&sig=00 HTTP/1.1\r\nHost: t\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        stream.write_all(req.as_bytes()).unwrap();
        let (code, body) = read_response(&mut stream);
        assert_eq!(code, 401, "{body}");
    }

    // Connect as trader A (query auth).
    let mut ws = WsClient::connect(port, TRADER_A, SECRET_A);
    let hello = ws.wait_for("\"t\":\"hello\"", 20);
    assert!(hello.contains("\"account\":2"));

    // Subscribe to bbo + trade for the symbol.
    ws.send_text(&format!("{{\"op\":\"sub\",\"channel\":\"bbo\",\"symbol\":{sym}}}"));
    ws.send_text(&format!("{{\"op\":\"sub\",\"channel\":\"trade\",\"symbol\":{sym}}}"));
    // Unknown symbol sub -> error.
    ws.send_text("{\"op\":\"sub\",\"channel\":\"bbo\",\"symbol\":999999}");
    let err = ws.wait_for("\"t\":\"error\"", 20);
    assert!(err.contains("unknown symbol"));

    // Rest a sell via WS order entry.
    ws.send_text(&format!("{{\"op\":\"order\",\"symbol\":{sym},\"side\":\"sell\",\"qty\":4,\"price\":6000,\"tif\":\"gtc\",\"kind\":\"limit\"}}"));
    let ack = ws.wait_for("\"t\":\"ack\"", 20);
    assert!(ack.contains("\"leaves\":4"));
    let order_id = extract_first_u64(&ack, "\"order\":");

    // Trader B (separate WS) buys 2 -> partial fill; both get fill frames.
    let mut ws_b = WsClient::connect(port, TRADER_B, SECRET_B);
    let _ = ws_b.wait_for("\"t\":\"hello\"", 20);
    ws_b.send_text(&format!("{{\"op\":\"order\",\"symbol\":{sym},\"side\":\"buy\",\"qty\":2,\"price\":6000,\"tif\":\"gtc\",\"kind\":\"limit\"}}"));
    let fill_b = ws_b.wait_for("\"t\":\"fill\"", 20);
    assert!(fill_b.contains("\"role\":\"taker"));
    assert!(fill_b.contains("\"price\":6000"));
    // For A the public trade frame is pushed before the maker fill; read
    // in arrival order (frames queue in the outbox channel).
    let tape = ws.wait_for("\"t\":\"trade\"", 20);
    assert!(tape.contains("\"price\":6000"));

    let fill_a = ws.wait_for("\"t\":\"fill\"", 20);
    assert!(fill_a.contains("\"role\":\"maker\""));

    // Balance frames flow to A after the fills (private, unsolicited).
    let bal = ws.wait_for("\"t\":\"balance\"", 30);
    assert!(bal.contains("\"underlying\":0"));

    // A cancels the remainder via WS.
    ws.send_text(&format!("{{\"op\":\"cancel\",\"order\":{order_id},\"symbol\":{sym}}}"));
    let canceled = ws.wait_for("\"t\":\"canceled\"", 20);
    assert!(canceled.contains("\"reason\":\"user\""));

    // B tries to cancel A's (already gone) order -> reject.
    ws_b.send_text(&format!("{{\"op\":\"cancel\",\"order\":{order_id},\"symbol\":{sym}}}"));
    let rej = ws_b.wait_for("\"t\":\"reject\"", 20);
    assert!(rej.contains("UnknownOrder"));

    // Ping/pong.
    ws.send_text("{\"op\":\"ping\"}");
    let pong = ws.wait_for("\"t\":\"pong\"", 20);
    assert!(pong.contains("pong"));

    // Malformed JSON message -> error frame (not a disconnect).
    ws.send_text("{not json");
    let err = ws.wait_for("\"t\":\"error\"", 20);
    assert!(err.contains("malformed"));

    tv.venue.drain();
}

#[test]
fn http_protocol_abuse() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    // Oversized body -> 413.
    let big = "x".repeat(64 * 1024 + 1);
    let (c, b) = http(port, "POST", "/v1/orders", &big, Some((TRADER_A, SECRET_A)));
    // The parser rejects at header size or content-length stage.
    assert!(c == 413 || c == 400, "c={c} b={b}");
    // CORS preflight.
    let (c, _) = http(port, "OPTIONS", "/v1/orders", "", None);
    assert_eq!(c, 200);
    // Unknown route.
    let (c, _) = http(port, "GET", "/nope", "", Some((TRADER_A, SECRET_A)));
    assert_eq!(c, 404);
    // Wrong method on a known route.
    let (c, b) = http(port, "GET", "/v1/orders", "", Some((TRADER_A, SECRET_A)));
    assert_eq!(c, 405, "{b}");
    // Slowloris: partial header, hold past the deadline.
    {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        stream.write_all(b"GET /health HTTP/1.1\r\nHost: t\r\n").unwrap();
        let start = std::time::Instant::now();
        let mut chunk = [0u8; 64];
        let n = stream.read(&mut chunk).unwrap_or(0);
        let elapsed = start.elapsed();
        assert_eq!(n, 0, "connection should be closed");
        assert!(
            elapsed < Duration::from_secs(14),
            "slowloris timeout too slow: {elapsed:?}"
        );
    }
    // Request smuggling shapes are refused.
    {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_timeout_2s();
        let req = "POST /v1/orders HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\nhello";
        stream.write_all(req.as_bytes()).unwrap();
        let (c, _) = read_response(&mut stream);
        assert_eq!(c, 400);
    }
    // Garbage request line.
    {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_timeout_2s();
        stream.write_all(b"\x16\x03\x01\x00\x00not-http\r\n\r\n").unwrap();
        let (c, _) = read_response(&mut stream);
        assert!(c == 400 || c == 0);
    }
    tv.venue.drain();
}

#[test]
fn ws_protocol_abuse() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    // Unmasked client frame -> protocol violation -> close.
    {
        let mut ws = WsClient::connect(port, TRADER_A, SECRET_A);
        let _ = ws.wait_for("\"t\":\"hello\"", 20);
        ws.send_raw(&[0x81, 0x02, b'h', b'i']); // unmasked
        assert!(ws.read_frame().is_none(), "expected close");
    }
    // Oversized frame -> 1009 close.
    {
        let mut ws = WsClient::connect(port, TRADER_A, SECRET_A);
        let _ = ws.wait_for("\"t\":\"hello\"", 20);
        // Announce a 1 MiB payload.
        let mut frame = vec![0x82, 0x80 | 127];
        frame.extend_from_slice(&1_048_576u64.to_be_bytes());
        frame.extend_from_slice(&[0u8; 4]); // mask
        ws.send_raw(&frame);
        assert!(ws.read_frame().is_none(), "expected close");
    }
    // Reserved bits -> close.
    {
        let mut ws = WsClient::connect(port, TRADER_A, SECRET_A);
        let _ = ws.wait_for("\"t\":\"hello\"", 20);
        let mut frame = client_text_frame(b"{}");
        frame[0] |= 0x40; // RSV1
        ws.send_raw(&frame);
        assert!(ws.read_frame().is_none());
    }
    // Subscription flood -> capped at 64.
    {
        let mut ws = WsClient::connect(port, TRADER_A, SECRET_A);
        let _ = ws.wait_for("\"t\":\"hello\"", 20);
        let (_, body) = http(port, "GET", "/v1/symbols", "", Some((READER, READER_SECRET)));
        // Subscribe to 70 distinct symbols.
        let mut syms = Vec::new();
        let mut rest = body.as_str();
        while let Some(p) = rest.find("\"symbol\":") {
            let after = &rest[p + 9..];
            let end = after.find(',').unwrap_or(after.len());
            let s: u64 = after[..end].parse().unwrap();
            syms.push(s);
            rest = after;
        }
        syms.dedup();
        for &s in syms.iter().take(70) {
            ws.send_text(&format!("{{\"op\":\"sub\",\"channel\":\"bbo\",\"symbol\":{s}}}"));
        }
        let err = ws.wait_for("subscription limit", 1200);
        assert!(err.contains("\"t\":\"error\""));
    }
    tv.venue.drain();
}

#[test]
fn rest_rate_limit_engages() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    let mut got_429 = false;
    // Keyring default: 100/s sustained, burst 200. Hammer 250 fast requests.
    for i in 0..800 {
        let (c, _b) = http(port, "GET", "/v1/time", "", None);
        if i == 0 {
            assert_eq!(c, 200);
        }
        let (c, _) = http(port, "GET", "/v1/account", "", Some((READER, READER_SECRET)));
        if c == 429 {
            got_429 = true;
            break;
        }
    }
    assert!(got_429, "rate limit must engage under a burst");
    tv.venue.drain();
}

#[test]
fn wal_recovery_restores_state() {
    let dir = std::env::temp_dir().join(format!("ns-venue-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let port = free_port();
    {
        let mut tv = boot_venue(port, Some(&dir));
        let (sym, _) = pick_symbol(port);
        // Rest an order as A.
        let (_, b) = http(
            port, "POST", "/v1/orders",
            &format!("{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":3,\"price\":50,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
            Some((TRADER_A, SECRET_A)),
        );
        assert!(b.contains("\"ok\":true"));
        // A trade too.
        http(
            port, "POST", "/v1/orders",
            &format!("{{\"symbol\":{sym},\"side\":\"sell\",\"qty\":2,\"price\":40,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
            Some((TRADER_B, SECRET_B)),
        );
        http(
            port, "POST", "/v1/orders",
            &format!("{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":2,\"price\":40,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
            Some((TRADER_A, SECRET_A)),
        );
        // Let the worker's touched-account snapshot land (µs-scale
        // eventual consistency between ack and balance replica refresh).
        std::thread::sleep(Duration::from_millis(150));
        let (_, before) = http(port, "GET", "/v1/account", "", Some((TRADER_A, SECRET_A)));
        // Drain (flushes WAL).
        tv.venue.drain();
        // Restart on the same WAL dir.
        let port2 = free_port();
        let mut tv2 = boot_venue(port2, Some(&dir));
        // Symbols survived (replay).
        let (sym2, _) = pick_symbol(port2);
        assert_eq!(sym, sym2);
        // A's state matches: same cash and the same resting orders.
        let (_, after) = http(port2, "GET", "/v1/account", "", Some((TRADER_A, SECRET_A)));
        let cash_before = extract_first_u64(&before, "\"cash\":");
        let cash_after = extract_first_u64(&after, "\"cash\":");
        assert_eq!(cash_before, cash_after, "cash must survive recovery");
        // The live replica itself must reflect the maker fill (leaves 1) —
        // guards the Trade event's maker order id end to end.
        assert!(before.contains("\"leaves\":1"), "live replica must show maker-filled leaves 1: {before}");
        // The trade in this scenario: B's crossing sell filled 2 as taker
        // against A's resting 3@50 bid (maker) -> A's bid shows leaves 1,
        // earned maker rebates, and A's follow-up 2@40 bid rests untouched.
        // Order rows must survive recovery with the same leaves the live
        // replica shows (the Trade event's maker id packs the true slot).
        assert!(after.contains("\"leaves\":1"), "maker-filled order must show leaves 1: {after}");
        assert!(after.contains("\"leaves\":2"), "resting follow-up order must survive: {after}");
        assert!(after.contains("\"qty\":2"), "position must survive: {after}");
        // Conservation holds after replay.
        let (_, audit) = http(port2, "GET", "/admin/audit", "", Some((ADMIN_KEY, ADMIN_SECRET)));
        assert!(audit.contains("\"conservation\":true"), "{audit}");
        // The recovered venue still accepts orders (price within the
        // recovered mark's band: mark 40 -> 32..=48).
        let (c, b) = http(
            port2, "POST", "/v1/orders",
            &format!("{{\"symbol\":{sym2},\"side\":\"buy\",\"qty\":1,\"price\":45,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
            Some((TRADER_A, SECRET_A)),
        );
        assert_eq!(c, 200);
        assert!(b.contains("\"ok\":true"));
        tv2.venue.drain();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn account_cannot_be_spoofed_via_orders() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    let (sym, _) = pick_symbol(port);
    // The order schema has no account field; sending one is rejected.
    let (c, b) = http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":1,\"price\":10,\"tif\":\"gtc\",\"kind\":\"limit\",\"account\":1}}"),
        Some((TRADER_B, SECRET_B)),
    );
    assert_eq!(c, 400, "{b}");
    assert!(b.contains("unknown field account"));
    // Trades executed by B credit B, not A.
    http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"sell\",\"qty\":1,\"price\":20,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
        Some((TRADER_A, SECRET_A)),
    );
    http(
        port, "POST", "/v1/orders",
        &format!("{{\"symbol\":{sym},\"side\":\"buy\",\"qty\":1,\"price\":20,\"tif\":\"gtc\",\"kind\":\"limit\"}}"),
        Some((TRADER_B, SECRET_B)),
    );
    let (_, acct_b) = http(port, "GET", "/v1/account", "", Some((TRADER_B, SECRET_B)));
    // B paid premium 20 + fee; cash < 50_000_000.
    let cash_b = extract_first_u64(&acct_b, "\"cash\":");
    assert!(cash_b < 50_000_000);
    tv.venue.drain();
}

#[test]
fn admin_shutdown_endpoint_drains() {
    let port = free_port();
    let mut tv = boot_venue(port, None);
    let (c, b) = http(port, "POST", "/admin/shutdown", "{}", Some((ADMIN_KEY, ADMIN_SECRET)));
    assert_eq!(c, 200, "{b}");
    // Non-admin cannot shut down the venue.
    {
        let port2 = free_port();
        let mut tv2 = boot_venue(port2, None);
        let (c, _) = http(port2, "POST", "/admin/shutdown", "{}", Some((TRADER_A, SECRET_A)));
        assert_eq!(c, 403);
        tv2.venue.drain();
    }
    // Wait for drain to complete.
    for _ in 0..100 {
        if tv.venue.ctx.stopping.load(Ordering::Acquire) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    tv.venue.drain();
}

/// Allow `use std::collections::atomic::Ordering as _;` style import hack.
mod shim {}
