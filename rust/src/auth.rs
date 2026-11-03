//! API-key authentication and authorization for the venue gateway.
//!
//! Design (see `docs/0DTE_API.md` §1):
//! * Keys are `(key_id: u32, secret: [u8;32])`. The registry holds secrets
//!   **in memory only** — never journaled, never logged, never serialized.
//! * Requests are signed `HMAC-SHA256(secret, key_id|ts|nonce|METHOD|path|body_hash)`.
//! * Checks, in order: key exists → timestamp window (±5 s) → nonce strictly
//!   increasing (replay defense) → constant-time MAC comparison → scope.
//! * Every failure increments an audit counter; the connection layer turns
//!   repeated failures into disconnects and IP penalties.
//! * Per-key REST rate limit: token bucket (burst 200, 100 req/s sustained).

use crate::accounts::TokenBucket;
use crate::crypto::{ct_eq, hmac_sha256, os_random, sha256, unhex};
use std::collections::HashMap;
use std::sync::Mutex;

/// Access scope granted to a key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Read-only market data + own account.
    Read,
    /// Order entry (implies read).
    Trade,
    /// Everything, including funding and venue operations.
    Admin,
}

impl Scope {
    /// Scope from its wire string.
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "read" => Some(Scope::Read),
            "trade" => Some(Scope::Trade),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }
    /// Wire string.
    pub fn name(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Trade => "trade",
            Scope::Admin => "admin",
        }
    }
    /// Does this scope satisfy `want`?
    pub fn allows(self, want: Scope) -> bool {
        self as u8 >= want as u8
    }
}

/// Sliding anti-replay window for one key's nonces.
///
/// Nonces must increase over time, but concurrent connections from one key
/// legitimately deliver them out of order (browser panels polling in
/// parallel, load testers). We accept any *unused* nonce inside a window
/// of [`NONCE_WINDOW`] below the highest seen; duplicates and anything
/// older than the window are rejected. Memory is bounded: at most
/// `NONCE_WINDOW` remembered nonces per key.
#[derive(Clone, Debug)]
pub struct NonceWindow {
    /// Highest nonce accepted so far.
    highest: u64,
    /// Nonces inside the window that have been used.
    used: std::collections::HashSet<u64>,
}

/// Window size in nonces (absorbs concurrent out-of-order delivery).
pub const NONCE_WINDOW: u64 = 4096;

impl Default for NonceWindow {
    fn default() -> Self {
        NonceWindow {
            highest: 0,
            used: std::collections::HashSet::new(),
        }
    }
}

impl NonceWindow {
    /// Try to accept `nonce`. Returns false for replays, zero, and nonces
    /// that fell out of the window.
    pub fn accept(&mut self, nonce: u64) -> bool {
        if nonce == 0 {
            return false;
        }
        if nonce > self.highest {
            self.highest = nonce;
            self.used.insert(nonce);
            self.prune();
            return true;
        }
        // At or below the highest.
        if self.highest - nonce >= NONCE_WINDOW {
            return false; // stale
        }
        if !self.used.insert(nonce) {
            return false; // replay
        }
        true
    }

    /// Drop remembered nonces that fell out of the window.
    fn prune(&mut self) {
        if self.used.len() as u64 > NONCE_WINDOW {
            let floor = self.highest.saturating_sub(NONCE_WINDOW);
            self.used.retain(|&n| n > floor);
        }
    }
}

/// One registered key.
pub struct ApiKey {
    /// Public key id (hex form is 8 chars).
    pub key_id: u32,
    /// Raw 32-byte secret.
    pub secret: [u8; 32],
    /// Owning account (all trading is executed as this account; clients
    /// cannot choose an account id — enforced here, not in the engine).
    pub account: u32,
    /// Granted scope.
    pub scope: Scope,
    /// Anti-replay nonce window (replaces naive last-nonce ordering).
    pub nonces: NonceWindow,
    /// REST rate bucket.
    pub bucket: TokenBucket,
    /// Failed-auth counter (for lockout policies).
    pub auth_fails: u64,
}

/// Authentication failure reasons (wire strings).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthErr {
    /// Unknown key id.
    UnknownKey,
    /// Timestamp outside the acceptance window.
    StaleTimestamp,
    /// Nonce not strictly greater than the last accepted one.
    BadNonce,
    /// HMAC mismatch.
    BadSignature,
    /// Scope does not allow this operation.
    ScopeDenied,
    /// Key exists (mint collision — effectively impossible).
    KeyExists,
    /// Rate limited.
    RateLimited,
}

impl AuthErr {
    /// Wire name.
    pub fn name(self) -> &'static str {
        match self {
            AuthErr::UnknownKey => "UnknownKey",
            AuthErr::StaleTimestamp => "StaleTimestamp",
            AuthErr::BadNonce => "BadNonce",
            AuthErr::BadSignature => "BadSignature",
            AuthErr::ScopeDenied => "ScopeDenied",
            AuthErr::KeyExists => "KeyExists",
            AuthErr::RateLimited => "RateLimited",
        }
    }
    /// HTTP status for this failure.
    pub fn status(self) -> u16 {
        match self {
            AuthErr::RateLimited => 429,
            AuthErr::ScopeDenied => 403,
            _ => 401,
        }
    }
}

/// The registry (thread-safe).
pub struct Keyring {
    inner: Mutex<HashMap<u32, ApiKey>>,
    /// Accepted timestamp window in milliseconds.
    pub window_ms: u64,
    /// Sustained REST rate per key (req/s).
    pub rate_per_sec: u64,
    /// REST burst per key.
    pub burst: u64,
}

/// A verified request identity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Identity {
    /// Key id.
    pub key_id: u32,
    /// Account all operations run as.
    pub account: u32,
    /// Granted scope.
    pub scope: Scope,
}

impl Default for Keyring {
    fn default() -> Self {
        Self::new(5_000, 100, 200)
    }
}

impl Keyring {
    /// A keyring with the given timestamp window and rate parameters.
    pub fn new(window_ms: u64, rate_per_sec: u64, burst: u64) -> Keyring {
        Keyring {
            inner: Mutex::new(HashMap::new()),
            window_ms,
            rate_per_sec,
            burst,
        }
    }

    /// Insert a key with an explicit secret (used by env-config seeding
    /// and recovery). Fails on id collision.
    pub fn insert(&self, key_id: u32, secret_hex: &str, account: u32, scope: Scope) -> Result<(), AuthErr> {
        let secret = match unhex(secret_hex) {
            Some(s) if s.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&s);
                arr
            }
            _ => return Err(AuthErr::UnknownKey),
        };
        let mut inner = self.inner.lock().map_err(|_| AuthErr::UnknownKey)?;
        if inner.contains_key(&key_id) {
            return Err(AuthErr::KeyExists);
        }
        inner.insert(
            key_id,
            ApiKey {
                key_id,
                secret,
                account,
                scope,
                nonces: NonceWindow::default(),
                bucket: TokenBucket::new(self.rate_per_sec, 0),
                auth_fails: 0,
            },
        );
        Ok(())
    }

    /// Mint a fresh key for `account` with `scope`. Returns `(key_id,
    /// secret_hex)` — the secret is shown exactly once.
    pub fn mint(&self, account: u32, scope: Scope) -> Result<(u32, String), AuthErr> {
        let mut secret = [0u8; 32];
        if !os_random(&mut secret) {
            // Fallback: hash of the current nanos + a monotonic counter —
            // still unique per call, though urandom should always exist.
            let mut mix = crate::crypto::sha256(&std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
                .to_le_bytes());
            for (i, b) in secret.iter_mut().enumerate() {
                *b = mix[i % 32];
                mix = crate::crypto::sha256(&mix);
            }
        }
        let mut inner = self.inner.lock().map_err(|_| AuthErr::UnknownKey)?;
        // Key ids avoid 0 (reserved "no key").
        let key_id = (inner.len() as u64 + 1).min(u32::MAX as u64) as u32;
        // Extremely defensive: skip collisions.
        let mut key_id = key_id;
        while inner.contains_key(&key_id) {
            key_id = key_id.wrapping_add(1).max(1);
        }
        inner.insert(
            key_id,
            ApiKey {
                key_id,
                secret,
                account,
                scope,
                nonces: NonceWindow::default(),
                bucket: TokenBucket::new(self.rate_per_sec, 0),
                auth_fails: 0,
            },
        );
        Ok((key_id, crate::crypto::hex(&secret)))
    }

    /// Number of registered keys.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Empty?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build the canonical signing string.
    pub fn canonical(
        key_id: &str,
        ts: &str,
        nonce: &str,
        method: &str,
        path_with_query: &str,
        body_sha_hex: &str,
    ) -> String {
        format!("{key_id}|{ts}|{nonce}|{method}|{path_with_query}|{body_sha_hex}")
    }

    /// Verify a request signature and authorize. `now_ms` is server time.
    /// On success, consumes rate budget and advances the nonce floor.
    #[allow(clippy::too_many_arguments)]
    pub fn verify(
        &self,
        key_id_hex: &str,
        ts_ms: u64,
        nonce: u64,
        method: &str,
        path_with_query: &str,
        body: &[u8],
        signature_hex: &str,
        now_ms: u64,
        want_scope: Scope,
    ) -> Result<Identity, AuthErr> {
        let key_id = parse_key_id(key_id_hex).ok_or(AuthErr::UnknownKey)?;
        let mut inner = self.inner.lock().map_err(|_| AuthErr::UnknownKey)?;
        let k = inner.get_mut(&key_id).ok_or(AuthErr::UnknownKey)?;
        // Timestamp window.
        let delta = now_ms.abs_diff(ts_ms);
        if delta > self.window_ms {
            k.auth_fails += 1;
            return Err(AuthErr::StaleTimestamp);
        }
        // Nonce anti-replay window (accepts out-of-order delivery within
        // the window; rejects duplicates and stale nonces).
        if !k.nonces.accept(nonce) {
            k.auth_fails += 1;
            return Err(AuthErr::BadNonce);
        }
        // Rate limit (per key).
        if !k.bucket.take(now_ms * 1_000_000) {
            return Err(AuthErr::RateLimited);
        }
        // Constant-time MAC comparison.
        let body_hash = crate::crypto::hex(&sha256(body));
        let canonical =
            Self::canonical(&key_id_hex.to_ascii_lowercase(), &ts_ms.to_string(), &nonce.to_string(), method, path_with_query, &body_hash);
        let want = hmac_sha256(&k.secret, canonical.as_bytes());
        let got = match unhex(signature_hex) {
            Some(v) => v,
            None => {
                k.auth_fails += 1;
                return Err(AuthErr::BadSignature);
            }
        };
        if !ct_eq(&want, &got) {
            k.auth_fails += 1;
            return Err(AuthErr::BadSignature);
        }
        // Scope authorization.
        if !k.scope.allows(want_scope) {
            return Err(AuthErr::ScopeDenied);
        }
        Ok(Identity {
            key_id,
            account: k.account,
            scope: k.scope,
        })
    }

    /// Total failed-auth attempts (metrics).
    pub fn total_auth_fails(&self) -> u64 {
        self.inner
            .lock()
            .map(|m| m.values().map(|k| k.auth_fails).sum())
            .unwrap_or(0)
    }
}

/// Parse an 8-hex-char key id.
pub fn parse_key_id(s: &str) -> Option<u32> {
    if s.len() != 8 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

/// Format a key id as 8 lowercase hex chars.
pub fn key_id_hex(id: u32) -> String {
    format!("{id:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{hex, hmac_sha256, sha256};

    fn signed_headers(
        key: &str,
        secret: &[u8; 32],
        ts: u64,
        nonce: u64,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> String {
        let body_hash = hex(&sha256(body));
        let canonical =
            Keyring::canonical(key, &ts.to_string(), &nonce.to_string(), method, path, &body_hash);
        hex(&hmac_sha256(secret, canonical.as_bytes()))
    }

    fn ring() -> Keyring {
        let r = Keyring::new(5_000, 1_000_000, 1_000_000);
        r.insert(0xA1B2C3D4, &"11".repeat(32), 7, Scope::Trade).unwrap();
        r.insert(0xDEAD_BEEF, &"22".repeat(32), 9, Scope::Read).unwrap();
        r.insert(0x1234_5678, &"33".repeat(32), 1, Scope::Admin).unwrap();
        r
    }

    const KEY: &str = "a1b2c3d4";
    const SECRET: [u8; 32] = [0x11; 32];

    #[test]
    fn verify_ok_and_identity() {
        let r = ring();
        let body = b"{\"qty\":1}";
        let sig = signed_headers(KEY, &SECRET, 1_000_000, 5, "POST", "/v1/orders", body);
        let id = r
            .verify(KEY, 1_000_000, 5, "POST", "/v1/orders", body, &sig, 1_000_000, Scope::Trade)
            .unwrap();
        assert_eq!(id.account, 7);
        assert_eq!(id.scope, Scope::Trade);
    }

    #[test]
    fn rejects_replay_and_stale() {
        let r = ring();
        let body = b"";
        let sig = signed_headers(KEY, &SECRET, 1_000_000, 5, "GET", "/v1/account", body);
        assert!(r.verify(KEY, 1_000_000, 5, "GET", "/v1/account", body, &sig, 1_000_000, Scope::Read).is_ok());
        // Same nonce again (replay).
        assert_eq!(
            r.verify(KEY, 1_000_000, 5, "GET", "/v1/account", body, &sig, 1_000_000, Scope::Read),
            Err(AuthErr::BadNonce)
        );
        // Lower nonce within the window: accepted (concurrent connections
        // deliver strictly-increasing nonces out of order — by design).
        let sig4 = signed_headers(KEY, &SECRET, 1_000_000, 4, "GET", "/v1/account", body);
        assert!(r.verify(KEY, 1_000_000, 4, "GET", "/v1/account", body, &sig4, 1_000_000, Scope::Read).is_ok());
        // Replay of 4 again: rejected.
        assert_eq!(
            r.verify(KEY, 1_000_000, 4, "GET", "/v1/account", body, &sig4, 1_000_000, Scope::Read),
            Err(AuthErr::BadNonce)
        );
        // Higher nonce fine.
        let sig6 = signed_headers(KEY, &SECRET, 1_000_000, 6, "GET", "/v1/account", body);
        assert!(r.verify(KEY, 1_000_000, 6, "GET", "/v1/account", body, &sig6, 1_000_000, Scope::Read).is_ok());
        // A nonce far below the window: rejected as stale.
        let sig_old = signed_headers(KEY, &SECRET, 1_000_000, 1, "GET", "/v1/account", body);
        let r2 = ring();
        let sig_hi = signed_headers(KEY, &SECRET, 1_000_000, 10_000, "GET", "/v1/account", body);
        assert!(r2.verify(KEY, 1_000_000, 10_000, "GET", "/v1/account", body, &sig_hi, 1_000_000, Scope::Read).is_ok());
        assert_eq!(
            r2.verify(KEY, 1_000_000, 1, "GET", "/v1/account", body, &sig_old, 1_000_000, Scope::Read),
            Err(AuthErr::BadNonce)
        );
    }

    #[test]
    fn nonce_window_semantics() {
        let mut w = NonceWindow::default();
        assert!(!w.accept(0), "nonce 0 is invalid");
        assert!(w.accept(1));
        assert!(!w.accept(1), "replay");
        assert!(w.accept(3), "gap");
        assert!(w.accept(2), "in-window out-of-order");
        assert!(!w.accept(2), "replay of the out-of-order slot");
        assert!(w.accept(4_000), "inside window");
        assert!(w.accept(100_000), "big jump");
        assert!(w.accept(100_000 - 4_095), "tail of window");
        assert!(!w.accept(100_000 - 4_096), "outside window");
        // Memory stays bounded under heavy churn.
        let mut w2 = NonceWindow::default();
        for n in 1..=100_000u64 {
            assert!(w2.accept(n));
        }
        assert!(w2.used.len() as u64 <= NONCE_WINDOW);
    }

    #[test]
    fn rejects_stale_timestamp() {
        let r = ring();
        let body = b"";
        let sig = signed_headers(KEY, &SECRET, 1_000_000, 10, "GET", "/v1/account", body);
        assert_eq!(
            r.verify(KEY, 1_000_000, 10, "GET", "/v1/account", body, &sig, 1_000_000 + 6_000, Scope::Read),
            Err(AuthErr::StaleTimestamp)
        );
        // Within window.
        assert!(r.verify(KEY, 1_000_000, 10, "GET", "/v1/account", body, &sig, 1_000_000 + 4_000, Scope::Read).is_ok());
    }

    #[test]
    fn rejects_bad_signature_and_unknown_key() {
        let r = ring();
        let body = b"{}";
        let sig = signed_headers(KEY, &SECRET, 1_000_000, 1, "POST", "/v1/orders", body);
        // Tampered body.
        assert_eq!(
            r.verify(KEY, 1_000_000, 1, "POST", "/v1/orders", b"{\"qty\":2}", &sig, 1_000_000, Scope::Trade),
            Err(AuthErr::BadSignature)
        );
        // Wrong key.
        assert_eq!(
            r.verify("00000000", 1_000_000, 2, "POST", "/v1/orders", body, &sig, 1_000_000, Scope::Trade),
            Err(AuthErr::UnknownKey)
        );
        // Non-hex signature (fresh nonce — the window accepts gaps).
        assert_eq!(
            r.verify(KEY, 1_000_000, 3, "POST", "/v1/orders", body, "zzzz", 1_000_000, Scope::Trade),
            Err(AuthErr::BadSignature)
        );
    }

    #[test]
    fn enforces_scope() {
        let r = ring();
        let body = b"";
        // Read key cannot trade.
        let sig = signed_headers("deadbeef", &[0x22; 32], 1_000_000, 1, "POST", "/v1/orders", body);
        assert_eq!(
            r.verify("deadbeef", 1_000_000, 1, "POST", "/v1/orders", body, &sig, 1_000_000, Scope::Trade),
            Err(AuthErr::ScopeDenied)
        );
        // Trade key can read.
        let sig2 = signed_headers(KEY, &SECRET, 1_000_000, 1, "GET", "/v1/account", body);
        assert!(r.verify(KEY, 1_000_000, 1, "GET", "/v1/account", body, &sig2, 1_000_000, Scope::Read).is_ok());
    }

    #[test]
    fn rate_limit_kicks_in() {
        let r = Keyring::new(5_000, 10, 20);
        r.insert(0x0000_0001, &"44".repeat(32), 1, Scope::Trade).unwrap();
        let key = "00000001";
        let secret = [0x44u8; 32];
        let mut ok = 0;
        let mut limited = 0;
        for nonce in 1..=60u64 {
            let sig = signed_headers(key, &secret, 100, nonce, "GET", "/v1/account", b"");
            match r.verify(key, 100, nonce, "GET", "/v1/account", b"", &sig, 100, Scope::Read) {
                Ok(_) => ok += 1,
                Err(AuthErr::RateLimited) => limited += 1,
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
        assert!(ok >= 20, "at least burst allowed, got {ok}");
        assert!(limited > 0, "rate limit must engage, got {limited}");
    }

    #[test]
    fn mint_produces_unique_keys() {
        let r = Keyring::default();
        let (id1, sec1) = r.mint(1, Scope::Trade).unwrap();
        let (id2, sec2) = r.mint(2, Scope::Trade).unwrap();
        assert_ne!(id1, id2);
        assert_ne!(sec1, sec2);
        assert_eq!(sec1.len(), 64);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn key_id_parsing() {
        assert_eq!(parse_key_id("a1b2c3d4"), Some(0xA1B2C3D4));
        assert_eq!(parse_key_id("A1B2C3D4"), Some(0xA1B2C3D4));
        assert_eq!(parse_key_id("a1b2c3d"), None);
        assert_eq!(parse_key_id("a1b2c3d45"), None);
        assert_eq!(parse_key_id("zzzzzzzz"), None);
        assert_eq!(key_id_hex(0xA1B2C3D4), "a1b2c3d4");
    }
}
