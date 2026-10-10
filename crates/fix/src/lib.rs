//! # poc-fix — the FIX 4.4 gateway (legacy-system connectivity)
//!
//! Every options desk of consequence runs buy-side infrastructure that
//! speaks one protocol: FIX. The OMS/EMS stacks at banks, prop firms,
//! and hedge funds cannot be rewritten for a new venue — the venue
//! must speak their dialect, byte for byte. This crate is that dialect:
//! a complete FIX 4.4 acceptor that bridges legacy desks onto the
//! engine's CLOB and, critically, onto the **RFQ dealer network**
//! (G-11) — `QuoteRequest` in, `Quote` out, `ExecutionReport` on
//! fills — so a dealer's existing FIX infrastructure can quote the
//! RFQ book without a single line of new client code.
//!
//! # Layout
//!
//! * [`wire`] — the `tag=value<SOH>` codec: `BodyLength(9)`,
//!   `CheckSum(10)`, strict structural validation, repeating-group
//!   access.
//! * [`session`] — the session protocol: Logon handshake with
//!   HeartBtInt, Heartbeat/TestRequest liveness, monotonic sequence
//!   numbers, ResendRequest for inbound gaps, store-and-forward resend
//!   with `PossDupFlag=Y` and `SequenceReset` GapFill for admin holes.
//! * [`app`] — the application layer: ClOrdID→OrderID routing,
//!   NewOrderSingle/Cancel/Replace/StatusRequest against the engine,
//!   ExecutionReport and CancelReject emissions, market-data
//!   snapshots, and RFQ-over-FIX.
//!
//! # Determinism
//!
//! No ambient clock, no I/O, no threads: every call takes `now`
//! explicitly and returns the bytes to write. Replaying the same
//! inbound frames at the same timestamps reproduces the same outbound
//! stream bit for bit — the same discipline as the engine's own
//! journal.

pub mod app;
pub mod session;
pub mod wire;

pub use app::{FixGateway, GatewayError, OrderRouter};
pub use session::{FixSession, Inbound, SessionError, SessionState};
pub use wire::{tag, FixMessage, MsgType, WireError, BEGIN_STRING, SOH};
