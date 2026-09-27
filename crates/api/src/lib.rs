//! # poc-api
//!
//! Gateway protocol library for the venue (gap-register **G-25, G-27,
//! G-32**). This is the embeddable *protocol* layer — the shapes,
//! invariants, and state machines a gateway host (tokio/axum in
//! production) drives — kept dependency-free like the rest of the
//! workspace:
//!
//! * [`json`] — a compact deterministic JSON codec for WS/REST payloads.
//! * [`session`] — market-data sessions: snapshot + sequenced deltas,
//!   gap detection, explicit resync (G-27).
//! * [`auth`] — API-key registry with per-key monotonic nonces
//!   (replay protection), a pluggable [`auth::Signer`], and
//!   deterministic token-bucket rate limits (G-25).
//! * [`withdrawal`] — the withdrawal pipeline: request validation,
//!   time-locked settlement, cancellation refunds, manual-approval
//!   tiers, and a custody adapter (G-32).
//!
//! ## What the gateway host adds
//!
//! Transport (WebSocket / HTTP / gRPC), TLS, key storage, and HMAC
//! wiring — deliberately out of scope for a library that must replay
//! bit-exactly in CI.

pub mod auth;
pub mod json;
pub mod session;
pub mod withdrawal;

pub use auth::{AuthError, AuthRegistry, FixedSigner, Signer, TokenBucket};
pub use json::Json;
pub use session::{BookFeed, Delta, MarketDataSession, Snapshot};
pub use withdrawal::{
    CustodyAdapter, InMemoryCustody, WithdrawalError, WithdrawalPipeline, WithdrawalPolicy,
    WithdrawalRequest, WithdrawalState,
};
