//! FIX session protocol: the logon/heartbeat/sequence-number machinery
//! that makes a FIX connection survive the real world — dropped frames,
//! restarts, and resends.
//!
//! # Why the session layer exists
//!
//! Every FIX-speaking desk on the planet runs infrastructure that
//! predates this venue by decades: sequence numbers in flat files,
//! store-and-forward resend, gap fill. Connecting to those systems is
//! not optional for a serious venue — the buy-side OMS cannot be
//! rewritten, so the venue must speak their dialect *exactly*. That is
//! why this layer implements the full state machine: `Logon`
//! handshake with `HeartBtInt` negotiation, `Heartbeat`/`TestRequest`
//! liveness, monotonic `MsgSeqNum(34)` in both directions,
//! `ResendRequest(2)` for inbound gaps, and store-and-forward resend of
//! application messages with `PossDupFlag(43)=Y` — with `SequenceReset`
//! `GapFill(123=Y)` collapsing admin-message ranges so a resend storm
//! never replays a day of heartbeats.
//!
//! # Determinism
//!
//! The session is a pure state machine: `handle(frame, now) -> Vec<
//! frame>` with no ambient clock and no I/O. The message store is a
//! `BTreeMap<u32, Vec<u8>>`; iteration order — and therefore gap-fill
//! boundaries — is fully deterministic.

use std::collections::BTreeMap;

use crate::wire::{tag, FixMessage, MsgType, WireError};

/// Session lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// TCP accepted; waiting for Logon.
    Connected,
    /// Logon received and answered; application traffic flows.
    Established,
    /// Logout exchanged; the connection is finished.
    Disconnected,
}

/// A session-level error worth reporting to the counterparty (or the
/// operator) instead of silently dropping the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// Wire-level failure.
    Wire(WireError),
    /// The message arrived before Logon.
    NotLoggedIn,
    /// Application traffic after Logout.
    SessionOver,
    /// A required header field was missing or unparsable.
    MissingHeader(&'static str),
    /// The message type is not one this acceptor speaks.
    UnsupportedMsgType(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(e) => write!(f, "wire error: {e}"),
            Self::NotLoggedIn => write!(f, "message before logon"),
            Self::SessionOver => write!(f, "message after logout"),
            Self::MissingHeader(t) => write!(f, "missing header field {t}"),
            Self::UnsupportedMsgType(t) => write!(f, "unsupported message type {t}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// One accepted FIX session (the venue is the acceptor).
///
/// All methods take `now` explicitly — no ambient clock — so replaying
/// the same inbound frames at the same timestamps reproduces the same
/// outbound bytes, bit for bit.
pub struct FixSession {
    /// Our CompID (the venue).
    pub our_comp_id: String,
    /// Their CompID (the desk).
    pub their_comp_id: String,
    /// Negotiated heartbeat interval, ms (from Logon's HeartBtInt).
    pub heartbeat_ms: u64,
    state: SessionState,
    /// Next inbound sequence number we expect.
    inbound_seq: u32,
    /// Next outbound sequence number we assign.
    outbound_seq: u32,
    /// Outbound application frames by sequence number (for resend).
    store: BTreeMap<u32, Vec<u8>>,
    /// Inbound frames that arrived ahead of sequence (the gap queue).
    pending: BTreeMap<u32, FixMessage>,
    /// TestRequest id issued for the current liveness probe.
    test_req_id: Option<String>,
}

/// What the session did with an inbound frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound<'m> {
    /// The frame was consumed at the session level only (logon,
    /// heartbeat, resend bookkeeping). Its application payload — if any —
    /// is handed back for the app layer.
    App(&'m FixMessage),
    /// The frame was consumed administratively (no app action needed).
    Admin,
    /// The frame was a future-sequence message that is queued pending
    /// the gap fill.
    Queued,
    /// The frame was a resent duplicate already processed (PossDup).
    Duplicate,
}

impl FixSession {
    /// New acceptor session for a counterparty.
    #[must_use]
    pub fn new(our_comp_id: impl Into<String>, their_comp_id: impl Into<String>) -> Self {
        Self {
            our_comp_id: our_comp_id.into(),
            their_comp_id: their_comp_id.into(),
            heartbeat_ms: 30_000,
            state: SessionState::Connected,
            inbound_seq: 1,
            outbound_seq: 1,
            store: BTreeMap::new(),
            pending: BTreeMap::new(),
            test_req_id: None,
        }
    }

    /// Current lifecycle state.
    #[must_use]
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Next expected inbound sequence number.
    #[must_use]
    pub fn expected_inbound_seq(&self) -> u32 {
        self.inbound_seq
    }

    /// Next outbound sequence number that will be assigned.
    #[must_use]
    pub fn next_outbound_seq(&self) -> u32 {
        self.outbound_seq
    }

    /// Build an outbound frame: stamps sequence numbers, CompIDs, and
    /// stores application messages for later resend.
    fn build(&mut self, msg_type: MsgType, body: FixMessage) -> Vec<u8> {
        let seq = self.outbound_seq;
        let bytes =
            Self::frame_with_seq(seq, &self.our_comp_id, &self.their_comp_id, msg_type, &body);
        // Store application messages for resend; admin messages are
        // collapsed into gap fills on resend.
        if !matches!(
            msg_type,
            MsgType::Heartbeat
                | MsgType::TestRequest
                | MsgType::ResendRequest
                | MsgType::Reject
                | MsgType::SequenceReset
                | MsgType::Logout
                | MsgType::Logon
        ) {
            self.store.insert(self.outbound_seq, bytes.clone());
        }
        self.outbound_seq = self.outbound_seq.saturating_add(1);
        bytes
    }

    /// Emit an application frame through the session: stamps sequence
    /// numbers and stores it for later resend. The gateway's only
    /// outbound path — every ExecutionReport and market-data snapshot
    /// rides through here so the desk's sequence stream stays exact.
    pub fn emit_out(&mut self, msg_type: MsgType, body: FixMessage) -> Vec<u8> {
        self.build(msg_type, body)
    }

    /// Build a frame with an EXPLICIT sequence number — used by resend
    /// machinery, where gap fills and re-emitted messages carry the
    /// numbers of the messages they replace, never fresh ones (FIX 4.4
    /// §4.3: sequence numbers in a resend stream are the original ones).
    fn frame_with_seq(
        seq: u32,
        sender: &str,
        target: &str,
        msg_type: MsgType,
        body: &FixMessage,
    ) -> Vec<u8> {
        let mut m = FixMessage::new();
        m.push(tag::MSG_SEQ_NUM, seq.to_string());
        m.push(tag::SENDER_COMP_ID, sender);
        m.push(tag::TARGET_COMP_ID, target);
        m.push(tag::SENDING_TIME, "20260101-00:00:00.000");
        m.push(tag::MSG_TYPE, msg_type.code());
        for &(t, ref v) in body.fields_raw() {
            m.push(t, v.clone());
        }
        m.encode()
    }

    /// Process one decoded inbound frame.
    ///
    /// Returns the app-level message (when it carries one), plus the
    /// administrative frames the session emits in response (Logon ack,
    /// heartbeats, resend batches, logout).
    pub fn handle<'m>(
        &mut self,
        msg: &'m FixMessage,
        _now: u64,
    ) -> Result<(Inbound<'m>, Vec<Vec<u8>>), SessionError> {
        // Header sanity: 34 required, 49/56 must match the session.
        let seq = msg
            .get_u64(tag::MSG_SEQ_NUM)
            .ok_or(SessionError::MissingHeader("34 MsgSeqNum"))?;
        let seq = u32::try_from(seq).map_err(|_| SessionError::MissingHeader("34"))?;
        let mtype = msg
            .msg_type()
            .ok_or(SessionError::MissingHeader("35 MsgType"))?;

        // Pre-logon: only Logon is acceptable.
        if self.state == SessionState::Connected && mtype != MsgType::Logon {
            return Err(SessionError::NotLoggedIn);
        }
        if self.state == SessionState::Disconnected {
            return Err(SessionError::SessionOver);
        }

        let mut out = Vec::new();

        // --- sequence handling -------------------------------------------
        let is_poss_dup = msg.get(tag::POSS_DUP_FLAG) == Some("Y");
        if seq < self.inbound_seq {
            // Too low: a duplicate of something we processed. With
            // PossDup it is ignored; without it, the counterparty has a
            // sequence-file problem — tell them where we are.
            if is_poss_dup {
                return Ok((Inbound::Duplicate, out));
            }
            let mut body = FixMessage::new();
            body.push(tag::TEXT, "msgseqnum too low");
            out.push(self.build(MsgType::Reject, body));
            return Ok((Inbound::Duplicate, out));
        }
        if seq > self.inbound_seq {
            // A gap: stash this frame and ask for the missing range
            // (BeginSeqNo(7) = what we expect, EndSeqNo(16) = 0 = all).
            self.pending.insert(seq, msg.clone());
            let mut rr = FixMessage::new();
            rr.push(7, self.inbound_seq.to_string());
            rr.push(16, "0");
            rr.push(tag::TEXT, "resend gap");
            out.push(self.build(MsgType::ResendRequest, rr));
            return Ok((Inbound::Queued, out));
        }

        // seq == expected: consume it.
        self.inbound_seq = self.inbound_seq.saturating_add(1);
        let app = self.dispatch(mtype, msg, &mut out)?;
        let mut results = out;

        // After consuming, drain any queued future frames.
        loop {
            let next = self.pending.keys().next().copied();
            match next {
                Some(s) if s == self.inbound_seq => {
                    let queued = self.pending.remove(&s).unwrap_or_default();
                    let inner = queued.msg_type().unwrap_or(MsgType::Other(0));
                    self.inbound_seq = self.inbound_seq.saturating_add(1);
                    let _ = self.dispatch(inner, &queued, &mut results);
                }
                _ => break,
            }
        }
        Ok((app, results))
    }

    /// Dispatch by message type once sequencing is settled.
    fn dispatch<'m>(
        &mut self,
        mtype: MsgType,
        msg: &'m FixMessage,
        out: &mut Vec<Vec<u8>>,
    ) -> Result<Inbound<'m>, SessionError> {
        match mtype {
            MsgType::Logon => {
                let hb = msg.get_u64(tag::HEART_BT_INT).unwrap_or(30);
                self.heartbeat_ms = hb.saturating_mul(1_000);
                // Acknowledge with our own Logon.
                let mut body = FixMessage::new();
                body.push(tag::ENCRYPT_METHOD, "0");
                body.push(tag::HEART_BT_INT, hb.to_string());
                out.push(self.build(MsgType::Logon, body));
                self.state = SessionState::Established;
                Ok(Inbound::Admin)
            }
            MsgType::Heartbeat => Ok(Inbound::Admin),
            MsgType::TestRequest => {
                // Answer with a Heartbeat echoing the TestReqID.
                let id = msg.get(tag::TEST_REQ_ID).unwrap_or("x").to_string();
                let mut body = FixMessage::new();
                body.push(tag::TEST_REQ_ID, id);
                out.push(self.build(MsgType::Heartbeat, body));
                Ok(Inbound::Admin)
            }
            MsgType::ResendRequest => {
                // The counterparty lost our messages: resend the
                // requested application range with PossDup, collapsing
                // admin gaps with SequenceReset GapFill.
                let from = msg.get_u64(7).unwrap_or(1);
                let to = msg.get_u64(16).unwrap_or(0); // 0 = through now
                let from = u32::try_from(from.max(1)).unwrap_or(1);
                let to = if to == 0 {
                    self.outbound_seq.saturating_sub(1)
                } else {
                    u32::try_from(to).unwrap_or(u32::MAX)
                };
                out.extend(self.resend_range(from, to));
                Ok(Inbound::Admin)
            }
            MsgType::SequenceReset => {
                // Honor a manual gap fill from the initiator.
                if let Some(new_seq) = msg.get_u64(tag::NEW_SEQ_NO) {
                    if let Ok(new_seq) = u32::try_from(new_seq) {
                        if new_seq > self.inbound_seq {
                            self.inbound_seq = new_seq;
                        }
                    }
                }
                Ok(Inbound::Admin)
            }
            MsgType::Logout => {
                let mut body = FixMessage::new();
                body.push(tag::TEXT, "logout acknowledged");
                out.push(self.build(MsgType::Logout, body));
                self.state = SessionState::Disconnected;
                Ok(Inbound::Admin)
            }
            MsgType::Reject => Ok(Inbound::Admin),
            // Application traffic: handed to the app layer.
            MsgType::NewOrderSingle
            | MsgType::OrderCancelRequest
            | MsgType::OrderCancelReplaceRequest
            | MsgType::OrderStatusRequest
            | MsgType::MarketDataRequest
            | MsgType::QuoteRequest
            | MsgType::Quote
            | MsgType::QuoteCancel
            | MsgType::QuoteStatusRequest => Ok(Inbound::App(msg)),
            other => Err(SessionError::UnsupportedMsgType(format!("{:?}", other))),
        }
    }

    /// Build the store-and-forward resend for `[from, to]`: stored
    /// application frames re-emitted with `PossDupFlag=Y` and their
    /// ORIGINAL sequence numbers, with `SequenceReset GapFill`
    /// covering the admin holes between them — never fresh numbers
    /// (FIX 4.4 §4.3).
    fn resend_range(&mut self, from: u32, to: u32) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut covered_to = from.saturating_sub(1);
        let sender = self.our_comp_id.clone();
        let target = self.their_comp_id.clone();
        let keys: Vec<u32> = self.store.range(from..=to).map(|(k, _)| *k).collect();
        for k in keys {
            // Admin hole before this app message? Gap-fill it.
            if k > covered_to.saturating_add(1) {
                let mut gf = FixMessage::new();
                gf.push(tag::POSS_DUP_FLAG, "Y");
                gf.push(tag::NEW_SEQ_NO, k.to_string());
                gf.push(tag::GAP_FILL_FLAG, "Y");
                out.push(Self::frame_with_seq(
                    covered_to.saturating_add(1),
                    &sender,
                    &target,
                    MsgType::SequenceReset,
                    &gf,
                ));
            }
            // Re-emit the application message with PossDup at its
            // original sequence number.
            if let Some(bytes) = self.store.get(&k).cloned() {
                out.push(self.stamp_poss_dup(&bytes));
            }
            covered_to = k;
        }
        // Trailing admin hole to `to`: one more gap fill.
        if to > covered_to {
            let mut gf = FixMessage::new();
            gf.push(tag::POSS_DUP_FLAG, "Y");
            gf.push(tag::NEW_SEQ_NO, to.saturating_add(1).to_string());
            gf.push(tag::GAP_FILL_FLAG, "Y");
            out.push(Self::frame_with_seq(
                covered_to.saturating_add(1),
                &sender,
                &target,
                MsgType::SequenceReset,
                &gf,
            ));
        }
        out
    }

    /// Re-encode a stored frame with `PossDupFlag(43)=Y`, preserving its
    /// original sequence number.
    fn stamp_poss_dup(&self, bytes: &[u8]) -> Vec<u8> {
        let Ok(mut msg) = FixMessage::decode(bytes) else {
            return bytes.to_vec();
        };
        msg.push(tag::POSS_DUP_FLAG, "Y");
        msg.encode()
    }

    /// Liveness: issue a TestRequest when `now` has advanced past the
    /// heartbeat interval since the last inbound traffic.
    pub fn poll(&mut self, now: u64, last_inbound: u64) -> Vec<Vec<u8>> {
        if self.state != SessionState::Established {
            return Vec::new();
        }
        if now.saturating_sub(last_inbound) < self.heartbeat_ms {
            return Vec::new();
        }
        if self.test_req_id.is_none() {
            self.test_req_id = Some(format!("probe-{now}"));
        }
        let mut body = FixMessage::new();
        body.push(
            tag::TEST_REQ_ID,
            self.test_req_id.clone().unwrap_or_else(|| "probe".into()),
        );
        vec![self.build(MsgType::TestRequest, body)]
    }

    /// Mark liveness: any inbound resets the probe state.
    pub fn note_inbound(&mut self) {
        self.test_req_id = None;
    }

    /// The stored application-message count (for tests and monitoring).
    #[must_use]
    pub fn stored_messages(&self) -> usize {
        self.store.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn logon(seq: u32) -> FixMessage {
        let mut m = FixMessage::new();
        m.push(tag::MSG_SEQ_NUM, seq.to_string());
        m.push(tag::SENDER_COMP_ID, "DESK");
        m.push(tag::TARGET_COMP_ID, "VENUE");
        m.push(tag::SENDING_TIME, "20260101-00:00:00.000");
        m.push(tag::MSG_TYPE, MsgType::Logon.code());
        m.push(tag::ENCRYPT_METHOD, "0");
        m.push(tag::HEART_BT_INT, "30");
        m
    }

    fn admin(seq: u32, mtype: MsgType) -> FixMessage {
        let mut m = FixMessage::new();
        m.push(tag::MSG_SEQ_NUM, seq.to_string());
        m.push(tag::SENDER_COMP_ID, "DESK");
        m.push(tag::TARGET_COMP_ID, "VENUE");
        m.push(tag::SENDING_TIME, "20260101-00:00:00.000");
        m.push(tag::MSG_TYPE, mtype.code());
        m
    }

    fn app_msg(seq: u32) -> FixMessage {
        let mut m = FixMessage::new();
        m.push(tag::MSG_SEQ_NUM, seq.to_string());
        m.push(tag::SENDER_COMP_ID, "DESK");
        m.push(tag::TARGET_COMP_ID, "VENUE");
        m.push(tag::SENDING_TIME, "20260101-00:00:00.000");
        m.push(tag::MSG_TYPE, MsgType::NewOrderSingle.code());
        m.push(tag::CL_ORD_ID, format!("order-{seq}"));
        m
    }

    // 1. logon handshake ------------------------------------------------------

    #[test]
    fn logon_handshake() {
        let mut s = FixSession::new("VENUE", "DESK");
        assert_eq!(s.state(), SessionState::Connected);
        let logon_msg = logon(1);
        let (inbound, out) = s.handle(&logon_msg, NOW).unwrap();
        assert_eq!(inbound, Inbound::Admin);
        assert_eq!(out.len(), 1);
        assert_eq!(s.state(), SessionState::Established);
        assert_eq!(s.heartbeat_ms, 30_000);
        // The ack carries sequence 1 and a Logon type.
        let ack = FixMessage::decode(&out[0]).unwrap();
        assert_eq!(ack.msg_type(), Some(MsgType::Logon));
        assert_eq!(ack.get_u64(tag::MSG_SEQ_NUM), Some(1));
        assert_eq!(s.next_outbound_seq(), 2);
    }

    // 2. pre-logon rejection ---------------------------------------------------

    #[test]
    fn rejects_traffic_before_logon() {
        let mut s = FixSession::new("VENUE", "DESK");
        assert_eq!(
            s.handle(&app_msg(1), NOW).unwrap_err(),
            SessionError::NotLoggedIn
        );
    }

    // 3. sequence gaps trigger resend requests --------------------------------

    #[test]
    fn gap_triggers_resend_request_and_queues() {
        let mut s = FixSession::new("VENUE", "DESK");
        s.handle(&logon(1), NOW).unwrap();

        // Inbound 3 before 2: queued + ResendRequest for 2.
        let m3 = app_msg(3);
        let (inbound, out) = s.handle(&m3, NOW).unwrap();
        assert_eq!(inbound, Inbound::Queued);
        assert_eq!(out.len(), 1);
        let rr = FixMessage::decode(&out[0]).unwrap();
        assert_eq!(rr.msg_type(), Some(MsgType::ResendRequest));
        assert_eq!(rr.get_u64(7), Some(2));

        // The gap fills: message 2 is consumed, then the queued 3
        // auto-drains (both hand back to the app layer through the
        // caller-visible result of THIS handle call... the drained one
        // is consumed administratively here).
        let m2 = app_msg(2);
        let (inbound, _) = s.handle(&m2, NOW).unwrap();
        assert!(matches!(inbound, Inbound::App(_)));
        assert_eq!(s.expected_inbound_seq(), 4);
    }

    // 4. duplicates are ignored ------------------------------------------------

    #[test]
    fn duplicates_ignored() {
        let mut s = FixSession::new("VENUE", "DESK");
        s.handle(&logon(1), NOW).unwrap();
        let m2 = app_msg(2);
        let (inbound, _) = s.handle(&m2, NOW).unwrap();
        assert!(matches!(inbound, Inbound::App(_)));
        // Straight duplicate (no PossDup): rejected-with-notice, seq
        // unmoved.
        let m2 = app_msg(2);
        let (inbound, out) = s.handle(&m2, NOW).unwrap();
        assert_eq!(inbound, Inbound::Duplicate);
        assert!(out
            .iter()
            .any(|b| FixMessage::decode(b).map(|m| m.msg_type()) == Ok(Some(MsgType::Reject))));
        // PossDup duplicate: silently ignored.
        let mut dup = app_msg(2);
        dup.push(tag::POSS_DUP_FLAG, "Y");
        let (inbound, out) = s.handle(&dup, NOW).unwrap();
        assert_eq!(inbound, Inbound::Duplicate);
        assert!(out.is_empty());
    }

    // 5. resend honors store-and-forward with gap fill ---------------------------

    #[test]
    fn resend_with_poss_dup_and_gap_fill() {
        let mut s = FixSession::new("VENUE", "DESK");
        s.handle(&logon(1), NOW).unwrap();

        // Simulate outbound traffic: app at 2 and 3, an admin heartbeat
        // at 4 (NOT stored), app again at 5.
        for _ in 0..2 {
            let mut m = FixMessage::new();
            m.push(tag::ORDER_ID, "x");
            let _ = s.build(MsgType::ExecutionReport, m);
        }
        let _ = s.build(MsgType::Heartbeat, FixMessage::new()); // seq 4
        let mut m5 = FixMessage::new();
        m5.push(tag::ORDER_ID, "y");
        let _ = s.build(MsgType::ExecutionReport, m5); // seq 5
        assert_eq!(s.stored_messages(), 3);
        assert_eq!(s.next_outbound_seq(), 6);

        // Counterparty asks for everything from 2. Their inbound sequence
        // stands at 2 (only the Logon was consumed on this side).
        let mut rr = admin(2, MsgType::ResendRequest);
        rr.push(7, "2");
        rr.push(16, "0");
        let (inbound, out) = s.handle(&rr, NOW).unwrap();
        assert_eq!(inbound, Inbound::Admin);
        // Two re-emitted app messages (2, 3), one gap fill covering 4,
        // then the app message at 5.
        let types: Vec<Option<MsgType>> = out
            .iter()
            .map(|b| FixMessage::decode(b).unwrap().msg_type())
            .collect();
        assert_eq!(
            types,
            vec![
                Some(MsgType::ExecutionReport), // 2 (PossDup)
                Some(MsgType::ExecutionReport), // 3 (PossDup)
                Some(MsgType::SequenceReset),   // gap fill to 5
                Some(MsgType::ExecutionReport), // 5 (PossDup)
            ]
        );
        // Gap fill carries NewSeqNo = 5 and GapFillFlag = Y.
        let gf = FixMessage::decode(&out[2]).unwrap();
        assert_eq!(gf.get_u64(tag::NEW_SEQ_NO), Some(5));
        assert_eq!(gf.get(tag::GAP_FILL_FLAG), Some("Y"));
        // Re-emitted frames carry PossDupFlag.
        for i in [0_usize, 1, 3] {
            let m = FixMessage::decode(&out[i]).unwrap();
            assert_eq!(m.get(tag::POSS_DUP_FLAG), Some("Y"));
        }
        // Resent frames carry their ORIGINAL sequence numbers, and the
        // gap fill carries the number of the first admin message it
        // replaces.
        let seqs: Vec<Option<u64>> = out
            .iter()
            .map(|b| FixMessage::decode(b).unwrap().get_u64(tag::MSG_SEQ_NUM))
            .collect();
        assert_eq!(seqs, vec![Some(2), Some(3), Some(4), Some(5)]);
        // Outbound numbering never regresses or consumes fresh numbers
        // for a resend.
        assert_eq!(s.next_outbound_seq(), 6);
    }

    // 6. test request is answered with a heartbeat --------------------------------

    #[test]
    fn test_request_answered() {
        let mut s = FixSession::new("VENUE", "DESK");
        s.handle(&logon(1), NOW).unwrap();
        let mut tr = admin(2, MsgType::TestRequest);
        tr.push(tag::TEST_REQ_ID, "ping-1");
        let (_, out) = s.handle(&tr, NOW).unwrap();
        assert_eq!(out.len(), 1);
        let hb = FixMessage::decode(&out[0]).unwrap();
        assert_eq!(hb.msg_type(), Some(MsgType::Heartbeat));
        assert_eq!(hb.get(tag::TEST_REQ_ID), Some("ping-1"));
    }

    // 7. liveness poll issues test requests ---------------------------------------

    #[test]
    fn liveness_poll() {
        let mut s = FixSession::new("VENUE", "DESK");
        s.handle(&logon(1), NOW).unwrap();
        assert!(s.poll(NOW, NOW).is_empty());
        let late = NOW + s.heartbeat_ms;
        let out = s.poll(late, NOW);
        assert_eq!(out.len(), 1);
        assert_eq!(
            FixMessage::decode(&out[0]).unwrap().msg_type(),
            Some(MsgType::TestRequest)
        );
        // After inbound traffic, the probe clears.
        s.note_inbound();
        assert!(s.poll(late, late).is_empty());
    }

    // 8. logout ---------------------------------------------------------------------

    #[test]
    fn logout_closes_the_session() {
        let mut s = FixSession::new("VENUE", "DESK");
        s.handle(&logon(1), NOW).unwrap();
        let (_, out) = s.handle(&admin(2, MsgType::Logout), NOW).unwrap();
        assert_eq!(s.state(), SessionState::Disconnected);
        assert_eq!(
            FixMessage::decode(&out[0]).unwrap().msg_type(),
            Some(MsgType::Logout)
        );
        // Traffic after logout errors.
        assert_eq!(
            s.handle(&app_msg(3), NOW).unwrap_err(),
            SessionError::SessionOver
        );
    }

    // 9. sequence reset (manual gap fill from the desk) ------------------------------

    #[test]
    fn inbound_sequence_reset_advances() {
        let mut s = FixSession::new("VENUE", "DESK");
        s.handle(&logon(1), NOW).unwrap();
        let mut sr = admin(2, MsgType::SequenceReset);
        sr.push(tag::NEW_SEQ_NO, "10");
        let _ = s.handle(&sr, NOW).unwrap();
        assert_eq!(s.expected_inbound_seq(), 10);
        // Backwards resets are ignored.
        let mut sr2 = admin(10, MsgType::SequenceReset);
        sr2.push(tag::NEW_SEQ_NO, "3");
        let _ = s.handle(&sr2, NOW).unwrap();
        assert_eq!(s.expected_inbound_seq(), 11);
    }

    // 10. determinism ------------------------------------------------------------------

    #[test]
    fn session_is_deterministic() {
        fn run() -> Vec<Vec<Vec<u8>>> {
            let mut s = FixSession::new("VENUE", "DESK");
            let mut acc = Vec::new();
            let l = logon(1);
            acc.push(s.handle(&l, NOW).unwrap().1);
            let m = app_msg(2);
            acc.push(s.handle(&m, NOW).unwrap().1);
            let tr = admin(3, MsgType::TestRequest);
            acc.push(s.handle(&tr, NOW).unwrap().1);
            acc
        }
        assert_eq!(run(), run());
    }
}
