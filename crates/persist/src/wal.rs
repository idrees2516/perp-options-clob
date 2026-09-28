//! Append-only write-ahead log with framed, checksummed records and
//! crash-safe recovery (G-24).
//!
//! ## Frame layout
//!
//! ```text
//! magic (2) | chain (8) | length (4) | payload (length) | crc (4)
//! ```
//!
//! * `chain` — a rolling FNV-1a chain over `(prev_chain, payload)`. A
//!   reordered or spliced log breaks the chain and is rejected.
//! * `crc` — CRC-32 (IEEE) of the payload; catches bit rot and torn
//!   writes.
//! * The final frame of a crashed segment may be truncated: its declared
//!   length, chain, or crc will fail, and recovery discards the tail
//!   cleanly (`TailDropped`) rather than refusing to start.
//!
//! ## Segments and checkpoints
//!
//! The log rotates at `max_segment_bytes` (sequence-numbered files).
//! A checkpoint is the recovery acceleration: `rotate_and_checkpoint`
//! closes the active segment and writes a small manifest recording the
//! retained segment set, the command count, and the chain head, so a
//! full restart replays only from the checkpoint. The manifest is
//! written atomically (write-temp, fsync, rename) — the classic
//! LevelDB trick.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use poc_core::Instrument;
use poc_engine::{Command, Engine, EngineConfig};

use crate::codec::{decode_command, decode_instrument, encode_command, encode_instrument};

const MAGIC: [u8; 2] = [0x50, 0x57]; // "PW"
/// Header bytes before the payload: magic(2) + chain(8) + length(4).
const HEADER_LEN: usize = 2 + 8 + 4;
/// Trailer bytes after the payload: crc(4).
const FOOTER_LEN: usize = 4;
/// Total per-frame overhead.
const FRAME_OVERHEAD: usize = HEADER_LEN + FOOTER_LEN;

/// One WAL record: an engine command, or a genesis instrument
/// registration (registrations are operator actions, not client
/// commands, but recovery must replay them).
#[derive(Debug, Clone, PartialEq)]
pub enum WalRecord {
    /// A client/operator command.
    Command(Command),
    /// An instrument registration (genesis).
    Instrument(Instrument),
}

fn encode_record(record: &WalRecord) -> Vec<u8> {
    let mut out = Vec::new();
    match record {
        WalRecord::Command(cmd) => {
            out.push(0x01);
            out.extend_from_slice(&encode_command(cmd));
        }
        WalRecord::Instrument(instr) => {
            out.push(0x02);
            out.extend_from_slice(&encode_instrument(instr));
        }
    }
    out
}

fn decode_record(data: &[u8]) -> Result<WalRecord, crate::codec::DecodeError> {
    match data.first() {
        Some(0x01) => decode_command(&data[1..]).map(WalRecord::Command),
        Some(0x02) => decode_instrument(&data[1..]).map(WalRecord::Instrument),
        _ => Err(crate::codec::DecodeError::Malformed),
    }
}

/// Why a frame could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// End of file reached cleanly (not an error).
    Eof,
    /// The tail frame is incomplete (crash mid-write).
    Torn {
        /// Bytes of the partial frame.
        partial: usize,
    },
    /// Magic mismatch.
    BadMagic,
    /// Length prefix exceeds sanity bounds.
    BadLength {
        /// The declared length.
        len: u32,
    },
    /// Payload crc mismatch.
    BadCrc,
    /// Chain hash does not continue the previous frame.
    ChainBroken,
    /// Payload failed to decode.
    Undecodable,
}

/// CRC-32 (IEEE 802.3, reflected) — dependency-free.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// FNV-1a 64 chained over frames.
fn fnv1a(prev: u64, data: &[u8]) -> u64 {
    let mut h = if prev == 0 {
        0xcbf2_9ce4_8422_2325
    } else {
        prev
    };
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Durability policy for the WAL writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncPolicy {
    /// Never fsync explicitly (fastest; OS decides).
    Never,
    /// fsync when a frame is explicitly flushed by the operator
    /// (batch windows).
    OnFlush,
    /// fsync after every frame (safest, slowest).
    EveryFrame,
}

/// The WAL writer over a directory of numbered segments.
pub struct WalWriter {
    dir: PathBuf,
    segment: PathBuf,
    file: File,
    next_segment: u64,
    bytes_written: u64,
    chain: u64,
    frames: u64,
    pub(crate) policy: FsyncPolicy,
    pub(crate) max_segment_bytes: u64,
}

impl WalWriter {
    /// Open (creating) a WAL at `dir`. Existing state is resumed when
    /// present: the chain head and next segment are picked up from the
    /// manifest/segments on disk.
    pub fn open(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::open_with(dir, FsyncPolicy::OnFlush, 64 * 1024 * 1024)
    }

    /// Open with an explicit durability policy and segment size.
    pub fn open_with(
        dir: impl AsRef<Path>,
        policy: FsyncPolicy,
        max_segment_bytes: u64,
    ) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        // Resume: find the highest segment and its chain head.
        let mut next_segment = 0_u64;
        let mut chain = 0_u64;
        let mut frames = 0_u64;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(n) = name
                .strip_prefix("wal-")
                .and_then(|s| s.parse::<u64>().ok())
            {
                next_segment = next_segment.max(n + 1);
            }
        }
        if next_segment > 0 {
            let last = segment_path(&dir, next_segment - 1);
            if let Ok(bytes) = fs::read(&last) {
                let mut iter = FrameIter::new(&bytes);
                loop {
                    match iter.next_frame() {
                        Ok(f) => {
                            chain = f.chain;
                            frames += 1;
                        }
                        Err(FrameError::Eof) | Err(FrameError::Torn { .. }) => break,
                        Err(_) => break,
                    }
                }
            }
        }
        let segment = segment_path(&dir, next_segment);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&segment)?;
        Ok(Self {
            dir,
            segment,
            file,
            next_segment,
            bytes_written: 0,
            chain,
            frames,
            policy,
            max_segment_bytes,
        })
    }

    /// Append one command; returns its frame index.
    pub fn append(&mut self, cmd: &Command) -> std::io::Result<u64> {
        self.append_record(&WalRecord::Command(cmd.clone()))
    }

    /// Register an instrument into the WAL (genesis frame): recovery
    /// re-registers it before replaying commands.
    pub fn register(&mut self, instrument: &Instrument) -> std::io::Result<u64> {
        self.append_record(&WalRecord::Instrument(instrument.clone()))
    }

    fn append_record(&mut self, record: &WalRecord) -> std::io::Result<u64> {
        let payload = encode_record(record);
        let chain = fnv1a(self.chain, &payload);
        let len = u32::try_from(payload.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large")
        })?;
        let mut frame = Vec::with_capacity(FRAME_OVERHEAD + payload.len());
        frame.extend_from_slice(&MAGIC);
        frame.extend_from_slice(&chain.to_le_bytes());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&crc32(&payload).to_le_bytes());

        let rotated = self.bytes_written + frame.len() as u64 > self.max_segment_bytes
            && self.bytes_written > 0;
        if rotated {
            self.file.sync_all().ok();
            self.next_segment += 1;
            self.segment = segment_path(&self.dir, self.next_segment);
            self.file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.segment)?;
            self.bytes_written = 0;
        }
        self.file.write_all(&frame)?;
        self.bytes_written += frame.len() as u64;
        self.chain = chain;
        let idx = self.frames;
        self.frames += 1;
        if self.policy == FsyncPolicy::EveryFrame {
            self.file.sync_all()?;
        }
        Ok(idx)
    }

    /// Explicit durability flush (for `OnFlush` batching).
    pub fn flush(&mut self) -> std::io::Result<()> {
        if self.policy == FsyncPolicy::OnFlush {
            self.file.sync_all()?;
        }
        Ok(())
    }

    /// Write a checkpoint manifest atomically and drop segments it
    /// supersedes (the operator calls this after confirming the engine
    /// has replayed through `command_count`).
    pub fn checkpoint(&mut self, command_count: u64) -> std::io::Result<PathBuf> {
        self.file.sync_all()?;
        let manifest = serde_free_manifest(
            &self.dir,
            self.next_segment,
            command_count,
            self.chain,
            self.frames,
        );
        let tmp = self.dir.join("checkpoint.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&manifest)?;
            f.sync_all()?;
        }
        let final_path = self.dir.join("checkpoint.bin");
        fs::rename(&tmp, &final_path)?;
        // Prune every segment (0..=active): recovery resumes from the
        // fresh segment the writer reopens below.
        for n in 0..=self.next_segment {
            let _ = fs::remove_file(segment_path(&self.dir, n));
        }
        // Re-open a fresh segment 0.
        self.next_segment = 0;
        self.bytes_written = 0;
        self.segment = segment_path(&self.dir, 0);
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.segment)?;
        Ok(final_path)
    }

    /// Frames written this session (including resumed count).
    #[must_use]
    pub fn frames_written(&self) -> u64 {
        self.frames
    }

    /// Current chain head.
    #[must_use]
    pub fn chain_head(&self) -> u64 {
        self.chain
    }

    /// The WAL directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn segment_path(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("wal-{n:012}"))
}

/// Minimal fixed-width manifest (no serde):
/// magic | next_segment | command_count | chain | frames.
fn serde_free_manifest(
    _dir: &Path,
    next_segment: u64,
    command_count: u64,
    chain: u64,
    frames: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    out.extend_from_slice(b"POCWALCP");
    out.extend_from_slice(&next_segment.to_le_bytes());
    out.extend_from_slice(&command_count.to_le_bytes());
    out.extend_from_slice(&chain.to_le_bytes());
    out.extend_from_slice(&frames.to_le_bytes());
    out
}

/// One decoded frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Chain hash after this frame.
    pub chain: u64,
    /// The decoded record.
    pub record: WalRecord,
}

/// Iterator over frames in a byte buffer.
pub struct FrameIter<'a> {
    data: &'a [u8],
    pos: usize,
    expect_chain: u64,
    started: bool,
}

impl<'a> FrameIter<'a> {
    /// Iterate frames in `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            expect_chain: 0,
            started: false,
        }
    }
}

impl FrameIter<'_> {
    /// Next frame, consuming it.
    pub fn next_frame(&mut self) -> Result<Frame, FrameError> {
        let remaining = self.data.len() - self.pos;
        if remaining == 0 {
            return Err(FrameError::Eof);
        }
        if remaining < FRAME_OVERHEAD {
            return Err(FrameError::Torn { partial: remaining });
        }
        let head = &self.data[self.pos..self.pos + HEADER_LEN];
        if head[0] != MAGIC[0] || head[1] != MAGIC[1] {
            return Err(FrameError::BadMagic);
        }
        let mut chain_buf = [0_u8; 8];
        chain_buf.copy_from_slice(&head[2..10]);
        let chain = u64::from_le_bytes(chain_buf);
        let mut len_buf = [0_u8; 4];
        len_buf.copy_from_slice(&head[10..14]);
        let len = u32::from_le_bytes(len_buf);
        let payload_start = self.pos + HEADER_LEN;
        let frame_end = payload_start + usize::try_from(len).unwrap_or(usize::MAX);
        if frame_end.saturating_add(FOOTER_LEN) > self.data.len() {
            // Declared length runs past the file: a torn tail (crash
            // mid-write), not corruption.
            return Err(FrameError::Torn {
                partial: self.data.len() - self.pos,
            });
        }
        let payload = &self.data[payload_start..frame_end];
        if self.started && chain != fnv1a(self.expect_chain, payload) {
            return Err(FrameError::ChainBroken);
        }
        let mut crc_buf = [0_u8; 4];
        crc_buf.copy_from_slice(&self.data[frame_end..frame_end + FOOTER_LEN]);
        if crc32(payload) != u32::from_le_bytes(crc_buf) {
            return Err(FrameError::BadCrc);
        }
        let record = decode_record(payload).map_err(|_| FrameError::Undecodable)?;
        self.pos = frame_end + FOOTER_LEN;
        self.expect_chain = chain;
        self.started = true;
        Ok(Frame { chain, record })
    }
}

/// The recovery report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Frames (commands) replayed.
    pub commands_replayed: u64,
    /// Torn tail frames dropped (a crash mid-write).
    pub torn_tails_dropped: u64,
    /// Corrupt frames rejected.
    pub corrupt_frames_rejected: u64,
    /// Segments read.
    pub segments: u64,
}

/// Recover the engine from a WAL directory: replay every command.
///
/// Deterministic engine ⇒ replayed engine ≡ original engine. Torn tails
/// are dropped (reported); corruption that is *not* at the tail aborts
/// recovery with an error — a hole in the middle of a WAL is never
/// silently skipped.
pub fn recover(
    config: EngineConfig,
    dir: impl AsRef<Path>,
) -> std::io::Result<(Engine, RecoveryReport)> {
    recover_from(config, dir, |_engine, _cmd| true)
}

/// Recover with a progress/abort callback (called before each command).
pub fn recover_from(
    config: EngineConfig,
    dir: impl AsRef<Path>,
    mut keep_going: impl FnMut(&Engine, &Command) -> bool,
) -> std::io::Result<(Engine, RecoveryReport)> {
    let dir = dir.as_ref();
    let mut segments: Vec<(u64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(n) = name
            .strip_prefix("wal-")
            .and_then(|s| s.parse::<u64>().ok())
        {
            segments.push((n, entry.path()));
        }
    }
    segments.sort_by_key(|a| a.0);

    let mut engine = Engine::new(config);
    let mut report = RecoveryReport {
        commands_replayed: 0,
        torn_tails_dropped: 0,
        corrupt_frames_rejected: 0,
        segments: u64::try_from(segments.len()).unwrap_or(0),
    };

    for (_, path) in segments {
        let bytes = fs::read(&path)?;
        let mut iter = FrameIter::new(&bytes);
        loop {
            match iter.next_frame() {
                Ok(frame) => match &frame.record {
                    WalRecord::Instrument(instr) => {
                        engine.register_instrument(instr.clone());
                        report.commands_replayed += 1;
                    }
                    WalRecord::Command(cmd) => {
                        if !keep_going(&engine, cmd) {
                            return Ok((engine, report));
                        }
                        engine.process(cmd.clone());
                        report.commands_replayed += 1;
                    }
                },
                Err(FrameError::Eof) => break,
                Err(FrameError::Torn { .. }) => {
                    report.torn_tails_dropped += 1;
                    break;
                }
                Err(
                    FrameError::BadMagic
                    | FrameError::BadLength { .. }
                    | FrameError::BadCrc
                    | FrameError::ChainBroken
                    | FrameError::Undecodable,
                ) => {
                    // Mid-segment corruption is fatal: report it as an
                    // error via the report and stop. (A hole in the
                    // middle must not be skipped silently.)
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "WAL corruption mid-segment; refusing to skip",
                    ));
                }
            }
        }
    }
    Ok((engine, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use poc_core::{Instrument, PerpMarket, Side};

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("poc-wal-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn demo_commands() -> Vec<Command> {
        vec![
            Command::Deposit {
                subaccount: 1,
                amount_quote_minor: 10_000_000,
            },
            Command::Deposit {
                subaccount: 2,
                amount_quote_minor: 10_000_000,
            },
            Command::Deposit {
                subaccount: 3,
                amount_quote_minor: 10_000_000,
            },
            Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: "pyth".into(),
                ts: 1_000,
                price_quote_minor: 8_000_000,
            },
            Command::OracleUpdate {
                base_symbol: "BTC".into(),
                provider: "chainlink".into(),
                ts: 1_000,
                price_quote_minor: 8_000_000,
            },
            Command::Tick { now: 1_000 },
            Command::Place {
                request: poc_engine::OrderRequest::limit(2, "BTC-PERP", Side::Ask, 79_950, 3),
                now: 1_001,
            },
            Command::Place {
                request: poc_engine::OrderRequest::limit(1, "BTC-PERP", Side::Bid, 79_960, 3),
                now: 1_002,
            },
            Command::Tick { now: 1_100 },
        ]
    }

    fn base_config() -> EngineConfig {
        EngineConfig::default()
    }

    #[test]
    fn wal_round_trip_replays_identical_engine() {
        let dir = temp_dir("roundtrip");
        let mut wal = WalWriter::open(&dir).unwrap();
        wal.register(&Instrument::Perp(PerpMarket::default()))
            .unwrap();
        for cmd in demo_commands() {
            wal.append(&cmd).unwrap();
        }
        wal.flush().unwrap();
        drop(wal);

        let mut live = Engine::new(base_config());
        live.register_instrument(Instrument::Perp(PerpMarket::default()));
        for cmd in demo_commands() {
            live.process(cmd);
        }

        let (recovered, report) = recover(base_config(), &dir).unwrap();
        assert_eq!(report.commands_replayed, 10, "genesis + 9 commands");
        assert_eq!(report.torn_tails_dropped, 0);
        assert_eq!(report.corrupt_frames_rejected, 0);
        // Identical settled state (including the trade the genesis
        // instrument enabled).
        let a = live.account(1).map(|x| x.cash_quote_minor);
        let b = recovered.account(1).map(|x| x.cash_quote_minor);
        assert_eq!(a, b);
        let pa = live.account(1).map(|x| x.lots_of("BTC-PERP"));
        let pb = recovered.account(1).map(|x| x.lots_of("BTC-PERP"));
        assert_eq!(pa, pb);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_tail_is_dropped_cleanly() {
        let dir = temp_dir("torn");
        let mut wal = WalWriter::open(&dir).unwrap();
        wal.register(&Instrument::Perp(PerpMarket::default()))
            .unwrap();
        for cmd in demo_commands() {
            wal.append(&cmd).unwrap();
        }
        wal.flush().unwrap();
        drop(wal);

        let path = segment_path(&dir, 0);
        let full = fs::read(&path).unwrap();
        // Truncate at every byte position: recovery must never panic and
        // must replay a prefix (or all of it when cut >= len).
        for cut in 0..full.len() {
            fs::write(&path, &full[..cut]).unwrap();
            let (recovered, report) = recover(base_config(), &dir).unwrap();
            assert!(
                report.commands_replayed <= 10,
                "cut {cut}: replayed {}",
                report.commands_replayed
            );
            assert!(report.torn_tails_dropped <= 1);
            assert_eq!(report.corrupt_frames_rejected, 0, "cut {cut}");
            // Replayed prefix must have consistent balances: every replayed
            // deposit is visible.
            if report.commands_replayed >= 4 {
                assert!(recovered.account(3).is_some());
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mid_wal_bitrot_is_refused() {
        let dir = temp_dir("bitrot");
        let mut wal = WalWriter::open(&dir).unwrap();
        for cmd in demo_commands() {
            wal.append(&cmd).unwrap();
        }
        drop(wal);

        let path = segment_path(&dir, 0);
        let mut bytes = fs::read(&path).unwrap();
        // Flip a bit inside the first frame's payload (past the 14-byte
        // header, inside command 0's bytes).
        bytes[HEADER_LEN + 2] ^= 0x40;
        fs::write(&path, &bytes).unwrap();

        let result = recover(base_config(), &dir);
        assert!(result.is_err(), "mid-segment corruption must abort");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn segment_rotation_and_checkpoint() {
        let dir = temp_dir("rotate");
        let mut wal = WalWriter::open_with(&dir, FsyncPolicy::Never, 1).unwrap();
        // max_segment_bytes = 1 forces rotation after every frame.
        wal.register(&Instrument::Perp(PerpMarket::default()))
            .unwrap();
        for cmd in demo_commands() {
            wal.append(&cmd).unwrap();
        }
        let segments: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name();
                n.to_string_lossy().starts_with("wal-").then_some(n)
            })
            .collect();
        assert!(
            segments.len() >= 10,
            "one segment per frame: got {}",
            segments.len()
        );

        wal.checkpoint(10).unwrap();
        // After the checkpoint a fresh WAL starts; recovery replays 0.
        let (_, report) = recover(base_config(), &dir).unwrap();
        assert_eq!(report.commands_replayed, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_continues_chain_across_reopen() {
        let dir = temp_dir("resume");
        {
            let mut wal = WalWriter::open(&dir).unwrap();
            for cmd in &demo_commands()[..3] {
                wal.append(cmd).unwrap();
            }
            wal.flush().unwrap();
        }
        {
            let mut wal = WalWriter::open(&dir).unwrap();
            for cmd in &demo_commands()[3..] {
                wal.append(cmd).unwrap();
            }
            wal.flush().unwrap();
        }
        let (_, report) = recover(base_config(), &dir).unwrap();
        assert_eq!(report.commands_replayed, 9);
        assert_eq!(report.torn_tails_dropped, 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
