//! # poc-persist
//!
//! Durable command WAL for the deterministic engine (gap-register
//! **G-24**).
//!
//! The engine is a pure command→event state machine (see `poc-engine`),
//! which is exactly the property a write-ahead log wants: **replaying
//! the command log reproduces the state bit-for-bit**. This crate makes
//! that durability practical:
//!
//! * [`codec`] — a compact, total binary encoding of every
//!   [`poc_engine::Command`] (varints, length prefixes, tagged enums).
//! * [`wal`] — append-only segments of framed, CRC-32-checked,
//!   FNV-chain-linked records; segment rotation; atomic checkpoint
//!   manifests; torn-tail-safe recovery; mid-log corruption refusal.
//!
//! ## Crash model
//!
//! A crash can leave three states on disk, all handled:
//!
//! | Disk state | Recovery result |
//! |---|---|
//! | Complete frames only | Full replay, no loss |
//! | Final frame truncated | Prefix replay, `torn_tails_dropped = 1` |
//! | Bit rot inside a frame | CRC fails; **mid-log corruption aborts** (never silently skipped) |
//!
//! ## Checkpoints
//!
//! `WalWriter::checkpoint(n)` writes an atomic manifest and prunes
//! superseded segments, bounding restart replay time. The checkpoint
//! records the frame count and chain head, so an attacker (or a bug)
//! cannot silently truncate history without breaking the resumed chain.

pub mod codec;
pub mod wal;

pub use codec::{decode_command, encode_command, DecodeError, Decoder, Encoder};
pub use wal::{
    crc32, recover, recover_from, Frame, FrameError, FrameIter, FsyncPolicy, RecoveryReport,
    WalWriter,
};
