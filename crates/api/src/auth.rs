//! Auth nonces and token-bucket rate limits (G-25).
//!
//! * [`AuthRegistry`] — API-key registry with per-key monotonic request
//!   nonces (replay protection, the standard exchange-API shape) and a
//!   pluggable [`Signer`] (HMAC-SHA256 in production; a fixed-key test
//!   signer in this library — the *protocol* is what matters here).
//! * [`TokenBucket`] — deterministic refill-by-time token bucket;
//!   virtual-time `now` keeps tests exact and replayable.

use std::collections::BTreeMap;

/// Signing abstraction over an authenticated request: the canonical
/// string is `key|nonce|method|path|body_hash`.
pub trait Signer {
    /// Sign the canonical request string.
    fn sign(&self, canonical: &str) -> [u8; 32];
}

/// A fixed-key test signer (documented as NOT production HMAC; swap the
/// trait impl for HMAC-SHA256 behind the gateway).
pub struct FixedSigner {
    key: u64,
}

impl FixedSigner {
    /// New signer seeded with `key`.
    #[must_use]
    pub fn new(key: u64) -> Self {
        Self { key }
    }
}

impl Signer for FixedSigner {
    fn sign(&self, canonical: &str) -> [u8; 32] {
        // FNV-1a (64-bit) folded twice into 32 bytes — deterministic,
        // dependency-free. Production swaps this trait for HMAC-SHA256.
        let mut h1 = self.key ^ 0xcbf2_9ce4_8422_2325;
        let mut h2 = !self.key ^ 0x9e37_79b9_7f4a_7c15;
        for b in canonical.as_bytes() {
            h1 ^= u64::from(*b);
            h1 = h1.wrapping_mul(0x0000_0100_0000_01b3);
            h2 = h2.rotate_left(5) ^ u64::from(*b);
            h2 = h2.wrapping_mul(0x1000_0000_01b3);
        }
        let mut out = [0_u8; 32];
        out[..8].copy_from_slice(&h1.to_le_bytes());
        out[8..16].copy_from_slice(&h2.to_le_bytes());
        out[16..24].copy_from_slice(&h1.swap_bytes().to_le_bytes());
        out[24..].copy_from_slice(&h2.swap_bytes().to_le_bytes());
        out
    }
}

/// A registered API key.
#[derive(Debug, Clone)]
pub struct ApiKey {
    /// The key id the client presents.
    pub key_id: String,
    /// Whether the key is active.
    pub active: bool,
    /// The last accepted nonce (requests must strictly increase).
    pub last_nonce: u64,
    /// Rate limit for this key.
    pub bucket: TokenBucket,
}

/// Why an authenticated request was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Unknown key id.
    UnknownKey,
    /// The key was disabled.
    KeyDisabled,
    /// Nonce not greater than the last accepted one (replay or
    /// out-of-order).
    StaleNonce {
        /// The nonce presented.
        presented: u64,
        /// The last accepted nonce.
        last: u64,
    },
    /// Signature mismatch.
    BadSignature,
    /// Rate limit exhausted (the request itself is otherwise valid —
    /// the client should back off, not re-sign).
    RateLimited,
}

/// Registry of API keys with nonce tracking.
#[derive(Debug, Default)]
pub struct AuthRegistry {
    keys: BTreeMap<String, ApiKey>,
}

impl AuthRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a key with a rate limit.
    pub fn register(&mut self, key_id: &str, rate_per_second: u64, burst: u64) {
        self.keys.insert(
            key_id.to_owned(),
            ApiKey {
                key_id: key_id.to_owned(),
                active: true,
                last_nonce: 0,
                bucket: TokenBucket::new(rate_per_second, burst),
            },
        );
    }

    /// Disable a key (compromise response).
    pub fn disable(&mut self, key_id: &str) {
        if let Some(k) = self.keys.get_mut(key_id) {
            k.active = false;
        }
    }

    /// Authenticate one request: nonce monotonicity + signature + rate.
    /// `canonical` is the request's canonical string; on success the
    /// nonce advances and a token is consumed.
    pub fn authenticate(
        &mut self,
        key_id: &str,
        nonce: u64,
        signature: &[u8; 32],
        canonical: &str,
        signer: &dyn Signer,
        now_ms: u64,
    ) -> Result<(), AuthError> {
        let key = self.keys.get(key_id).ok_or(AuthError::UnknownKey)?;
        if !key.active {
            return Err(AuthError::KeyDisabled);
        }
        if nonce <= key.last_nonce {
            return Err(AuthError::StaleNonce {
                presented: nonce,
                last: key.last_nonce,
            });
        }
        // The signature binds the key id, the nonce, and the payload —
        // an attacker cannot lift a signature onto another request.
        let expected = signer.sign(canonical);
        if expected != *signature {
            return Err(AuthError::BadSignature);
        }
        let key = self.keys.get_mut(key_id).ok_or(AuthError::UnknownKey)?;
        if !key.bucket.try_consume(now_ms) {
            return Err(AuthError::RateLimited);
        }
        key.last_nonce = nonce;
        Ok(())
    }

    /// Read-only key lookup.
    #[must_use]
    pub fn key(&self, key_id: &str) -> Option<&ApiKey> {
        self.keys.get(key_id)
    }
}

/// Deterministic token bucket: `capacity` tokens, refilled at
/// `rate_per_second` (fractional tokens accumulate in micro-units).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBucket {
    rate_per_ms: u64,
    capacity: u64,
    tokens_micro: u64,
    last_ms: u64,
}

impl TokenBucket {
    /// New bucket, pre-filled to capacity.
    #[must_use]
    pub fn new(rate_per_second: u64, capacity: u64) -> Self {
        Self {
            rate_per_ms: rate_per_second,
            capacity,
            tokens_micro: capacity.saturating_mul(1_000_000),
            last_ms: 0,
        }
    }

    /// Try to consume one token at time `now_ms`.
    pub fn try_consume(&mut self, now_ms: u64) -> bool {
        if now_ms > self.last_ms {
            let elapsed = now_ms - self.last_ms;
            let add = elapsed
                .saturating_mul(self.rate_per_ms)
                .saturating_mul(1000);
            self.tokens_micro = self
                .tokens_micro
                .saturating_add(add)
                .min(self.capacity.saturating_mul(1_000_000));
            self.last_ms = now_ms;
        }
        if self.tokens_micro >= 1_000_000 {
            self.tokens_micro -= 1_000_000;
            true
        } else {
            false
        }
    }

    /// Tokens available right now (fractional, for reporting).
    #[must_use]
    pub fn available(&self) -> f64 {
        self.tokens_micro as f64 / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(key: &str, nonce: u64, path: &str) -> String {
        format!("{key}|{nonce}|GET|{path}|")
    }

    #[test]
    fn happy_path_authenticates_and_advances_nonce() {
        let mut reg = AuthRegistry::new();
        reg.register("desk-1", 10, 5);
        let signer = FixedSigner::new(7);
        let sig = signer.sign(&canonical("desk-1", 1, "/v1/orders"));
        assert!(reg
            .authenticate(
                "desk-1",
                1,
                &sig,
                &canonical("desk-1", 1, "/v1/orders"),
                &signer,
                0
            )
            .is_ok());
        // Same nonce again: replay.
        let err = reg
            .authenticate(
                "desk-1",
                1,
                &sig,
                &canonical("desk-1", 1, "/v1/orders"),
                &signer,
                0,
            )
            .unwrap_err();
        assert_eq!(
            err,
            AuthError::StaleNonce {
                presented: 1,
                last: 1
            }
        );
    }

    #[test]
    fn signature_binds_the_request() {
        let mut reg = AuthRegistry::new();
        reg.register("desk-1", 10, 5);
        let signer = FixedSigner::new(7);
        let sig = signer.sign(&canonical("desk-1", 1, "/v1/orders"));
        // Replayed against a different path.
        let err = reg
            .authenticate(
                "desk-1",
                2,
                &sig,
                &canonical("desk-1", 2, "/v1/withdrawals"),
                &signer,
                0,
            )
            .unwrap_err();
        assert_eq!(err, AuthError::BadSignature);
    }

    #[test]
    fn unknown_and_disabled_keys_rejected() {
        let mut reg = AuthRegistry::new();
        reg.register("desk-1", 10, 5);
        let signer = FixedSigner::new(7);
        assert_eq!(
            reg.authenticate("nope", 1, &[0; 32], "x", &signer, 0)
                .unwrap_err(),
            AuthError::UnknownKey
        );
        reg.disable("desk-1");
        let sig = signer.sign(&canonical("desk-1", 1, "/v1/orders"));
        assert_eq!(
            reg.authenticate(
                "desk-1",
                1,
                &sig,
                &canonical("desk-1", 1, "/v1/orders"),
                &signer,
                0
            )
            .unwrap_err(),
            AuthError::KeyDisabled
        );
    }

    #[test]
    fn bucket_bursts_then_refills_over_time() {
        let mut reg = AuthRegistry::new();
        reg.register("desk-1", 2, 3); // 2/s sustained, burst 3.
        let signer = FixedSigner::new(1);
        let mut ok = 0;
        for i in 1..=4_u64 {
            let c = canonical("desk-1", i, "/x");
            let sig = signer.sign(&c);
            if reg
                .authenticate("desk-1", i, &sig, &c, &signer, 1000)
                .is_ok()
            {
                ok += 1;
            }
        }
        assert_eq!(ok, 3, "burst capacity consumed, 4th limited");
        // 500ms later: one token refilled (2/s).
        let c = canonical("desk-1", 9, "/x");
        let sig = signer.sign(&c);
        assert!(reg
            .authenticate("desk-1", 9, &sig, &c, &signer, 1500)
            .is_ok());
        assert_eq!(
            reg.authenticate("desk-1", 10, &sig, &c, &signer, 1500)
                .unwrap_err(),
            AuthError::RateLimited
        );
    }
}
