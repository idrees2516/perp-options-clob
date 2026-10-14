//! # ns-clob v2 — unified nanosecond 0DTE options trading stack
//!
//! One crate, zero runtime dependencies, `#![forbid(unsafe_code)]`.
//!
//! * [`engine::Shard`] — one underlying's complete market: CLOB matching,
//!   pre-trade risk, options margin, per-trade settlement, auctions,
//!   expiry, circuit breaker, journal, audit.
//! * [`platform::Platform`] — multi-shard router with SPSC command rings and
//!   worker threads (the 20M+ TPS ingress path).
//! * [`sim`] — deterministic market simulator for tests and throughput runs.
//!
//! Money is exact-integer double-entry: `sum(cash) + house == deposits`
//! after every single command. Time is logical nanoseconds; replaying the
//! journal reproduces state bit-for-bit (`state_hash`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod accounts;
pub mod auction;
pub mod book;
pub mod auth;
pub mod crypto;
pub mod engine;
pub mod expiry;
pub mod fees;
pub mod gateway;
pub mod http;
pub mod json;
pub mod journal;
pub mod margin;
pub mod marketdata;
pub mod platform;
pub mod replica;
pub mod sim;
pub mod symbol;
pub mod types;
pub mod venue;
pub mod ws;

pub use engine::{Cmd, Phase, Shard, ShardConfig, ShardStats};
pub use types::*;
