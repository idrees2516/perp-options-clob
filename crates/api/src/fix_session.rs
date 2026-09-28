//! FIX 4.4 session transport (G-26): sequence numbers, heartbeats,
//! resend, gap-fill, and a real TCP adapter over the wire codec.
//!
//! ## What this closes
//!
//! [`crate::fix`] shipped the deterministic protocol core — the
//! tag=value codec and the typed application subset. What it
//! deliberately was not: a *session*. A FIX session is a state machine
//! with real obligations, and institutional desks judge a venue by how
//! it behaves when the wire misbehaves:
//!
//! * **Sequence integrity** — every message carries `34=SeqNum`;
//!   the receiver expects exactly the previous + 1. Anything else is a
//!   *gap* (triggering a ResendRequest) or a *stale message*
//!   (PossDup-tagged replays are dropped silently; untagged regressions
//!   are answered with a SequenceReset-GapFill resync).
//! * **Resend** — the session stores its outbound application messages
//!   and replays any requested range with `43=Y` (PossDupFlag),
//!   replacing administrative messages inside the range with
//!   SequenceReset-GapFill — the FIX 4.4 recovery choreography.
//! * **Liveness** — silence past two heartbeat intervals triggers a
//!   TestRequest; an unanswered TestRequest disconnects the session.
//! * **Framing** — FIX messages end at the `10=XXX<SOH>` checksum;
//!   the framing layer reassembles split TCP segments and never hands
//!   a partial message to the codec.
//!
//! The session is transport-agnostic behind the [`Wire`] trait: an
//! in-memory duplex pair for deterministic tests, and [`TcpWire`] (a
//! non-blocking `TcpStream`) for deployment. Time enters only through
//! the explicit `now_ms` parameter of [`FixSession::pump`] — the same
//! determinism discipline the engine uses, so a session's entire
//! behaviour is a pure function of (bytes in, clock).
//!
//! ## Session policy summary
//!
//! | Inbound | Rule |
//! |---------|------|
//! | `34 == expected` | deliver, advance |
//! | `34 > expected`  | ResendRequest(7=expected, 16=0), buffer until gap fills |
//! | `34 < expected`, `43=Y` | duplicate, drop |
//! | `34 < expected`, no flag | SequenceReset-GapFill(36=expected), drop |
//! | `0` Heartbeat | refresh liveness |
//! | `1` TestRequest | reply Heartbeat echoing `112` |
//! | `2` ResendRequest | replay stored range with `43=Y`, GapFill the rest |
//! | `4` SequenceReset `123=Y` | jump expected to `36` |
//! | `5` Logout | echo Logout, disconnect |
//!
//! Outbound administrative messages are never resent; their sequence
//! numbers are covered by GapFill — exactly the spec's intent that a
//! recovered stream contains *state changes*, not heartbeats.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use crate::fix::{CodecError, FixMessage, GatewayMessage};

/// Session-level errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixSessionError {
    /// Transport I/O failure.
    Io(String),
    /// A frame failed codec validation (length/checksum/grammar).
    Decode(CodecError),
    /// The peer violated the session protocol irrecoverably.
    Protocol(String),
    /// The framing buffer exceeded the configured cap.
    OversizedFrame,
}

impl std::fmt::Display for FixSessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FixSessionError::Io(e) => write!(f, "transport i/o: {e}"),
            FixSessionError::Decode(e) => write!(f, "codec: {e:?}"),
            FixSessionError::Protocol(e) => write!(f, "protocol: {e}"),
            FixSessionError::OversizedFrame => write!(f, "frame exceeds session cap"),
        }
    }
}

impl std::error::Error for FixSessionError {}

impl From<CodecError> for FixSessionError {
    fn from(e: CodecError) -> Self {
        FixSessionError::Decode(e)
    }
}

/// The byte transport a session runs over.
pub trait Wire {
    /// Read whatever is available into `buf`; `Ok(0)` means nothing
    /// pending (never blocks).
    fn try_read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Write the whole buffer.
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
}

impl Wire for TcpStream {
    fn try_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // A non-blocking read: WouldBlock surfaces as Ok(0) semantics.
        match Read::read(self, buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        Write::write_all(self, buf)
    }
}

/// A non-blocking TCP wire (the deployment transport).
#[derive(Debug)]
pub struct TcpWire {
    stream: TcpStream,
}

impl TcpWire {
    /// Wrap a connected stream; the read side is set non-blocking.
    ///
    /// # Errors
    /// Propagates `set_nonblocking` failures.
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self { stream })
    }

    /// The underlying stream (for `try_clone` on accept loops).
    #[must_use]
    pub fn into_inner(self) -> TcpStream {
        self.stream
    }
}

impl Wire for TcpWire {
    fn try_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Wire::try_read(&mut self.stream, buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        Write::write_all(&mut self.stream, buf)
    }
}

/// One end of an in-memory duplex pair (the deterministic test
/// transport): writes land in the peer's inbox, reads drain this end's
/// inbox.
#[derive(Debug, Clone)]
pub struct LoopbackWire {
    inbox: Arc<Mutex<Vec<u8>>>,
    outbox: Arc<Mutex<Vec<u8>>>,
}

/// A connected pair of loopback wires.
#[derive(Debug, Clone)]
pub struct LoopbackPair {
    /// Wire A.
    pub a: LoopbackWire,
    /// Wire B (A's writes arrive here, and vice versa).
    pub b: LoopbackWire,
}

impl LoopbackPair {
    /// A fresh pair.
    #[must_use]
    pub fn new() -> Self {
        let inbox_a = Arc::new(Mutex::new(Vec::new()));
        let inbox_b = Arc::new(Mutex::new(Vec::new()));
        let a = LoopbackWire {
            inbox: Arc::clone(&inbox_a),
            outbox: Arc::clone(&inbox_b),
        };
        let b = LoopbackWire {
            inbox: Arc::clone(&inbox_b),
            outbox: Arc::clone(&inbox_a),
        };
        Self { a, b }
    }
}

impl Default for LoopbackPair {
    fn default() -> Self {
        Self::new()
    }
}

impl Wire for LoopbackWire {
    fn try_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut inbox = self
            .inbox
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?;
        let n = inbox.len().min(buf.len());
        buf[..n].copy_from_slice(&inbox[..n]);
        inbox.drain(..n);
        Ok(n)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let mut outbox = self
            .outbox
            .lock()
            .map_err(|_| io::Error::other("poisoned"))?;
        outbox.extend_from_slice(buf);
        Ok(())
    }
}

/// FIX frame reassembly: accumulate bytes, extract complete messages
/// ending at `10=XXX<SOH>`.
#[derive(Debug, Default)]
pub struct FixFraming {
    buf: Vec<u8>,
    cap: usize,
}

const DEFAULT_FRAME_CAP: usize = 1 << 16; // 64 KiB — the largest FIX message we accept

impl FixFraming {
    /// A framer with the default 64 KiB cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            cap: DEFAULT_FRAME_CAP,
        }
    }

    /// Feed raw bytes; returns every complete message in arrival order.
    ///
    /// # Errors
    /// [`FixSessionError::OversizedFrame`] when the buffer would
    /// exceed the cap without containing a terminator.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<String>, FixSessionError> {
        self.buf.extend_from_slice(data);
        if self.buf.len() > self.cap {
            return Err(FixSessionError::OversizedFrame);
        }
        let mut out = Vec::new();
        while let Some(end) = find_message_end(&self.buf) {
            let frame: Vec<u8> = self.buf.drain(..=end).collect();
            // FIX is ASCII on the wire; our subset is strict UTF-8.
            let text = String::from_utf8_lossy(&frame).into_owned();
            out.push(text);
        }
        Ok(out)
    }

    /// Bytes buffered awaiting completion (diagnostics).
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// Find the index of the SOH that terminates a checksum field ending a
/// complete message: the pattern `SOH 10=DDD SOH` where DDD is exactly
/// three digits.
fn find_message_end(buf: &[u8]) -> Option<usize> {
    let soh = 0x01_u8;
    let mut i = 0;
    while i + 7 < buf.len() {
        // Look for SOH '1' '0' '=' d d d SOH
        if buf[i] == soh
            && buf[i + 1] == b'1'
            && buf[i + 2] == b'0'
            && buf[i + 3] == b'='
            && buf[i + 4].is_ascii_digit()
            && buf[i + 5].is_ascii_digit()
            && buf[i + 6].is_ascii_digit()
            && buf[i + 7] == soh
        {
            return Some(i + 7);
        }
        i += 1;
    }
    None
}

/// One delivered pump result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixEvent {
    /// An application message, sequence-validated and deduplicated.
    App(GatewayMessage),
    /// A heartbeat was sent (liveness duty).
    HeartbeatSent,
    /// A TestRequest was sent (peer went quiet).
    TestRequested,
    /// A ResendRequest was sent (inbound gap detected).
    ResendRequested {
        /// First missing sequence number.
        from: u64,
    },
    /// A stored range was replayed to the peer with PossDup.
    RangeResent {
        /// First replayed sequence number.
        from: u64,
        /// Last replayed sequence number.
        to: u64,
    },
    /// A GapFill covered sequence numbers we do not store.
    GapFilled {
        /// The sequence the peer should resume from.
        new_seq: u64,
    },
    /// A stale untagged message was answered with a resync.
    StaleReset {
        /// The sequence we told the peer to jump to.
        new_seq: u64,
    },
    /// A PossDup-tagged duplicate was dropped.
    DuplicateDropped,
    /// The session ended.
    Disconnected {
        /// Why.
        reason: DisconnectReason,
    },
}

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Clean logout exchange.
    Logout,
    /// Unanswered TestRequest past the deadline.
    HeartbeatTimeout,
    /// Unrecoverable protocol violation.
    Protocol(String),
}

/// A stored outbound message available for resend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    /// The sequence number it was sent with.
    pub seq: u64,
    /// Whether it is an application message (resent) or
    /// administrative (gap-filled, never resent).
    pub administrative: bool,
    /// The complete encoded wire form, checksum included.
    pub wire: String,
}

/// The FIX session state machine over any [`Wire`].
pub struct FixSession<W: Wire> {
    transport: W,
    framing: FixFraming,
    role: Role,
    logged_on: bool,
    /// Next sequence number we expect from the peer.
    inbound_seq: u64,
    /// Next sequence number we will assign.
    outbound_seq: u64,
    heartbeat_s: u32,
    /// Wall-clock of the last inbound frame (ms).
    last_rx_ms: u64,
    /// Outstanding TestRequest: (test_req_id, deadline_ms).
    test_req: Option<(u64, u64)>,
    /// Next TestReqID (deterministic counter).
    next_test_id: u64,
    /// Outbound message store, oldest first (bounded).
    store: VecDeque<StoredMessage>,
    store_limit: usize,
    /// Messages received ahead of the gap, pending delivery
    /// (seq, message).
    pending: Vec<(u64, FixMessage)>,
    /// Set when the session should no longer pump.
    closed: bool,
}

/// Which side of the logon handshake we play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// We initiate (client/desk).
    Initiator,
    /// We accept and echo Logon (venue gateway).
    Acceptor,
}

impl<W: Wire> FixSession<W> {
    /// A session over `transport` in `role`, sequence numbers at 1.
    #[must_use]
    pub fn new(transport: W, role: Role) -> Self {
        Self {
            transport,
            framing: FixFraming::new(),
            role,
            logged_on: false,
            inbound_seq: 1,
            outbound_seq: 1,
            heartbeat_s: 30,
            last_rx_ms: 0,
            test_req: None,
            next_test_id: 1,
            store: VecDeque::new(),
            store_limit: 4_096,
            pending: Vec::new(),
            closed: false,
        }
    }

    /// Whether the logon handshake completed.
    #[must_use]
    pub fn is_logged_on(&self) -> bool {
        self.logged_on && !self.closed
    }

    /// The next expected inbound sequence number.
    #[must_use]
    pub fn expected_inbound_seq(&self) -> u64 {
        self.inbound_seq
    }

    /// Send the initial Logon (initiator role).
    ///
    /// # Errors
    /// Transport or protocol errors.
    pub fn logon(&mut self, heartbeat_s: u32) -> Result<(), FixSessionError> {
        let mut msg = FixMessage::new();
        msg.set(35, "A").set(108, heartbeat_s.to_string());
        self.heartbeat_s = heartbeat_s;
        self.send(&msg)
    }

    /// Send Logout and mark the session closed locally.
    ///
    /// # Errors
    /// Transport errors.
    pub fn logout(&mut self) -> Result<(), FixSessionError> {
        let mut msg = FixMessage::new();
        msg.set(35, "5");
        let result = self.send(&msg);
        self.closed = true;
        result
    }

    /// Send an application message with the next outbound sequence.
    ///
    /// # Errors
    /// Transport errors; sending on a closed session.
    pub fn send(&mut self, msg: &FixMessage) -> Result<(), FixSessionError> {
        if self.closed {
            return Err(FixSessionError::Protocol("session closed".into()));
        }
        let seq = self.outbound_seq;
        self.outbound_seq = self.outbound_seq.saturating_add(1);
        let mut with_seq = FixMessage {
            fields: msg.fields.clone(),
        };
        with_seq.set(34, seq.to_string());
        let administrative = Self::is_administrative(&with_seq);
        let wire = with_seq.encode();
        self.transport
            .write_all(wire.as_bytes())
            .map_err(|e| FixSessionError::Io(e.to_string()))?;
        // Store for resend (bounded, oldest evicted).
        if self.store.len() >= self.store_limit {
            self.store.pop_front();
        }
        self.store.push_back(StoredMessage {
            seq,
            administrative,
            wire,
        });
        Ok(())
    }

    /// Pump the session: drain readable bytes, frame and process every
    /// complete message, then run liveness policy. Returns the events
    /// produced, in order.
    ///
    /// `now_ms` is the caller's clock — the session is deterministic
    /// in (bytes, clock).
    ///
    /// # Errors
    /// Transport, framing, and codec failures.
    pub fn pump(&mut self, now_ms: u64) -> Result<Vec<FixEvent>, FixSessionError> {
        if self.closed {
            return Ok(vec![FixEvent::Disconnected {
                reason: DisconnectReason::Logout,
            }]);
        }
        let mut events = Vec::new();
        // 1. Drain the wire.
        let mut scratch = [0_u8; 8_192];
        loop {
            let n = self
                .transport
                .try_read(&mut scratch)
                .map_err(|e| FixSessionError::Io(e.to_string()))?;
            if n == 0 {
                break;
            }
            for text in self.framing.feed(&scratch[..n])? {
                let msg = FixMessage::decode(&text)?;
                if let Some(mut ev) = self.handle_inbound(msg, now_ms) {
                    events.append(&mut ev);
                }
            }
            if n < scratch.len() {
                break; // short read: drained for now
            }
        }
        // 2. Liveness.
        if self.logged_on && self.heartbeat_s > 0 {
            let hb_ms = u64::from(self.heartbeat_s) * 1_000;
            let silent = now_ms.saturating_sub(self.last_rx_ms);
            if let Some((_, deadline)) = self.test_req {
                if now_ms >= deadline {
                    self.closed = true;
                    events.push(FixEvent::Disconnected {
                        reason: DisconnectReason::HeartbeatTimeout,
                    });
                }
            } else if silent > hb_ms.saturating_mul(2) {
                let id = self.next_test_id;
                self.next_test_id = self.next_test_id.saturating_add(1);
                let mut req = FixMessage::new();
                req.set(35, "1").set(112, id.to_string());
                self.send(&req)?;
                self.test_req = Some((id, now_ms.saturating_add(hb_ms)));
                events.push(FixEvent::TestRequested);
            }
        }
        Ok(events)
    }

    /// Whether a message type is administrative (never resent on
    /// recovery; its sequence numbers are gap-filled instead).
    fn is_administrative(msg: &FixMessage) -> bool {
        matches!(
            msg.get(35),
            Some("0") | Some("1") | Some("2") | Some("4") | Some("5") | Some("A")
        )
    }

    /// Sequence-validate and process one complete inbound message.
    fn handle_inbound(&mut self, msg: FixMessage, now_ms: u64) -> Option<Vec<FixEvent>> {
        let mut events = Vec::new();
        self.last_rx_ms = now_ms;
        self.test_req = None; // any inbound proves liveness

        let Some(seq_str) = msg.get(34) else {
            self.closed = true;
            events.push(FixEvent::Disconnected {
                reason: DisconnectReason::Protocol("missing 34".into()),
            });
            return Some(events);
        };
        let Ok(seq) = seq_str.parse::<u64>() else {
            self.closed = true;
            events.push(FixEvent::Disconnected {
                reason: DisconnectReason::Protocol(format!("bad 34: {seq_str}")),
            });
            return Some(events);
        };
        let poss_dup = msg.get(43) == Some("Y");
        let msg_type = msg.get(35).unwrap_or("").to_string();

        // Sequence dispatch.
        if seq == self.inbound_seq {
            self.inbound_seq = self.inbound_seq.saturating_add(1);
            self.deliver(msg_type, msg, &mut events);
            // Deliver anything buffered behind the gap that is now
            // filled.
            self.flush_pending(&mut events);
        } else if seq > self.inbound_seq {
            // Gap: request the missing range, buffer this one.
            let from = self.inbound_seq;
            let mut req = FixMessage::new();
            req.set(35, "2").set(7, from.to_string()).set(16, "0");
            if self.send(&req).is_err() {
                self.closed = true;
            }
            events.push(FixEvent::ResendRequested { from });
            if Self::is_administrative(&msg) {
                // Heartbeats during a gap are consumed; they cannot
                // fill it. ResendRequests during a gap are honored
                // (recovery is bidirectional).
                if msg_type == "2" {
                    self.handle_resend_request(&msg, &mut events);
                }
            } else {
                self.pending.push((seq, msg));
            }
        } else {
            // Stale: seq < expected.
            if poss_dup {
                events.push(FixEvent::DuplicateDropped);
            } else {
                let new_seq = self.inbound_seq;
                let mut reset = FixMessage::new();
                reset
                    .set(35, "4")
                    .set(123, "Y")
                    .set(34, new_seq.to_string())
                    .set(36, new_seq.to_string());
                // Carries the *current expected* seq (not a fresh
                // counter) so the peer can resync without a new gap.
                let _ = self.send_stored(new_seq, &reset);
                events.push(FixEvent::StaleReset { new_seq });
            }
        }
        Some(events)
    }

    /// Deliver one in-sequence message by type.
    fn deliver(&mut self, msg_type: String, msg: FixMessage, events: &mut Vec<FixEvent>) {
        match msg_type.as_str() {
            "A" => {
                // Logon: adopt the peer's requested heartbeat interval
                // (the initiator's 108 governs the session — the
                // standard convention) and acceptors echo the Logon.
                let requested = msg
                    .get(108)
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(30);
                self.heartbeat_s = requested;
                self.logged_on = true;
                if self.role == Role::Acceptor {
                    let mut echo = FixMessage::new();
                    echo.set(35, "A").set(108, self.heartbeat_s.to_string());
                    let _ = self.send(&echo);
                }
            }
            "0" => {} // Heartbeat: liveness already refreshed.
            "1" => {
                // TestRequest: answer with Heartbeat echoing 112.
                let mut hb = FixMessage::new();
                hb.set(35, "0");
                if let Some(id) = msg.get(112) {
                    hb.set(112, id.to_string());
                }
                let _ = self.send(&hb);
                events.push(FixEvent::HeartbeatSent);
            }
            "2" => self.handle_resend_request(&msg, events),
            "4" => {
                // SequenceReset: honor GapFill jumps (36=NewSeqNo).
                if msg.get(123) == Some("Y") {
                    if let Some(new_seq) = msg.get(36).and_then(|v| v.parse::<u64>().ok()) {
                        if new_seq > self.inbound_seq {
                            self.inbound_seq = new_seq;
                            events.push(FixEvent::GapFilled { new_seq });
                            self.flush_pending(events);
                        }
                    }
                }
            }
            "5" => {
                // Logout: echo and disconnect cleanly.
                let mut bye = FixMessage::new();
                bye.set(35, "5");
                let _ = self.send(&bye);
                self.closed = true;
                events.push(FixEvent::Disconnected {
                    reason: DisconnectReason::Logout,
                });
            }
            _ => {
                // Application: decode the typed layer and hand over.
                if let Ok(app) = GatewayMessage::from_fix(&msg) {
                    events.push(FixEvent::App(app));
                }
            }
        }
    }

    /// The peer asked us to replay [BeginSeqNo(7), EndSeqNo(16)].
    ///
    /// The FIX 4.4 recovery choreography: application messages are
    /// replayed on the wire with their *original* sequence numbers
    /// (PossDupFlag `43=Y` so the peer knows they are replays);
    /// administrative messages inside the range are *never* resent —
    /// each admin run is covered by one SequenceReset-GapFill whose
    /// `34` is the run's first seq and whose `36` is the seq the peer
    /// should resume consuming at. Ranges evicted from the bounded
    /// store are covered by a leading GapFill to the oldest retained
    /// message.
    fn handle_resend_request(&mut self, msg: &FixMessage, events: &mut Vec<FixEvent>) {
        let Some(from) = msg.get(7).and_then(|v| v.parse::<u64>().ok()) else {
            return;
        };
        // 0 (or absent) = "everything to the present" per the spec's
        // infinity convention.
        let to = msg
            .get(16)
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|t| *t > 0)
            .unwrap_or(self.outbound_seq.saturating_sub(1));
        // Plan the whole response first (immutable borrow of the
        // store), then execute the writes — keeps the borrow checker
        // honest without cloning the store.
        enum Action {
            GapFill { seq: u64, resume_at: u64 },
            Resend { wire: String },
        }
        let mut actions: Vec<Action> = Vec::new();
        let mut resent_from = None;
        let mut resent_to = None;
        // Evicted leading range: GapFill from `from` to the oldest
        // retained message (the peer must not re-request what we can
        // no longer produce).
        if let Some(oldest) = self.store.front().map(|s| s.seq) {
            if oldest > from {
                actions.push(Action::GapFill {
                    seq: from,
                    resume_at: oldest,
                });
                events.push(FixEvent::GapFilled { new_seq: oldest });
            }
        }
        let mut admin_run_start: Option<u64> = None;
        for stored in &self.store {
            if stored.seq < from || stored.seq > to {
                continue;
            }
            if stored.administrative {
                if admin_run_start.is_none() {
                    admin_run_start = Some(stored.seq);
                }
                continue;
            }
            // App message: close the admin run that precedes it,
            // resuming at this message's seq.
            if let Some(start) = admin_run_start.take() {
                actions.push(Action::GapFill {
                    seq: start,
                    resume_at: stored.seq,
                });
                events.push(FixEvent::GapFilled {
                    new_seq: stored.seq,
                });
            }
            let Ok(mut re) = FixMessage::decode(&stored.wire) else {
                continue;
            };
            re.set(43, "Y");
            if re.get(122).is_none() {
                re.set(122, "0");
            }
            let seq = stored.seq;
            actions.push(Action::Resend { wire: re.encode() });
            if resent_from.is_none() {
                resent_from = Some(seq);
            }
            resent_to = Some(seq);
        }
        // Trailing admin run: resume at the next *new* outbound seq.
        if let Some(start) = admin_run_start.take() {
            let resume = self.outbound_seq;
            actions.push(Action::GapFill {
                seq: start,
                resume_at: resume,
            });
            events.push(FixEvent::GapFilled { new_seq: resume });
        }
        // Execute.
        for action in actions {
            match action {
                Action::GapFill { seq, resume_at } => {
                    let mut gf = FixMessage::new();
                    gf.set(35, "4")
                        .set(123, "Y")
                        .set(34, seq.to_string())
                        .set(36, resume_at.to_string());
                    let _ = self.send_stored(seq, &gf);
                }
                Action::Resend { wire } => {
                    let _ = self.transport.write_all(wire.as_bytes());
                }
            }
        }
        if let (Some(f), Some(t)) = (resent_from, resent_to) {
            events.push(FixEvent::RangeResent { from: f, to: t });
        }
    }

    /// Send a replayed message with its *original* sequence number
    /// (bypasses the outbound counter; does not re-store).
    fn send_stored(&mut self, seq: u64, msg: &FixMessage) -> Result<(), FixSessionError> {
        let wire = msg.encode();
        self.transport
            .write_all(wire.as_bytes())
            .map_err(|e| FixSessionError::Io(e.to_string()))?;
        let _ = seq;
        Ok(())
    }

    /// Deliver buffered messages that became in-sequence after a gap
    /// fill or SequenceReset jump.
    fn flush_pending(&mut self, events: &mut Vec<FixEvent>) {
        if self.pending.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].0 == self.inbound_seq {
                let (_, msg) = self.pending.remove(i);
                let msg_type = msg.get(35).unwrap_or("").to_string();
                self.inbound_seq = self.inbound_seq.saturating_add(1);
                self.deliver(msg_type, msg, events);
            } else {
                i += 1;
            }
        }
    }
}

/// Serve FIX sessions over TCP: one thread per connection, each a
/// fresh [`FixSession`] in [`Role::Acceptor`] pumping at a 20 ms
/// cadence and forwarding application messages to `handler`.
///
/// This is the deployment shape: the handler owns the engine side
/// (translate `GatewayMessage` to commands, push `ExecutionReport`s
/// back out). Returns the bound address; drop the returned join
/// handle's keep-alive by dropping the listener is not required —
/// the accept loop ends with the process.
///
/// # Errors
/// Binding failures.
pub fn serve_fix<H>(addr: &str, handler: H) -> io::Result<std::net::SocketAddr>
where
    H: FnMut(&mut FixSession<TcpWire>, GatewayMessage) + Send + 'static,
{
    let listener = std::net::TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    let handler = Arc::new(Mutex::new(handler));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Ok(wire) = TcpWire::new(stream) else {
                continue;
            };
            let handler = Arc::clone(&handler);
            std::thread::spawn(move || {
                let mut session = FixSession::new(wire, Role::Acceptor);
                let mut tick: u64 = 0;
                loop {
                    tick += 1;
                    match session.pump(tick * 100) {
                        Ok(events) => {
                            for event in events {
                                if let FixEvent::App(app) = event {
                                    if let Ok(mut guard) = handler.lock() {
                                        guard(&mut session, app);
                                    }
                                }
                            }
                        }
                        Err(_) => break,
                    }
                    if !session.is_logged_on() && tick > 1 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                // Final drain after logout.
                let _ = session.pump(tick * 100 + 1);
            });
        }
    });
    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::GatewayMessage;
    use poc_core::{OrderType, Side, TimeInForce};

    fn client_server() -> (FixSession<LoopbackWire>, FixSession<LoopbackWire>) {
        let pair = LoopbackPair::new();
        let client = FixSession::new(pair.a, Role::Initiator);
        let server = FixSession::new(pair.b, Role::Acceptor);
        (client, server)
    }

    fn logon(client: &mut FixSession<LoopbackWire>, server: &mut FixSession<LoopbackWire>) {
        client.logon(30).unwrap();
        let ev = server.pump(100).unwrap();
        assert!(server.is_logged_on(), "server logged on: {ev:?}");
        let ev = client.pump(200).unwrap();
        assert!(client.is_logged_on(), "client logged on: {ev:?}");
    }

    #[test]
    fn framing_reassembles_split_messages() {
        let mut framer = FixFraming::new();
        let mut m = FixMessage::new();
        m.set(35, "A").set(108, "30");
        let wire = m.encode();
        // Feed the message one byte at a time.
        let mut collected = Vec::new();
        for byte in wire.as_bytes() {
            collected.extend(framer.feed(&[*byte]).unwrap());
        }
        assert_eq!(collected.len(), 1);
        assert!(FixMessage::decode(&collected[0]).is_ok());
        // Two messages in one write.
        let mut collected = framer.feed(format!("{wire}{wire}").as_bytes()).unwrap();
        assert_eq!(collected.len(), 2);
        let _ = &mut collected;
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut framer = FixFraming::new();
        let junk = vec![b'x'; 70_000];
        assert_eq!(
            framer.feed(&junk).unwrap_err(),
            FixSessionError::OversizedFrame
        );
    }

    #[test]
    fn logon_handshake_and_heartbeat_adoption() {
        let (mut client, mut server) = client_server();
        assert!(!client.is_logged_on());
        logon(&mut client, &mut server);
        // The initiator's requested interval governs both sides.
        assert_eq!(server.heartbeat_s, 30);
        assert_eq!(client.heartbeat_s, 30);
        // A tighter request is adopted verbatim.
        let pair = LoopbackPair::new();
        let mut c2 = FixSession::new(pair.a, Role::Initiator);
        let mut s2 = FixSession::new(pair.b, Role::Acceptor);
        c2.logon(5).unwrap();
        let _ = s2.pump(1_000).unwrap();
        assert_eq!(s2.heartbeat_s, 5);
        // Client heartbeats flow and refresh liveness.
        let mut hb = FixMessage::new();
        hb.set(35, "0");
        client.send(&hb).unwrap();
        let events = server.pump(300).unwrap();
        assert!(!events
            .iter()
            .any(|e| matches!(e, FixEvent::Disconnected { .. })));
    }

    #[test]
    fn application_message_round_trip() {
        let (mut client, mut server) = client_server();
        logon(&mut client, &mut server);
        let mut order = FixMessage::new();
        order
            .set(35, "D")
            .set(49, "7")
            .set(55, "BTC-PERP")
            .set(54, "1")
            .set(38, "5")
            .set(44, "79900")
            .set(40, "3")
            .set(11, "ord-1");
        client.send(&order).unwrap();
        let events = server.pump(400).unwrap();
        let app = events
            .iter()
            .find_map(|e| match e {
                FixEvent::App(GatewayMessage::NewOrderSingle { request, .. }) => Some(request),
                _ => None,
            })
            .expect("order delivered");
        assert_eq!(app.subaccount, 7);
        assert_eq!(app.symbol, "BTC-PERP");
        assert_eq!(app.side, Side::Bid);
        assert_eq!(app.qty_lots, 5);
        assert_eq!(app.order_type, OrderType::Limit);
        assert_eq!(app.tif, TimeInForce::Gtc);
    }

    #[test]
    fn inbound_gap_triggers_resend_and_buffered_delivery() {
        // Raw server wire (no session): we control sequence numbers
        // directly to create a real gap on the client's inbound stream.
        let pair = LoopbackPair::new();
        let mut client = FixSession::new(pair.a, Role::Initiator);
        let mut server_wire = pair.b;
        // Client logs on; the raw side answers with a Logon echo.
        client.logon(1).unwrap();
        let mut echo = FixMessage::new();
        echo.set(35, "A").set(108, "1").set(34, "1");
        server_wire.write_all(echo.encode().as_bytes()).unwrap();
        let events = client.pump(100).unwrap();
        assert!(client.is_logged_on(), "logon: {events:?}");
        assert_eq!(client.expected_inbound_seq(), 2);
        // Server sends app seq 2, then seq 4 (gap at 3).
        let mut m1 = FixMessage::new();
        m1.set(35, "H").set(11, "q1").set(34, "2");
        let mut m3 = FixMessage::new();
        m3.set(35, "H").set(11, "q3").set(34, "4");
        server_wire.write_all(m1.encode().as_bytes()).unwrap();
        server_wire.write_all(m3.encode().as_bytes()).unwrap();
        let events = client.pump(200).unwrap();
        // Gap detected at 3: resend requested, seq-4 message buffered.
        assert!(events
            .iter()
            .any(|e| matches!(e, FixEvent::ResendRequested { from: 3 })));
        assert_eq!(client.expected_inbound_seq(), 3);
        // One app message delivered (seq 2), one buffered (seq 4).
        let delivered = events
            .iter()
            .filter(|e| matches!(e, FixEvent::App(GatewayMessage::OrderStatusRequest { .. })))
            .count();
        assert_eq!(delivered, 1);
        // The missing seq 3 arrives: buffered 4 must flush in order.
        let mut m2 = FixMessage::new();
        m2.set(35, "H").set(11, "q2").set(34, "3");
        server_wire.write_all(m2.encode().as_bytes()).unwrap();
        let events = client.pump(300).unwrap();
        let delivered = events
            .iter()
            .filter(|e| matches!(e, FixEvent::App(GatewayMessage::OrderStatusRequest { .. })))
            .count();
        assert_eq!(delivered, 2, "seq 3 + buffered seq 4 both deliver");
        assert_eq!(client.expected_inbound_seq(), 5);
    }

    #[test]
    fn poss_dup_duplicates_dropped() {
        let (mut client, mut server) = client_server();
        logon(&mut client, &mut server);
        let mut m = FixMessage::new();
        m.set(35, "0");
        // Send seq 5 directly with possdup semantics via the stored path.
        let mut stale = FixMessage::new();
        stale.set(35, "0").set(34, "1").set(43, "Y");
        let wire = stale.encode();
        // Inject into the server's inbound stream: write on the client
        // side (its outbox is the server's inbox).
        client.transport.write_all(wire.as_bytes()).unwrap();
        let _ = m;
        // Server has advanced past 1 (logon echo consumed it): seq 1
        // with 43=Y is a duplicate, dropped silently.
        let before = server.expected_inbound_seq();
        let events = server.pump(800).unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, FixEvent::DuplicateDropped)));
        assert_eq!(server.expected_inbound_seq(), before);
    }

    #[test]
    fn stale_untagged_gets_gapfill_resync() {
        let (mut client, mut server) = client_server();
        logon(&mut client, &mut server);
        // Client at expected 3 (logon + echo + ...). Send a stale seq 1
        // WITHOUT possdup: server answers with a SequenceReset.
        let _ = client;
        let mut stale = FixMessage::new();
        stale.set(35, "0").set(34, "1");
        let wire = stale.encode();
        // Bypass the client session's own send(): inject on the wire.
        client.transport.write_all(wire.as_bytes()).unwrap();
        let before = server.expected_inbound_seq();
        let events = server.pump(900).unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, FixEvent::StaleReset { .. })));
        assert_eq!(server.expected_inbound_seq(), before);
    }

    #[test]
    fn silence_triggers_test_request_then_disconnect() {
        let (mut client, mut server) = client_server();
        // Short heartbeat: 1s.
        client.logon(1).unwrap();
        let _ = server.pump(100).unwrap();
        let _ = client.pump(200).unwrap();
        assert!(server.is_logged_on());
        assert_eq!(server.heartbeat_s, 1);
        // Silence: pump far past 2 intervals.
        let events = server.pump(10_000).unwrap();
        assert!(events.iter().any(|e| matches!(e, FixEvent::TestRequested)));
        // Still silent at the deadline: disconnect.
        let events = server.pump(12_000).unwrap();
        assert!(matches!(
            events.last(),
            Some(FixEvent::Disconnected {
                reason: DisconnectReason::HeartbeatTimeout
            })
        ));
        assert!(!server.is_logged_on());
    }

    #[test]
    fn test_request_is_answered_with_heartbeat() {
        let (mut client, mut server) = client_server();
        logon(&mut client, &mut server);
        let mut tr = FixMessage::new();
        tr.set(35, "1").set(112, "42");
        server.send(&tr).unwrap();
        let events = client.pump(20_000).unwrap();
        assert!(events.iter().any(|e| matches!(e, FixEvent::HeartbeatSent)));
        // The answering heartbeat refreshes the server's liveness and
        // carries the echoed 112.
        let events = server.pump(20_100).unwrap();
        assert!(!events
            .iter()
            .any(|e| matches!(e, FixEvent::Disconnected { .. })));
        assert!(!events.iter().any(|e| matches!(e, FixEvent::TestRequested)));
    }

    #[test]
    fn logout_is_clean() {
        let (mut client, mut server) = client_server();
        logon(&mut client, &mut server);
        client.logout().unwrap();
        let events = server.pump(30_000).unwrap();
        assert!(matches!(
            events.last(),
            Some(FixEvent::Disconnected {
                reason: DisconnectReason::Logout
            })
        ));
        assert!(!server.is_logged_on());
    }

    #[test]
    fn sequence_reset_jump_honored() {
        let (mut client, mut server) = client_server();
        logon(&mut client, &mut server);
        // Inject a SequenceReset telling the server to jump its
        // expected inbound to 100.
        let mut reset = FixMessage::new();
        reset.set(35, "4").set(123, "Y").set(34, "2").set(36, "100");
        let wire = reset.encode();
        client.transport.write_all(wire.as_bytes()).unwrap();
        let events = server.pump(40_000).unwrap();
        assert!(events
            .iter()
            .any(|e| matches!(e, FixEvent::GapFilled { new_seq: 100 })));
        assert_eq!(server.expected_inbound_seq(), 100);
    }

    #[test]
    fn resend_replaces_admin_messages_with_gapfill() {
        let (mut client, mut server) = client_server();
        logon(&mut client, &mut server);
        // Server sends: heartbeat (admin), report (app), heartbeat
        // (admin), report (app) after the Logon echo (seq 1, admin).
        let mut hb = FixMessage::new();
        hb.set(35, "0");
        let mut rep1 = FixMessage::new();
        rep1.set(35, "H").set(11, "r1");
        let mut rep2 = FixMessage::new();
        rep2.set(35, "H").set(11, "r2");
        server.send(&hb).unwrap();
        server.send(&rep1).unwrap();
        server.send(&hb).unwrap();
        server.send(&rep2).unwrap();
        // Client consumes everything: echo(1), hb(2), rep1(3),
        // hb(4), rep2(5) -> expected 6.
        let _ = client.pump(50_000).unwrap();
        assert_eq!(client.expected_inbound_seq(), 6);
        // Client requests a full resend from 1.
        let mut req = FixMessage::new();
        req.set(35, "2").set(7, "1").set(16, "0");
        client.send(&req).unwrap();
        let events = server.pump(50_100).unwrap();
        // App messages resent with possdup: rep1 at 3, rep2 at 5.
        assert!(events
            .iter()
            .any(|e| matches!(e, FixEvent::RangeResent { from: 3, to: 5 })));
        // Admin runs (1..3 and 4..5) covered by GapFills.
        let gapfills: Vec<u64> = events
            .iter()
            .filter_map(|e| match e {
                FixEvent::GapFilled { new_seq } => Some(*new_seq),
                _ => None,
            })
            .collect();
        assert_eq!(gapfills, vec![3, 5]);
        // The client accepts the replay without a disconnect, and the
        // stale replays are dropped as duplicates (already consumed).
        let events = client.pump(50_200).unwrap();
        assert!(!events
            .iter()
            .any(|e| matches!(e, FixEvent::Disconnected { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, FixEvent::DuplicateDropped)));
        assert_eq!(client.expected_inbound_seq(), 6);
    }

    #[test]
    fn tcp_loopback_full_session() {
        // A real socket, a real logon, a real order, a real logout.
        let addr = serve_fix("127.0.0.1:0", |_session, msg| {
            // The handler sees the typed application layer.
            assert!(matches!(msg, GatewayMessage::NewOrderSingle { .. }));
        })
        .expect("bind");
        let stream = TcpStream::connect(addr).expect("connect");
        let wire = TcpWire::new(stream).expect("wire");
        let mut client = FixSession::new(wire, Role::Initiator);
        client.logon(1).expect("logon sent");
        // Pump until the acceptor's echo arrives.
        let mut logged_on = false;
        for tick in 0..200 {
            // Yield to the server thread between pumps: the session
            // runs on a 20 ms cadence in its own thread.
            std::thread::sleep(std::time::Duration::from_millis(10));
            let events = client.pump(tick * 50).expect("pump");
            if client.is_logged_on() {
                logged_on = true;
                let _ = events;
                break;
            }
        }
        assert!(logged_on, "client completed logon over TCP");
        let mut order = FixMessage::new();
        order
            .set(35, "D")
            .set(49, "1")
            .set(55, "BTC-PERP")
            .set(54, "2")
            .set(38, "1")
            .set(40, "1")
            .set(11, "tcp-1");
        client.send(&order).expect("order sent");
        // Give the server thread a moment, then log out cleanly.
        std::thread::sleep(std::time::Duration::from_millis(150));
        client.logout().expect("logout sent");
        let events = client.pump(10_000).expect("final pump");
        let _ = events;
        // The session closed locally by our own logout.
        assert!(!client.is_logged_on());
    }
}
