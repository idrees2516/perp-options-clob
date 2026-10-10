//! FIX 4.4 wire codec: the `tag=value<SOH>` framing every FIX engine on
//! earth speaks, with `BodyLength(9)` and `CheckSum(10)` exactly as the
//! specification demands.
//!
//! # The wire format
//!
//! A FIX message is an ordered sequence of fields:
//!
//! ```text
//! 8=FIX.4.4<SOH>9=0123<SOH>35=D<SOH>...body...<SOH>10=168<SOH>
//! ```
//!
//! * `8` (BeginString) must be the first field;
//! * `9` (BodyLength) the second — the byte count from just after the
//!   `9` field's `<SOH>` up to and including the `<SOH>` before `10`;
//! * `10` (CheckSum) the last — the sum of every byte up to and
//!   including that same `<SOH>`, modulo 256, rendered as three ASCII
//!   digits.
//!
//! The codec is deliberately allocation-light and dependency-free:
//! [`FixMessage`] is a `Vec<(u32, String)>` preserving transmission
//! order (repeating groups are opaque to the codec and interpreted by
//! the application layer). Encoding is canonical; decoding is strict —
//! a malformed frame is rejected whole, never partially consumed.

use std::fmt;

/// The field separator byte.
pub const SOH: u8 = 0x01;

/// The BeginString this gateway speaks.
pub const BEGIN_STRING: &str = "FIX.4.4";

/// Well-known FIX 4.4 tag numbers used by this gateway.
pub mod tag {
    /// BeginString.
    pub const BEGIN_STRING: u32 = 8;
    /// BodyLength.
    pub const BODY_LENGTH: u32 = 9;
    /// CheckSum.
    pub const CHECK_SUM: u32 = 10;
    /// MsgSeqNum.
    pub const MSG_SEQ_NUM: u32 = 34;
    /// MsgType.
    pub const MSG_TYPE: u32 = 35;
    /// SenderCompID.
    pub const SENDER_COMP_ID: u32 = 49;
    /// TargetCompID.
    pub const TARGET_COMP_ID: u32 = 56;
    /// SendingTime (UTC, `YYYYMMDD-HH:MM:SS.mss`).
    pub const SENDING_TIME: u32 = 52;
    /// PossDupFlag.
    pub const POSS_DUP_FLAG: u32 = 43;
    /// PossResend.
    pub const POSS_RESEND: u32 = 97;
    /// NewSeqNo (SequenceReset).
    pub const NEW_SEQ_NO: u32 = 36;
    /// GapFillFlag.
    pub const GAP_FILL_FLAG: u32 = 123;
    /// SessionRejectReason.
    pub const SESSION_REJECT_REASON: u32 = 373;
    /// Text.
    pub const TEXT: u32 = 58;
    /// RefSeqNum.
    pub const REF_SEQ_NUM: u32 = 316;
    /// RefMsgType.
    pub const REF_MSG_TYPE: u32 = 372;
    /// ResetSeqNumFlag.
    pub const RESET_SEQ_NUM_FLAG: u32 = 141;
    /// HeartBtInt.
    pub const HEART_BT_INT: u32 = 108;
    /// EncryptMethod.
    pub const ENCRYPT_METHOD: u32 = 98;
    /// TestReqID.
    pub const TEST_REQ_ID: u32 = 112;

    // -- order management -------------------------------------------------
    /// ClOrdID.
    pub const CL_ORD_ID: u32 = 11;
    /// OrderID (venue-assigned).
    pub const ORDER_ID: u32 = 37;
    /// OrigClOrdID.
    pub const ORIG_CL_ORD_ID: u32 = 41;
    /// Symbol.
    pub const SYMBOL: u32 = 55;
    /// Side (1=Buy, 2=Sell).
    pub const SIDE: u32 = 54;
    /// OrderQty (lots).
    pub const ORDER_QTY: u32 = 38;
    /// OrdType (1=Market, 2=Limit).
    pub const ORD_TYPE: u32 = 40;
    /// Price (ticks, integer encoding).
    pub const PRICE: u32 = 44;
    /// StopPx (ticks).
    pub const STOP_PX: u32 = 99;
    /// TimeInForce.
    pub const TIME_IN_FORCE: u32 = 59;
    /// ExecInst (venue extension: `P` = post-only, `R` = reduce-only).
    pub const EXEC_INST: u32 = 18;
    /// TransactTime.
    pub const TRANSACT_TIME: u32 = 60;
    /// ExecType.
    pub const EXEC_TYPE: u32 = 150;
    /// OrdStatus.
    pub const ORD_STATUS: u32 = 39;
    /// LeavesQty.
    pub const LEAVES_QTY: u32 = 151;
    /// CumQty.
    pub const CUM_QTY: u32 = 14;
    /// AvgPx.
    pub const AVG_PX: u32 = 6;
    /// LastPx.
    pub const LAST_PX: u32 = 31;
    /// LastQty.
    pub const LAST_QTY: u32 = 32;
    /// ExecID.
    pub const EXEC_ID: u32 = 17;
    /// Text (rejections).
    pub const ORDER_TEXT: u32 = 58;

    // -- market data ------------------------------------------------------
    /// MDReqID.
    pub const MD_REQ_ID: u32 = 262;
    /// SubscriptionRequestType (0 = snapshot).
    pub const SUBSCRIPTION_REQUEST_TYPE: u32 = 263;
    /// NoRelatedSym.
    pub const NO_RELATED_SYM: u32 = 146;
    /// NoMDEntryTypes.
    pub const NO_MD_ENTRY_TYPES: u32 = 267;
    /// MDEntryType (0 = bid, 1 = ask).
    pub const MD_ENTRY_TYPE: u32 = 269;
    /// NoMDEntries.
    pub const NO_MD_ENTRIES: u32 = 268;
    /// MDEntryPx.
    pub const MD_ENTRY_PX: u32 = 270;
    /// MDEntrySize.
    pub const MD_ENTRY_SIZE: u32 = 271;

    // -- RFQ (the Paradigm dealer flow over FIX) ---------------------------
    /// QuoteReqID.
    pub const QUOTE_REQ_ID: u32 = 131;
    /// QuoteID.
    pub const QUOTE_ID: u32 = 117;
    /// BidPx.
    pub const BID_PX: u32 = 132;
    /// OfferPx.
    pub const OFFER_PX: u32 = 133;
    /// BidSize.
    pub const BID_SIZE: u32 = 136;
    /// OfferSize.
    pub const OFFER_SIZE: u32 = 135;
    /// QuoteStatus.
    pub const QUOTE_STATUS: u32 = 297;
}

/// FIX message types this gateway understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MsgType {
    /// Heartbeat.
    Heartbeat,
    /// TestRequest.
    TestRequest,
    /// ResendRequest.
    ResendRequest,
    /// Reject.
    Reject,
    /// SequenceReset (GapFill / Reset).
    SequenceReset,
    /// Logout.
    Logout,
    /// Logon.
    Logon,
    /// NewOrderSingle.
    NewOrderSingle,
    /// OrderCancelRequest.
    OrderCancelRequest,
    /// OrderCancelReplaceRequest.
    OrderCancelReplaceRequest,
    /// OrderStatusRequest.
    OrderStatusRequest,
    /// ExecutionReport.
    ExecutionReport,
    /// OrderCancelReject.
    OrderCancelReject,
    /// MarketDataRequest.
    MarketDataRequest,
    /// MarketDataSnapshotFullRefresh.
    MarketDataSnapshotFullRefresh,
    /// QuoteRequest (RFQ).
    QuoteRequest,
    /// Quote (dealer answer).
    Quote,
    /// QuoteCancel.
    QuoteCancel,
    /// QuoteStatusRequest.
    QuoteStatusRequest,
    /// Anything else (carries the raw code).
    Other(u32),
}

impl MsgType {
    /// The `35=` wire code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Heartbeat => "0",
            Self::TestRequest => "1",
            Self::ResendRequest => "2",
            Self::Reject => "3",
            Self::SequenceReset => "4",
            Self::Logout => "5",
            Self::Logon => "A",
            Self::NewOrderSingle => "D",
            Self::OrderCancelRequest => "F",
            Self::OrderCancelReplaceRequest => "G",
            Self::OrderStatusRequest => "H",
            Self::ExecutionReport => "8",
            Self::OrderCancelReject => "9",
            Self::MarketDataRequest => "V",
            Self::MarketDataSnapshotFullRefresh => "W",
            Self::QuoteRequest => "R",
            Self::Quote => "S",
            Self::QuoteCancel => "Z",
            Self::QuoteStatusRequest => "a",
            Self::Other(c) => match c {
                0x100 => "?",
                _ => "?",
            },
        }
    }

    /// Parse a `35=` value.
    #[must_use]
    pub fn parse(code: &str) -> Self {
        match code {
            "0" => Self::Heartbeat,
            "1" => Self::TestRequest,
            "2" => Self::ResendRequest,
            "3" => Self::Reject,
            "4" => Self::SequenceReset,
            "5" => Self::Logout,
            "A" => Self::Logon,
            "D" => Self::NewOrderSingle,
            "F" => Self::OrderCancelRequest,
            "G" => Self::OrderCancelReplaceRequest,
            "H" => Self::OrderStatusRequest,
            "8" => Self::ExecutionReport,
            "9" => Self::OrderCancelReject,
            "V" => Self::MarketDataRequest,
            "W" => Self::MarketDataSnapshotFullRefresh,
            "R" => Self::QuoteRequest,
            "S" => Self::Quote,
            "Z" => Self::QuoteCancel,
            "a" => Self::QuoteStatusRequest,
            other => Self::Other(u32::from(other.as_bytes().first().copied().unwrap_or(0))),
        }
    }
}

/// Wire-level parse/encode failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The frame does not start with `8=FIX.4.4`.
    BadBeginString,
    /// `BodyLength` missing, unparsable, or disagreeing with the frame.
    BadBodyLength,
    /// `CheckSum` missing, unparsable, or disagreeing with the bytes.
    BadCheckSum { expected: u16, got: u16 },
    /// A field had no `=`, an empty tag, or a non-numeric tag.
    MalformedField,
    /// Trailing bytes after the CheckSum field.
    TrailingBytes,
    /// The buffer is shorter than any legal FIX frame.
    TooShort,
    /// The frame terminator is missing (partial transmission).
    Unterminated,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadBeginString => write!(f, "frame must begin with 8={BEGIN_STRING}"),
            Self::BadBodyLength => write!(f, "bad or missing body length"),
            Self::BadCheckSum { expected, got } => {
                write!(f, "checksum mismatch: expected {expected:03}, got {got:03}")
            }
            Self::MalformedField => write!(f, "malformed field"),
            Self::TrailingBytes => write!(f, "trailing bytes after checksum"),
            Self::TooShort => write!(f, "frame too short"),
            Self::Unterminated => write!(f, "unterminated frame"),
        }
    }
}

impl std::error::Error for WireError {}

/// An ordered FIX message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixMessage {
    fields: Vec<(u32, String)>,
}

impl FixMessage {
    /// An empty message.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a field (transmission order preserved).
    pub fn push(&mut self, tag: u32, value: impl Into<String>) -> &mut Self {
        self.fields.push((tag, value.into()));
        self
    }

    /// First value of a tag.
    #[must_use]
    pub fn get(&self, tag: u32) -> Option<&str> {
        self.fields
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, v)| v.as_str())
    }

    /// All values of a repeating tag, in order.
    #[must_use]
    pub fn get_all(&self, tag: u32) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|(t, _)| *t == tag)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Raw ordered field list (for re-encoding wrappers).
    #[must_use]
    pub fn fields_raw(&self) -> &[(u32, String)] {
        &self.fields
    }

    /// The message type, if `35` is present and legal.
    #[must_use]
    pub fn msg_type(&self) -> Option<MsgType> {
        self.get(tag::MSG_TYPE).map(MsgType::parse)
    }

    /// Integer view of a tag's first value.
    #[must_use]
    pub fn get_u64(&self, tag: u32) -> Option<u64> {
        self.get(tag).and_then(|v| v.parse().ok())
    }

    /// The checksum of the encoded body (everything up to and including
    /// the `<SOH>` before `10=`).
    fn checksum_of(bytes: &[u8]) -> u16 {
        let sum: u64 = bytes.iter().map(|&b| u64::from(b)).sum();
        (sum % 256) as u16
    }

    /// Encode to wire bytes with canonical `BodyLength` and `CheckSum`.
    ///
    /// The header must already contain every field the caller wants
    /// *before* `35`; this method appends nothing but the length and
    /// checksum framing.
    pub fn encode(&self) -> Vec<u8> {
        // Serialize the body: everything except BeginString/BodyLength.
        let mut body = Vec::new();
        for &(t, ref v) in &self.fields {
            if t == tag::BEGIN_STRING || t == tag::BODY_LENGTH || t == tag::CHECK_SUM {
                continue;
            }
            body.extend_from_slice(t.to_string().as_bytes());
            body.push(b'=');
            body.extend_from_slice(v.as_bytes());
            body.push(SOH);
        }
        let begin = format!("{tag}={BEGIN_STRING}", tag = tag::BEGIN_STRING);
        let header = format!("{tag}={len}", tag = tag::BODY_LENGTH, len = body.len());
        let mut out = Vec::with_capacity(body.len() + begin.len() + header.len() + 8);
        out.extend_from_slice(begin.as_bytes());
        out.push(SOH);
        out.extend_from_slice(header.as_bytes());
        out.push(SOH);
        out.extend_from_slice(&body);
        let ck = Self::checksum_of(&out);
        out.extend_from_slice(format!("{}={ck:03}", tag::CHECK_SUM).as_bytes());
        out.push(SOH);
        out
    }

    /// Decode one complete frame. The buffer must contain exactly one
    /// message (callers buffer partial transmissions upstream).
    pub fn decode(frame: &[u8]) -> Result<Self, WireError> {
        if frame.len() < 20 {
            return Err(WireError::TooShort);
        }
        if frame.first() == Some(&b'8') {
            // ok
        } else {
            return Err(WireError::BadBeginString);
        }

        // Walk fields. Field i starts where the previous SOH left off.
        let mut fields: Vec<(u32, String)> = Vec::new();
        let mut pos = 0_usize;
        let mut saw_checksum = false;
        while pos < frame.len() {
            let end = match frame[pos..].iter().position(|&b| b == SOH) {
                Some(off) => pos + off,
                None => return Err(WireError::Unterminated),
            };
            let raw = &frame[pos..end];
            if raw.is_empty() {
                return Err(WireError::MalformedField);
            }
            let eq = match raw.iter().position(|&b| b == b'=') {
                Some(i) if i > 0 => i,
                _ => return Err(WireError::MalformedField),
            };
            let t: u32 = std::str::from_utf8(&raw[..eq])
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or(WireError::MalformedField)?;
            let v = String::from_utf8_lossy(&raw[eq + 1..]).into_owned();
            if t == tag::CHECK_SUM {
                saw_checksum = true;
                if end + 1 != frame.len() {
                    return Err(WireError::TrailingBytes);
                }
                // Verify checksum over everything up to this field's SOH
                // (exclusive of the checksum field itself).
                let expected = Self::checksum_of(&frame[..pos]);
                let got: u16 = v
                    .parse()
                    .map_err(|_| WireError::BadCheckSum { expected, got: 999 })?;
                if expected != got {
                    return Err(WireError::BadCheckSum { expected, got });
                }
                fields.push((t, v));
                break;
            }
            fields.push((t, v));
            pos = end + 1;
        }
        if !saw_checksum {
            return Err(WireError::Unterminated);
        }

        // Structural validation: 8, 9, and the body length agreement.
        // The checksum field is exactly `10=` + 3 digits + <SOH> = 7 bytes.
        let first = fields.first().ok_or(WireError::TooShort)?;
        if first.0 != tag::BEGIN_STRING || first.1 != BEGIN_STRING {
            return Err(WireError::BadBeginString);
        }
        let body_len: usize = fields
            .get(1)
            .and_then(|(t, v)| (*t == tag::BODY_LENGTH).then(|| v.parse::<usize>().ok()))
            .unwrap_or(None)
            .ok_or(WireError::BadBodyLength)?;
        // Body spans from after the `9=NNN<SOH>` field to the SOH before
        // `10=`: frame = 8-field + 9-field + body + 10-field (7 bytes).
        let len_field_end = {
            let mut acc = 0_usize;
            for (t, v) in &fields {
                acc += t.to_string().len() + 1 + v.len() + 1;
                if *t == tag::BODY_LENGTH {
                    break;
                }
            }
            acc
        };
        if body_len + len_field_end + 7 != frame.len() {
            return Err(WireError::BadBodyLength);
        }
        Ok(Self { fields })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // 1. round trip preserves fields -----------------------------------------

    #[test]
    fn round_trip_preserves_fields() {
        let m = logon(7);
        let bytes = m.encode();
        let back = FixMessage::decode(&bytes).unwrap();
        assert_eq!(back.get_u64(tag::MSG_SEQ_NUM), Some(7));
        assert_eq!(back.get(tag::SENDER_COMP_ID), Some("DESK"));
        assert_eq!(back.get(tag::TARGET_COMP_ID), Some("VENUE"));
        assert_eq!(back.get_u64(tag::HEART_BT_INT), Some(30));
        assert_eq!(back.msg_type(), Some(MsgType::Logon));
        // Canonical re-encode is byte-identical.
        assert_eq!(back.encode(), bytes);
    }

    // 2. checksum catches corruption ------------------------------------------

    #[test]
    fn checksum_rejects_corruption() {
        let m = logon(1);
        let mut bytes = m.encode();
        // Corrupt one byte inside the body.
        let idx = bytes.len() / 2;
        bytes[idx] = b'X';
        assert!(matches!(
            FixMessage::decode(&bytes),
            Err(WireError::BadCheckSum { .. })
        ));
    }

    // 3. length validation ----------------------------------------------------

    #[test]
    fn body_length_must_agree() {
        let mut m = logon(1);
        // Inject a wrong BodyLength into the message model: encode uses
        // the computed length, so craft a raw frame instead.
        m.push(9999, "zz");
        let good = m.encode();
        assert!(FixMessage::decode(&good).is_ok());
        // Manually tamper with the 9= field of a valid frame.
        let mut tampered = good.clone();
        let pos = tampered.windows(2).position(|w| w == *b"9=").unwrap_or(0);
        tampered[pos + 2] = b'0';
        assert!(matches!(
            FixMessage::decode(&tampered),
            Err(WireError::BadBodyLength) | Err(WireError::BadCheckSum { .. })
        ));
    }

    // 4. structural rejections -------------------------------------------------

    #[test]
    fn structural_rejections() {
        assert_eq!(FixMessage::decode(b""), Err(WireError::TooShort));
        let mut m = logon(1);
        let mut bytes = m.encode();
        // Cut the checksum off: unterminated / too short.
        bytes.truncate(bytes.len() - 4);
        assert!(FixMessage::decode(&bytes).is_err());
        // Trailing junk after checksum.
        let mut junk = logon(1).encode();
        junk.extend_from_slice(b"junk");
        assert_eq!(FixMessage::decode(&junk), Err(WireError::TrailingBytes));
        // A minimal valid heartbeat built through the encoder.
        let mut hb = FixMessage::new();
        hb.push(tag::MSG_TYPE, MsgType::Heartbeat.code());
        let raw = hb.encode();
        assert!(FixMessage::decode(&raw).is_ok());
        // A field without '=' (checksum computed honestly).
        let mut bytes: Vec<u8> = b"8=FIX.4.4\x019=9\x01".to_vec();
        bytes.extend_from_slice(b"BADFIELD\x01");
        let sum: u16 = (bytes.iter().map(|&b| u64::from(b)).sum::<u64>() % 256) as u16;
        bytes.extend_from_slice(format!("10={sum:03}\x01").as_bytes());
        assert_eq!(FixMessage::decode(&bytes), Err(WireError::MalformedField));
    }

    // 5. msg type parse --------------------------------------------------------

    #[test]
    fn msg_type_round_trip() {
        for t in [
            MsgType::Logon,
            MsgType::Heartbeat,
            MsgType::TestRequest,
            MsgType::ResendRequest,
            MsgType::Reject,
            MsgType::SequenceReset,
            MsgType::Logout,
            MsgType::NewOrderSingle,
            MsgType::OrderCancelRequest,
            MsgType::OrderCancelReplaceRequest,
            MsgType::OrderStatusRequest,
            MsgType::ExecutionReport,
            MsgType::OrderCancelReject,
            MsgType::MarketDataRequest,
            MsgType::MarketDataSnapshotFullRefresh,
            MsgType::QuoteRequest,
            MsgType::Quote,
            MsgType::QuoteCancel,
            MsgType::QuoteStatusRequest,
        ] {
            assert_eq!(MsgType::parse(t.code()), t);
        }
    }

    // 6. repeating group access ------------------------------------------------

    #[test]
    fn repeating_group_get_all() {
        let mut m = FixMessage::new();
        m.push(146, "2");
        m.push(55, "BTC-PERP");
        m.push(55, "ETH-PERP");
        assert_eq!(m.get_all(55), vec!["BTC-PERP", "ETH-PERP"]);
        assert_eq!(m.get(55), Some("BTC-PERP"));
    }

    // 7. empty values and unicode safety ----------------------------------------

    #[test]
    fn values_are_raw_strings() {
        let mut m = FixMessage::new();
        m.push(58, "rejected: price band (±5%)");
        let bytes = m.encode();
        let back = FixMessage::decode(&bytes).unwrap();
        assert_eq!(back.get(58), Some("rejected: price band (±5%)"));
    }
}
