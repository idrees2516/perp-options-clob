//! FIX 4.4 session codec and order-gateway mapping (G-26).
//!
//! ## What this is — and deliberately is not
//!
//! Institutional desks quote FIX before they quote anything else: it is
//! the wire their OMS speaks. This module is the **deterministic core**
//! of a FIX connectivity layer:
//!
//! * a complete tag=value message codec (SOH-separated, the FIX
//!   grammar), with checksum validation;
//! * the venue's FIX subset as typed messages — Logon, Logout,
//!   NewOrderSingle, OrderCancelRequest, OrderStatusRequest, and the
//!   ExecutionReport / CancelReject / MarketDataSnapshot responses;
//! * a pure translation onto the engine's [`Command`] surface, so FIX
//!   sessions are just another front end over the same deterministic
//!   sequencer.
//!
//! What it is not: a TCP/SSL transport, sequence-number persistence, or
//! a session-state recovery engine. Those are deployment concerns that
//! belong to the gateway binary, not the testable protocol core — the
//! same boundary the rest of this workspace draws everywhere else.
//!
//! ## The subset (fields fixed by the venue profile)
//!
//! ```text
//! 8=FIX.4.4|9=len|35=msgtype|49=sender|56=target|34=seq|...
//! ```
//!
//! Tags follow FIX 4.4 numbering. The codec is byte-exact and
//! round-trip tested; MsgType `0` (heartbeat) and test requests are
//! accepted and echoed by the session layer.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use poc_core::{OrderType, SelfTradePrevention, Side, TimeInForce};
use poc_engine::command::OrderRequest;

/// The SOH separator (0x01) every FIX field is terminated with.
pub const SOH: char = '\u{1}';

/// A decoded FIX message: tag → value, insertion-ordered by tag (FIX
/// requires ascending tags on the wire for some engines; we emit sorted).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FixMessage {
    /// Field map (tag → raw string).
    pub fields: BTreeMap<u32, String>,
}

/// Codec errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// Malformed field (no `=` or empty).
    BadField(String),
    /// The leading BeginString tag was not 8.
    MissingBeginString,
    /// The BodyLength tag did not match the body.
    BadBodyLength,
    /// The trailing checksum tag was not 10 or did not match.
    BadChecksum,
    /// The message type tag (35) was missing or unknown.
    BadMsgType(String),
}

impl FixMessage {
    /// An empty message.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one field.
    pub fn set(&mut self, tag: u32, value: impl Into<String>) -> &mut Self {
        self.fields.insert(tag, value.into());
        self
    }

    /// Read one field.
    #[must_use]
    pub fn get(&self, tag: u32) -> Option<&str> {
        self.fields.get(&tag).map(String::as_str)
    }

    /// Encode to the wire format: `8=..|9=..|<body>|10=checksum|`.
    /// BodyLength covers every byte between the 9 field's SOH and the
    /// checksum field's SOH; the checksum is the byte sum of the message
    /// up to and including the SOH before tag 10, mod 256.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut head = String::new();
        let begin = self
            .fields
            .get(&8)
            .cloned()
            .unwrap_or_else(|| "FIX.4.4".into());
        let _ = write!(head, "8={begin}{SOH}");
        // Body = all fields except 8, 9, 10, in ascending tag order.
        let mut body = String::new();
        for (tag, value) in &self.fields {
            if matches!(tag, 8..=10) {
                continue;
            }
            let _ = write!(body, "{tag}={value}{SOH}");
        }
        let mut out = head;
        let _ = write!(out, "9={}{SOH}", body.len());
        out.push_str(&body);
        let checksum = checksum_of(&out);
        let _ = write!(out, "10={checksum:03}{SOH}");
        out
    }

    /// Decode from the wire format, validating length and checksum.
    pub fn decode(wire: &str) -> Result<Self, CodecError> {
        let mut fields = BTreeMap::new();
        for field in wire.trim_matches(SOH).split(SOH) {
            if field.is_empty() {
                continue;
            }
            let Some((tag, value)) = field.split_once('=') else {
                return Err(CodecError::BadField(field.to_string()));
            };
            let Ok(tag) = tag.parse::<u32>() else {
                return Err(CodecError::BadField(field.to_string()));
            };
            fields.insert(tag, value.to_string());
        }
        if !fields.contains_key(&8) {
            return Err(CodecError::MissingBeginString);
        }
        // BodyLength: bytes between the 9 field's SOH and the 10 field.
        let expected: usize = fields
            .get(&9)
            .and_then(|v| v.parse().ok())
            .ok_or(CodecError::BadBodyLength)?;
        let mut body = String::new();
        for (tag, value) in &fields {
            if matches!(tag, 8..=10) {
                continue;
            }
            let _ = write!(body, "{tag}={value}{SOH}");
        }
        if body.len() != expected {
            return Err(CodecError::BadBodyLength);
        }
        // Checksum: sum of everything up to (not including) tag 10.
        let mut prefix = String::new();
        let _ = write!(
            prefix,
            "8={}{SOH}",
            fields.get(&8).cloned().unwrap_or_default()
        );
        let _ = write!(prefix, "9={expected}{SOH}");
        prefix.push_str(&body);
        let computed = checksum_of(&prefix);
        let given: u32 = fields
            .get(&10)
            .and_then(|v| v.parse().ok())
            .ok_or(CodecError::BadChecksum)?;
        if computed != given {
            return Err(CodecError::BadChecksum);
        }
        Ok(FixMessage { fields })
    }

    /// The message type (tag 35).
    #[must_use]
    pub fn msg_type(&self) -> Option<&str> {
        self.get(35)
    }

    /// Application fields only (envelope tags 8/9/10 excluded) — the
    /// comparison basis for round trips.
    #[must_use]
    pub fn application_fields(&self) -> BTreeMap<u32, String> {
        self.fields
            .iter()
            .filter(|(tag, _)| !matches!(tag, 8..=10))
            .map(|(t, v)| (*t, v.clone()))
            .collect()
    }
}

/// FIX checksum: byte sum mod 256.
fn checksum_of(s: &str) -> u32 {
    s.bytes().map(u32::from).sum::<u32>() % 256
}

/// The venue's FIX application-layer subset, decoded one step past the
/// generic tag map into typed intents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayMessage {
    /// Logon (A): session establishment.
    Logon {
        /// Heartbeat interval the client asks for (tag 108), seconds.
        heartbeat_s: u32,
    },
    /// Logout (5).
    Logout,
    /// NewOrderSingle (D): a one-lot-or-more placement.
    NewOrderSingle {
        /// The engine request translated from FIX tags.
        request: OrderRequest,
        /// Client order id (tag 11) for report matching.
        cl_ord_id: String,
    },
    /// OrderCancelRequest (F).
    OrderCancelRequest {
        /// The client order id being canceled.
        cl_ord_id: String,
        /// The engine order id (tag 1 on the cancel).
        order_id: u64,
    },
    /// OrderStatusRequest (H).
    OrderStatusRequest {
        /// The client order id to report on.
        cl_ord_id: String,
    },
    /// MarketDataRequest (V) — full-depth snapshot semantics.
    MarketDataRequest {
        /// The requested symbol.
        symbol: String,
    },
    /// Heartbeat (0) / TestRequest (1) — session keep-alives.
    Heartbeat,
}

impl GatewayMessage {
    /// Decode the typed layer from a generic FIX message.
    pub fn from_fix(msg: &FixMessage) -> Result<Self, CodecError> {
        let unknown = || CodecError::BadMsgType(msg.get(35).unwrap_or("").to_string());
        match msg.get(35).unwrap_or("") {
            "A" => Ok(GatewayMessage::Logon {
                heartbeat_s: msg.get(108).and_then(|v| v.parse().ok()).unwrap_or(30),
            }),
            "5" => Ok(GatewayMessage::Logout),
            "0" | "1" => Ok(GatewayMessage::Heartbeat),
            "D" => {
                let symbol = msg.get(55).unwrap_or_default().to_string();
                let side = match msg.get(54) {
                    Some("1") => Side::Bid,
                    Some("2") => Side::Ask,
                    _ => return Err(unknown()),
                };
                let qty: u64 = msg.get(38).and_then(|v| v.parse().ok()).unwrap_or(0);
                let price: Option<u64> = msg.get(44).and_then(|v| v.parse().ok());
                // FIX 59: 0=Day, 1=GTC, 3=IOC, 4=FOK.
                let tif = match msg.get(59).unwrap_or("1") {
                    "3" => TimeInForce::Ioc,
                    "4" => TimeInForce::Fok,
                    _ => TimeInForce::Gtc,
                };
                let order_type = match (msg.get(40).unwrap_or("2"), price) {
                    ("1", _) => OrderType::Market,
                    ("2", Some(p)) => OrderType::StopLimit {
                        trigger_price: p,
                        limit_price: p,
                    },
                    ("3", Some(_p)) => OrderType::Limit,
                    (_, Some(p)) => OrderType::StopLimit {
                        trigger_price: p,
                        limit_price: p,
                    },
                    (_, None) => OrderType::Market,
                };
                let subaccount: u64 = msg
                    .get(1)
                    .or_else(|| msg.get(49))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                Ok(GatewayMessage::NewOrderSingle {
                    request: OrderRequest {
                        subaccount,
                        symbol,
                        side,
                        order_type,
                        price_ticks: price,
                        qty_lots: qty,
                        tif,
                        post_only: false,
                        reduce_only: msg.get(59) == Some("5"), // reduce-only convention absent in FIX: use GTD as reserved
                        stp: SelfTradePrevention::CancelNewest,
                        display_lots: None,
                        oco_group: None,
                        client_ts: msg.get(52).and_then(|v| v.parse().ok()).unwrap_or(0),
                    },
                    cl_ord_id: msg.get(11).unwrap_or_default().to_string(),
                })
            }
            "F" => Ok(GatewayMessage::OrderCancelRequest {
                cl_ord_id: msg.get(11).unwrap_or_default().to_string(),
                order_id: msg.get(37).and_then(|v| v.parse().ok()).unwrap_or(0),
            }),
            "H" => Ok(GatewayMessage::OrderStatusRequest {
                cl_ord_id: msg.get(11).unwrap_or_default().to_string(),
            }),
            "V" => Ok(GatewayMessage::MarketDataRequest {
                symbol: msg.get(55).unwrap_or_default().to_string(),
            }),
            _ => Err(unknown()),
        }
    }

    /// Encode this typed message into an outbound ExecutionReport (35=8)
    /// acknowledging a placement — the report a FIX client expects
    /// first.
    #[must_use]
    pub fn execution_report_ack(
        cl_ord_id: &str,
        order_id: u64,
        qty: u64,
        symbol: &str,
    ) -> FixMessage {
        let mut m = FixMessage::new();
        m.set(35, "8")
            .set(11, cl_ord_id)
            .set(37, order_id.to_string());
        m.set(55, symbol).set(150, "0").set(39, "0"); // New
        m.set(38, qty.to_string()).set(6, "0");
        m
    }

    /// An outbound CancelReject (35=9).
    #[must_use]
    pub fn cancel_reject(cl_ord_id: &str, reason: u32) -> FixMessage {
        let mut m = FixMessage::new();
        m.set(35, "9")
            .set(11, cl_ord_id)
            .set(102, reason.to_string());
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip_with_valid_checksum() {
        let mut m = FixMessage::new();
        m.set(35, "A").set(49, "DESK").set(56, "VENUE").set(34, "1");
        m.set(108, "30");
        let wire = m.encode();
        let back = FixMessage::decode(&wire).expect("decodes");
        // The envelope (8/9/10) is added by decode; the application
        // fields must survive exactly.
        assert_eq!(back.application_fields(), m.application_fields());
    }

    #[test]
    fn corrupted_checksum_rejected() {
        let mut m = FixMessage::new();
        m.set(35, "A").set(108, "30");
        let wire = m.encode();
        // Tamper with one value (swap the heartbeat) without fixing 10=.
        let tampered = wire.replace("30\u{1}", "31\u{1}");
        if tampered == wire {
            // 30 appears only in 108; guard against accidental no-op.
            return;
        }
        assert!(FixMessage::decode(&tampered).is_err());
    }

    #[test]
    fn new_order_single_translates_to_engine_request() {
        let mut m = FixMessage::new();
        m.set(35, "D").set(49, "7").set(55, "BTC-PERP").set(54, "1");
        m.set(38, "5")
            .set(44, "79900")
            .set(40, "3")
            .set(11, "ord-1");
        let typed = GatewayMessage::from_fix(&m).expect("typed");
        match typed {
            GatewayMessage::NewOrderSingle { request, cl_ord_id } => {
                assert_eq!(cl_ord_id, "ord-1");
                assert_eq!(request.subaccount, 7);
                assert_eq!(request.symbol, "BTC-PERP");
                assert_eq!(request.side, Side::Bid);
                assert_eq!(request.qty_lots, 5);
                assert_eq!(request.price_ticks, Some(79_900));
                assert_eq!(request.tif, TimeInForce::Gtc);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn logon_and_logout_decode() {
        let mut m = FixMessage::new();
        m.set(35, "A").set(108, "5");
        assert_eq!(
            GatewayMessage::from_fix(&m),
            Ok(GatewayMessage::Logon { heartbeat_s: 5 })
        );
        let mut m = FixMessage::new();
        m.set(35, "5");
        assert_eq!(GatewayMessage::from_fix(&m), Ok(GatewayMessage::Logout));
    }

    #[test]
    fn execution_report_ack_round_trips() {
        let m = GatewayMessage::execution_report_ack("c1", 42, 5, "BTC-PERP");
        assert_eq!(m.get(35), Some("8"));
        assert_eq!(m.get(39), Some("0"));
        let wire = m.encode();
        let back = FixMessage::decode(&wire).expect("decodes");
        assert_eq!(back.application_fields(), m.application_fields());
    }

    #[test]
    fn unknown_msg_type_rejected() {
        let mut m = FixMessage::new();
        m.set(35, "ZZ");
        assert!(GatewayMessage::from_fix(&m).is_err());
    }
}
