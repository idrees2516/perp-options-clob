//! Event stream, command journal, CRC32 and WAL framing.
//!
//! The engine appends fixed-size events into a reusable buffer per batch
//! (zero allocation steady state). Commands (the journalable inputs) are
//! framed into a command log with CRC32-protected batches — replaying the
//! log into a fresh engine reproduces the exact same state (`state_hash`
//! bit-for-bit).

use crate::types::{AcctId, Cash, Nanos, OrderId, Price, Qty, Seq, SymbolId};

/// Engine events. Fixed-size, POD-serializable, part of the wire contract.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// Order accepted and (possibly partially) resting.
    Ack {
        /// Order id.
        order: OrderId,
        /// Account.
        account: AcctId,
        /// Symbol.
        symbol: SymbolId,
        /// Leaves resting (0 = fully filled / not rested).
        leaves: Qty,
    },
    /// Order rejected.
    Reject {
        /// Client order reference (order id assigned speculatively).
        order: OrderId,
        /// Account.
        account: AcctId,
        /// Symbol.
        symbol: SymbolId,
        /// Reject code.
        code: u8,
    },
    /// Order cancelled (user, system, STP, expiry sweep, auction sweep).
    Canceled {
        /// Order id.
        order: OrderId,
        /// Account.
        account: AcctId,
        /// Symbol.
        symbol: SymbolId,
        /// Leaves at cancel.
        leaves: Qty,
        /// 0 user, 1 STP, 2 expiry, 3 auction, 4 OCO, 5 replace.
        reason: u8,
    },
    /// Trade: taker aggressed maker.
    Trade {
        /// Symbol.
        symbol: SymbolId,
        /// Execution price.
        price: Price,
        /// Execution quantity.
        qty: Qty,
        /// Taker order.
        taker: OrderId,
        /// Maker order.
        maker: OrderId,
        /// Taker account.
        taker_acct: AcctId,
        /// Maker account.
        maker_acct: AcctId,
        /// 0 buy aggressor, 1 sell aggressor, 2 auction.
        aggressor: u8,
        /// Fee charged to taker (paid to house).
        taker_fee: u64,
        /// Maker fee: positive = charged, 0 = none; rebates are separate.
        maker_fee: u64,
        /// True when maker received a rebate of `maker_fee`.
        maker_rebate: bool,
    },
    /// Option mark (last trade price) updated.
    Mark {
        /// Symbol.
        symbol: SymbolId,
        /// New mark.
        price: Price,
    },
    /// Best bid/offer changed.
    Bbo {
        /// Symbol.
        symbol: SymbolId,
        /// Best bid price (0 if none).
        bid: Price,
        /// Best bid qty.
        bid_qty: u64,
        /// Best ask price (0 if none).
        ask: Price,
        /// Best ask qty.
        ask_qty: u64,
    },
    /// Symbol entered auction phase.
    AuctionOpened {
        /// Symbol.
        symbol: SymbolId,
    },
    /// Auction cleared at a single price.
    AuctionCleared {
        /// Symbol.
        symbol: SymbolId,
        /// Clearing price.
        price: Price,
        /// Total cleared quantity.
        qty: Qty,
    },
    /// Underlying circuit-breaker halt.
    Halted {
        /// Underlying id.
        underlying: u16,
        /// Halt level (7/13 style).
        level: u8,
    },
    /// Halt lifted; symbols reopening via auction.
    Resumed {
        /// Underlying id.
        underlying: u16,
    },
    /// Position cash-settled at expiry.
    Exercised {
        /// Symbol.
        symbol: SymbolId,
        /// Account.
        account: AcctId,
        /// Contracts settled (absolute).
        qty: Qty,
        /// Intrinsic per contract.
        intrinsic: Price,
        /// Settlement fee charged.
        fee: u64,
        /// Net cash flow to the account (+ long receive, - short pay).
        cash_flow: Cash,
    },
    /// Symbol expired; no further trading.
    Expired {
        /// Symbol.
        symbol: SymbolId,
        /// Settlement price used.
        settle: Price,
    },
    /// Deposit acknowledged.
    Deposited {
        /// Account.
        account: AcctId,
        /// Amount.
        amount: Cash,
    },
    /// Withdrawal acknowledged.
    Withdrawn {
        /// Account.
        account: AcctId,
        /// Amount.
        amount: Cash,
    },
    /// Account cash fell below margin requirement (soft risk event).
    MarginDeficit {
        /// Account.
        account: AcctId,
        /// Required margin.
        required: u64,
        /// Current cash.
        cash: Cash,
    },
    /// Fast-market state toggled (band tightening).
    FastMarket {
        /// Underlying id.
        underlying: u16,
        /// Fast market on?
        on: bool,
    },
}

impl Event {
    /// Serialized size of the largest variant (all are <= this).
    pub const WIRE_MAX: usize = 64;

    /// Serialize into `buf` (little-endian, fixed layout per variant tag).
    /// Returns bytes written. Layouts are stable — part of the wire contract.
    pub fn write_bytes(&self, buf: &mut [u8]) -> usize {
        let put64 = |buf: &mut [u8], off: usize, v: u64| {
            buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
        };
        let put32 = |buf: &mut [u8], off: usize, v: u32| {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        };
        match self {
            Event::Ack {
                order,
                account,
                symbol,
                leaves,
            } => {
                buf[0] = 1;
                put64(buf, 1, *order);
                put32(buf, 9, *account);
                put32(buf, 13, *symbol);
                put32(buf, 17, *leaves);
                21
            }
            Event::Reject {
                order,
                account,
                symbol,
                code,
            } => {
                buf[0] = 2;
                put64(buf, 1, *order);
                put32(buf, 9, *account);
                put32(buf, 13, *symbol);
                buf[17] = *code;
                18
            }
            Event::Canceled {
                order,
                account,
                symbol,
                leaves,
                reason,
            } => {
                buf[0] = 3;
                put64(buf, 1, *order);
                put32(buf, 9, *account);
                put32(buf, 13, *symbol);
                put32(buf, 17, *leaves);
                buf[21] = *reason;
                22
            }
            Event::Trade {
                symbol,
                price,
                qty,
                taker,
                maker,
                taker_acct,
                maker_acct,
                aggressor,
                taker_fee,
                maker_fee,
                maker_rebate,
            } => {
                buf[0] = 4;
                put32(buf, 1, *symbol);
                put64(buf, 5, *price);
                put32(buf, 13, *qty);
                put64(buf, 17, *taker);
                put64(buf, 25, *maker);
                put32(buf, 33, *taker_acct);
                put32(buf, 37, *maker_acct);
                buf[41] = *aggressor;
                put64(buf, 42, *taker_fee);
                put64(buf, 50, *maker_fee);
                buf[58] = *maker_rebate as u8;
                59
            }
            Event::Mark { symbol, price } => {
                buf[0] = 5;
                put32(buf, 1, *symbol);
                put64(buf, 5, *price);
                13
            }
            Event::Bbo {
                symbol,
                bid,
                bid_qty,
                ask,
                ask_qty,
            } => {
                buf[0] = 6;
                put32(buf, 1, *symbol);
                put64(buf, 5, *bid);
                put64(buf, 13, *bid_qty);
                put64(buf, 21, *ask);
                put64(buf, 29, *ask_qty);
                37
            }
            Event::AuctionOpened { symbol } => {
                buf[0] = 7;
                put32(buf, 1, *symbol);
                5
            }
            Event::AuctionCleared { symbol, price, qty } => {
                buf[0] = 8;
                put32(buf, 1, *symbol);
                put64(buf, 5, *price);
                put32(buf, 13, *qty);
                17
            }
            Event::Halted { underlying, level } => {
                buf[0] = 9;
                buf[1] = (*underlying & 0xFF) as u8;
                buf[2] = (*underlying >> 8) as u8;
                buf[3] = *level;
                4
            }
            Event::Resumed { underlying } => {
                buf[0] = 10;
                buf[1] = (*underlying & 0xFF) as u8;
                buf[2] = (*underlying >> 8) as u8;
                3
            }
            Event::Exercised {
                symbol,
                account,
                qty,
                intrinsic,
                fee,
                cash_flow,
            } => {
                buf[0] = 11;
                put32(buf, 1, *symbol);
                put32(buf, 5, *account);
                put32(buf, 9, *qty);
                put64(buf, 13, *intrinsic);
                put64(buf, 21, *fee);
                put64(buf, 29, *cash_flow as u64);
                put64(buf, 37, ((*cash_flow as u128) >> 64) as u64);
                45
            }
            Event::Expired { symbol, settle } => {
                buf[0] = 12;
                put32(buf, 1, *symbol);
                put64(buf, 5, *settle);
                13
            }
            Event::Deposited { account, amount } => {
                buf[0] = 13;
                put32(buf, 1, *account);
                put64(buf, 5, *amount as u64);
                put64(buf, 13, ((*amount as u128) >> 64) as u64);
                21
            }
            Event::Withdrawn { account, amount } => {
                buf[0] = 14;
                put32(buf, 1, *account);
                put64(buf, 5, *amount as u64);
                put64(buf, 13, ((*amount as u128) >> 64) as u64);
                21
            }
            Event::MarginDeficit {
                account,
                required,
                cash,
            } => {
                buf[0] = 15;
                put32(buf, 1, *account);
                put64(buf, 5, *required);
                put64(buf, 13, *cash as u64);
                put64(buf, 21, ((*cash as u128) >> 64) as u64);
                29
            }
            Event::FastMarket { underlying, on } => {
                buf[0] = 16;
                buf[1] = (*underlying & 0xFF) as u8;
                buf[2] = (*underlying >> 8) as u8;
                buf[3] = *on as u8;
                4
            }
        }
    }
}

/// Reusable event batch buffer. `clear()` keeps capacity — the steady state
/// performs no allocation.
#[derive(Clone, Debug, Default)]
pub struct EvBuf {
    /// Events in this batch.
    pub events: Vec<Event>,
}

impl EvBuf {
    /// An empty buffer.
    pub fn new() -> EvBuf {
        EvBuf { events: Vec::new() }
    }

    /// An empty buffer with reserved capacity.
    pub fn with_capacity(n: usize) -> EvBuf {
        EvBuf {
            events: Vec::with_capacity(n),
        }
    }

    /// Drop batch contents, keeping capacity.
    #[inline(always)]
    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Append an event.
    #[inline(always)]
    pub fn push(&mut self, e: Event) {
        self.events.push(e);
    }

    /// Number of events in the batch.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Is the batch empty?
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// CRC32 (IEEE 802.3, reflected, poly 0xEDB88320) — table computed at first
/// use, then cached. Used for WAL batch integrity.
pub struct Crc32 {
    table: [u32; 256],
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    /// Build the table.
    pub fn new() -> Crc32 {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            table[i] = c;
            i += 1;
        }
        Crc32 { table }
    }

    /// CRC32 of a byte slice.
    pub fn of(&self, data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c = self.table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        c ^ 0xFFFF_FFFF
    }
}

/// A framed WAL record batch: `[magic u32][version u16][flags u16][len u32]
/// [crc32 u32][payload len bytes]`.
pub struct WalBatch;

impl WalBatch {
    /// Magic value.
    pub const MAGIC: u32 = 0x4E53_434C; // "NSCL"
    /// Format version.
    pub const VERSION: u16 = 2;

    /// Frame `payload` into `out`.
    pub fn frame(out: &mut Vec<u8>, payload: &[u8], crc: &Crc32) {
        out.extend_from_slice(&Self::MAGIC.to_le_bytes());
        out.extend_from_slice(&Self::VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc.of(payload).to_le_bytes());
        out.extend_from_slice(payload);
    }

    /// Validate and strip one frame; returns (payload slice, bytes consumed),
    /// or `None` on corruption/truncation.
    pub fn parse<'a>(buf: &'a [u8], crc: &Crc32) -> Option<(&'a [u8], usize)> {
        if buf.len() < 16 {
            return None;
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().ok()?);
        if magic != Self::MAGIC {
            return None;
        }
        let len = u32::from_le_bytes(buf[8..12].try_into().ok()?) as usize;
        if buf.len() < 16 + len {
            return None;
        }
        let want = u32::from_le_bytes(buf[12..16].try_into().ok()?);
        let payload = &buf[16..16 + len];
        if crc.of(payload) != want {
            return None;
        }
        Some((payload, 16 + len))
    }
}

/// Where flushed WAL batches land.
pub enum Sink {
    /// Discard (pure capacity mode: measures the engine, not the disk).
    None,
    /// Keep framed records in memory (replay tests, small sessions).
    Mem(Vec<u8>),
    /// Write framed records to any `Write` sink (production WAL file).
    Io(Box<dyn std::io::Write + Send>),
}

/// Command journal: batches serialized commands and frames them into a WAL
/// sink with CRC32 integrity. The hot path only ever appends to the pending
/// batch (~40 bytes memcpy); flushing is off the critical path.
pub struct CmdLog {
    sink: Sink,
    crc: Crc32,
    batch: Vec<u8>,
    scratch: Vec<u8>,
    written: u64,
}

impl CmdLog {
    /// A fresh in-memory log with estimated capacity.
    pub fn new(cap: usize) -> CmdLog {
        CmdLog {
            sink: Sink::Mem(Vec::with_capacity(cap)),
            crc: Crc32::new(),
            batch: Vec::with_capacity(4 * 1024),
            scratch: Vec::with_capacity(4 * 1024),
            written: 0,
        }
    }

    /// A log that discards flushed data (capacity benchmarking).
    pub fn discarding() -> CmdLog {
        CmdLog {
            sink: Sink::None,
            crc: Crc32::new(),
            batch: Vec::with_capacity(4 * 1024),
            scratch: Vec::with_capacity(4 * 1024),
            written: 0,
        }
    }

    /// Attach an arbitrary `Write` sink (e.g. a WAL file).
    pub fn with_sink(cap: usize, w: Box<dyn std::io::Write + Send>) -> CmdLog {
        CmdLog {
            sink: Sink::Io(w),
            crc: Crc32::new(),
            batch: Vec::with_capacity(cap.min(1 << 20)),
            scratch: Vec::with_capacity(cap.min(1 << 20)),
            written: 0,
        }
    }

    /// Total framed bytes written to the sink.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Append a serialized command (tag + fields) to the pending batch.
    pub fn append_raw(&mut self, raw: &[u8]) {
        self.batch.extend_from_slice(raw);
    }

    /// Pending (unflushed) byte length.
    #[inline]
    pub fn pending_len(&self) -> usize {
        self.batch.len()
    }

    /// Flush the pending batch as one CRC-framed record into the sink.
    pub fn flush(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        match &mut self.sink {
            Sink::None => self.batch.clear(),
            Sink::Mem(bytes) => {
                WalBatch::frame(bytes, &self.batch, &self.crc);
                self.written += self.batch.len() as u64 + 16;
                self.batch.clear();
            }
            Sink::Io(w) => {
                self.scratch.clear();
                WalBatch::frame(&mut self.scratch, &self.batch, &self.crc);
                let _ = w.write_all(&self.scratch);
                self.written += self.batch.len() as u64 + 16;
                self.batch.clear();
            }
        }
    }

    /// Iterate framed batches (memory sink only), yielding payloads.
    pub fn batches(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let bytes = match &self.sink {
            Sink::Mem(b) => b,
            _ => return out,
        };
        let mut rest = &bytes[..];
        let crc = Crc32::new();
        while let Some((payload, used)) = WalBatch::parse(rest, &crc) {
            out.push(payload.to_vec());
            rest = &rest[used..];
        }
        out
    }
}

/// Serialize a logical timestamp + sequence pair (command header).
#[inline]
pub fn cmd_header(buf: &mut Vec<u8>, tag: u8, time: Nanos, seq: Seq) {
    buf.push(tag);
    buf.extend_from_slice(&time.to_le_bytes());
    buf.extend_from_slice(&seq.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        let c = Crc32::new();
        assert_eq!(c.of(b"123456789"), 0xCBF4_3926);
        assert_eq!(c.of(b""), 0);
    }

    #[test]
    fn wal_roundtrip_and_corruption() {
        let c = Crc32::new();
        let mut out = Vec::new();
        WalBatch::frame(&mut out, b"hello world", &c);
        WalBatch::frame(&mut out, b"second batch", &c);
        let (p1, used) = WalBatch::parse(&out, &c).unwrap();
        assert_eq!(p1, b"hello world");
        let (p2, _) = WalBatch::parse(&out[used..], &c).unwrap();
        assert_eq!(p2, b"second batch");
        // corrupt one byte
        let mut bad = out.clone();
        bad[20] ^= 0xFF;
        assert!(WalBatch::parse(&bad, &c).is_none());
    }

    #[test]
    fn event_wire_roundtrip_smoke() {
        let e = Event::Trade {
            symbol: 3,
            price: 123_456,
            qty: 10,
            taker: 5,
            maker: 6,
            taker_acct: 7,
            maker_acct: 8,
            aggressor: 0,
            taker_fee: 650,
            maker_fee: 100,
            maker_rebate: true,
        };
        let mut buf = [0u8; Event::WIRE_MAX];
        let n = e.write_bytes(&mut buf);
        assert!(n <= Event::WIRE_MAX && n > 0);
        assert_eq!(buf[0], 4);
    }

    #[test]
    fn cmdlog_batches() {
        let mut log = CmdLog::new(1024);
        log.append_raw(&[1, 2, 3]);
        log.append_raw(&[4, 5]);
        log.flush();
        log.append_raw(&[9]);
        log.flush();
        let b = log.batches();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0], vec![1, 2, 3, 4, 5]);
        assert_eq!(b[1], vec![9]);
    }
}
