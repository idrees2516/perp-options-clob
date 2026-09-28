//! # poc-api
//!
//! Gateway protocol library for the venue (gap-register **G-25, G-26,
//! G-27, G-32**). This is the embeddable *protocol* layer — the shapes,
//! invariants, and state machines a gateway host drives — kept
//! dependency-free like the rest of the workspace:
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
//! * [`fix`] — the FIX 4.4 wire codec and typed order-gateway subset
//!   (G-26 protocol core).
//! * [`fix_session`] — the FIX session state machine (G-26 transport):
//!   sequence integrity, resend/gap-fill recovery, heartbeats, and a
//!   real TCP adapter behind the [`fix_session::Wire`] trait.
//!
//! ## What the gateway host adds
//!
//! TLS, key storage, and HMAC wiring — deliberately out of scope for a
//! library that must replay bit-exactly in CI.

pub mod auth;
pub mod fix;
pub mod fix_session;
pub mod json;
pub mod session;
pub mod withdrawal;

pub use auth::{AuthError, AuthRegistry, FixedSigner, Signer, TokenBucket};
pub use fix_session::{
    DisconnectReason, FixEvent, FixFraming, FixSession, FixSessionError, LoopbackPair,
    LoopbackWire, Role, StoredMessage, TcpWire, Wire,
};
pub use json::Json;
pub use session::{BookFeed, Delta, MarketDataSession, Snapshot};
pub use withdrawal::{
    CustodyAdapter, InMemoryCustody, WithdrawalError, WithdrawalPipeline, WithdrawalPolicy,
    WithdrawalRequest, WithdrawalState,
};
