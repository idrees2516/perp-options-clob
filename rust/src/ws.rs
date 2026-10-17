//! RFC 6455 WebSocket frame codec (server side).
//!
//! Protocol-abuse defenses baked in:
//! * Client frames MUST be masked; unmasked client frames are a protocol
//!   error (close 1002) — this is what the RFC mandates for servers and
//!   what naive "cross-protocol" attacks send.
//! * RSV bits must be zero (no extensions negotiated).
//! * Payload cap (default 16 KiB) — oversized frames close 1009.
//! * Control frames (ping/pong/close) must have FIN set and payload ≤ 125.
//! * Fragmented message assembly is capped (message bomb defense).
//! * Close frames are surfaced so the connection can echo and shut down.

/// Maximum single-frame payload accepted from a client.
pub const MAX_PAYLOAD: usize = 16 * 1024;

/// WebSocket opcode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Opcode {
    /// Continuation of a fragmented message.
    Continuation,
    /// Text message.
    Text,
    /// Binary message.
    Binary,
    /// Close.
    Close,
    /// Ping.
    Ping,
    /// Pong.
    Pong,
    /// Reserved opcode (protocol error).
    Reserved,
}

impl Opcode {
    fn from_u8(v: u8) -> Opcode {
        match v {
            0x0 => Opcode::Continuation,
            0x1 => Opcode::Text,
            0x2 => Opcode::Binary,
            0x8 => Opcode::Close,
            0x9 => Opcode::Ping,
            0xA => Opcode::Pong,
            _ => Opcode::Reserved,
        }
    }
}

/// A decoded frame.
#[derive(Clone, Debug)]
pub struct Frame {
    /// FIN bit set?
    pub fin: bool,
    /// Opcode.
    pub opcode: Opcode,
    /// Payload (unmasked).
    pub payload: Vec<u8>,
}

/// Decode result.
pub enum Decoded {
    /// Incomplete; keep feeding.
    NeedMore,
    /// One complete frame; the usize is total bytes consumed.
    Frame(Frame, usize),
    /// Protocol violation: close with this code.
    Violation(u16, &'static str),
}

/// Decode one frame from `buf` (which may contain trailing bytes of
/// subsequent frames).
pub fn decode(buf: &[u8]) -> Decoded {
    if buf.len() < 2 {
        return Decoded::NeedMore;
    }
    let b0 = buf[0];
    let b1 = buf[1];
    if b0 & 0x70 != 0 {
        return Decoded::Violation(1002, "reserved bits set");
    }
    let fin = b0 & 0x80 != 0;
    let opcode = Opcode::from_u8(b0 & 0x0F);
    let masked = b1 & 0x80 != 0;
    let mut len = (b1 & 0x7F) as usize;
    let mut off = 2usize;
    match len {
        126 => {
            if buf.len() < off + 2 {
                return Decoded::NeedMore;
            }
            len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            off = 4;
        }
        127 => {
            if buf.len() < off + 8 {
                return Decoded::NeedMore;
            }
            let mut w = [0u8; 8];
            w.copy_from_slice(&buf[2..10]);
            // A u64 length that can't fit usize is protocol abuse.
            let l = u64::from_be_bytes(w);
            if l > u64::MAX / 2 || l > usize::MAX as u64 {
                return Decoded::Violation(1009, "absurd length");
            }
            len = l as usize;
            off = 10;
        }
        _ => {}
    }
    if !masked {
        return Decoded::Violation(1002, "client frame not masked");
    }
    if len > MAX_PAYLOAD {
        return Decoded::Violation(1009, "frame too large");
    }
    if matches!(opcode, Opcode::Ping | Opcode::Pong | Opcode::Close) {
        if !fin {
            return Decoded::Violation(1002, "fragmented control frame");
        }
        if len > 125 {
            return Decoded::Violation(1002, "control frame payload > 125");
        }
    }
    if matches!(opcode, Opcode::Reserved) {
        return Decoded::Violation(1002, "reserved opcode");
    }
    if buf.len() < off + 4 + len {
        return Decoded::NeedMore;
    }
    let mask = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
    let mut payload = buf[off + 4..off + 4 + len].to_vec();
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
    let total = off + 4 + len;
    Decoded::Frame(
        Frame {
            fin,
            opcode,
            payload,
        },
        total,
    )
}

/// Serialize a server frame (never masked).
pub fn encode(opcode: Opcode, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    let op = match opcode {
        Opcode::Continuation => 0x00,
        Opcode::Text => 0x01,
        Opcode::Binary => 0x02,
        Opcode::Close => 0x08,
        Opcode::Ping => 0x09,
        Opcode::Pong => 0x0A,
        Opcode::Reserved => 0x00,
    };
    out.push(0x80 | op); // server frames are always FIN.
    if payload.len() < 126 {
        out.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// Encode a text message.
pub fn encode_text(s: &str) -> Vec<u8> {
    encode(Opcode::Text, s.as_bytes())
}

/// Encode a close frame with a status code.
pub fn encode_close(code: u16) -> Vec<u8> {
    encode(Opcode::Close, &code.to_be_bytes())
}

/// Assembles fragmented messages into whole ones, enforcing a total
/// message-size cap.
#[derive(Default)]
pub struct MessageBuf {
    /// Assembled message bytes.
    pub data: Vec<u8>,
    /// Assembling a fragmented message?
    in_msg: bool,
    /// Fragmented message must be text or binary.
    msg_binary: bool,
}

impl MessageBuf {
    /// Feed a decoded frame; `Some((is_binary, message))` when a complete
    /// data message arrived. Control frames are passed through as
    /// `Some(Control(frame))`? No — they are returned via the enum.
    pub fn feed(&mut self, f: Frame) -> MsgResult {
        match f.opcode {
            Opcode::Text | Opcode::Binary => {
                if self.in_msg {
                    return MsgResult::Violation(1002, "new data frame during fragmentation");
                }
                if f.fin {
                    if f.payload.len() > MAX_PAYLOAD {
                        return MsgResult::Violation(1009, "message too large");
                    }
                    return MsgResult::Message(f.opcode == Opcode::Binary, f.payload);
                }
                if f.payload.len() > MAX_PAYLOAD {
                    return MsgResult::Violation(1009, "message too large");
                }
                self.in_msg = true;
                self.msg_binary = f.opcode == Opcode::Binary;
                self.data = f.payload;
                MsgResult::NeedMore
            }
            Opcode::Continuation => {
                if !self.in_msg {
                    return MsgResult::Violation(1002, "continuation without start");
                }
                if self.data.len() + f.payload.len() > MAX_PAYLOAD {
                    return MsgResult::Violation(1009, "message too large");
                }
                self.data.extend_from_slice(&f.payload);
                if f.fin {
                    self.in_msg = false;
                    let data = std::mem::take(&mut self.data);
                    MsgResult::Message(self.msg_binary, data)
                } else {
                    MsgResult::NeedMore
                }
            }
            Opcode::Ping => MsgResult::Control(f),
            Opcode::Pong => MsgResult::Control(f),
            Opcode::Close => MsgResult::Closed,
            Opcode::Reserved => MsgResult::Violation(1002, "reserved opcode"),
        }
    }
}

/// Result of feeding a frame to [`MessageBuf`].
pub enum MsgResult {
    /// Still assembling.
    NeedMore,
    /// A complete data message (is_binary, bytes).
    Message(bool, Vec<u8>),
    /// A control frame (ping/pong) — handle it.
    Control(Frame),
    /// Client closed.
    Closed,
    /// Protocol violation: close with code.
    Violation(u16, &'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_frame(op: u8, payload: &[u8], fin: bool) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(if fin { 0x80 | op } else { op });
        if payload.len() < 126 {
            out.push(0x80 | payload.len() as u8);
        } else {
            out.push(0x80 | 126);
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        let mask = [0x37, 0xFA, 0x21, 0x3D];
        out.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            out.push(b ^ mask[i & 3]);
        }
        out
    }

    #[test]
    fn roundtrip_text() {
        let bytes = client_frame(0x1, b"{\"op\":\"ping\"}", true);
        match decode(&bytes) {
            Decoded::Frame(f, n) => {
                assert_eq!(n, bytes.len());
                assert!(f.fin);
                assert_eq!(f.opcode, Opcode::Text);
                assert_eq!(f.payload, b"{\"op\":\"ping\"}");
            }
            _ => panic!("expected frame"),
        }
    }

    #[test]
    fn handles_two_frames_in_buffer() {
        let mut bytes = client_frame(0x1, b"one", true);
        bytes.extend_from_slice(&client_frame(0x9, b"", true));
        match decode(&bytes) {
            Decoded::Frame(_, n) => {
                let rest = &bytes[n..];
                match decode(rest) {
                    Decoded::Frame(f2, _) => assert_eq!(f2.opcode, Opcode::Ping),
                    _ => panic!("second frame"),
                }
            }
            _ => panic!("first frame"),
        }
    }

    #[test]
    fn unmasked_rejected() {
        // Server-style frame from a client.
        let bytes = vec![0x81, 0x02, b'h', b'i'];
        assert!(matches!(decode(&bytes), Decoded::Violation(1002, _)));
    }

    #[test]
    fn reserved_bits_rejected() {
        let mut b = client_frame(0x1, b"x", true);
        b[0] |= 0x40; // RSV1
        assert!(matches!(decode(&b), Decoded::Violation(1002, _)));
    }

    #[test]
    fn huge_frame_rejected() {
        // Announce a 1 MiB payload.
        let mut bytes = vec![0x81, 0x80 | 127];
        bytes.extend_from_slice(&1_048_576u64.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        assert!(matches!(decode(&bytes), Decoded::Violation(1009, _)));
    }

    #[test]
    fn control_frame_rules() {
        // Fragmented ping (FIN=0).
        let b = client_frame(0x9, b"x", false);
        assert!(matches!(decode(&b), Decoded::Violation(1002, _)));
        // Oversized control: 126-byte ping.
        let big = [0u8; 126];
        let b2 = client_frame(0x9, &big, true);
        assert!(matches!(decode(&b2), Decoded::Violation(1002, _)));
    }

    #[test]
    fn fragmentation_assembly() {
        let mut m = MessageBuf::default();
        let start = client_frame(0x1, b"part1-", false);
        let f1 = match decode(&start) {
            Decoded::Frame(f, _) => f,
            _ => panic!(),
        };
        assert!(matches!(m.feed(f1), MsgResult::NeedMore));
        let cont = client_frame(0x0, b"part2", true);
        let f2 = match decode(&cont) {
            Decoded::Frame(f, _) => f,
            _ => panic!(),
        };
        match m.feed(f2) {
            MsgResult::Message(binary, data) => {
                assert!(!binary);
                assert_eq!(data, b"part1-part2");
            }
            _ => panic!("expected complete message"),
        }
    }

    #[test]
    fn fragmentation_abuse() {
        // Continuation without start.
        let mut m = MessageBuf::default();
        let cont = client_frame(0x0, b"x", true);
        let f = match decode(&cont) {
            Decoded::Frame(f, _) => f,
            _ => panic!(),
        };
        assert!(matches!(m.feed(f), MsgResult::Violation(1002, _)));
        // New data frame during fragmentation.
        let mut m2 = MessageBuf::default();
        let start = client_frame(0x1, b"a", false);
        let fs = match decode(&start) {
            Decoded::Frame(f, _) => f,
            _ => panic!(),
        };
        m2.feed(fs);
        let again = client_frame(0x1, b"b", true);
        let fa = match decode(&again) {
            Decoded::Frame(f, _) => f,
            _ => panic!(),
        };
        assert!(matches!(m2.feed(fa), MsgResult::Violation(1002, _)));
        // Fragment bomb: many continuations exceeding the cap.
        let mut m3 = MessageBuf::default();
        let start = client_frame(0x2, &vec![0u8; 8 * 1024], false);
        let fstart = match decode(&start) {
            Decoded::Frame(f, _) => f,
            _ => panic!(),
        };
        m3.feed(fstart);
        let cont = client_frame(0x0, &vec![0u8; 8 * 1024 + 1], false);
        let fc = match decode(&cont) {
            Decoded::Frame(f, _) => f,
            _ => panic!(),
        };
        assert!(matches!(m3.feed(fc), MsgResult::Violation(1009, _)));
    }

    #[test]
    fn server_encode_roundtrip() {
        let bytes = encode_text("{\"t\":\"bbo\"}");
        // Server frames are unmasked — decode() would reject them as client
        // input; verify structure manually.
        assert_eq!(bytes[0], 0x81);
        assert_eq!(bytes[1] as usize, "{\"t\":\"bbo\"}".len());
        assert_eq!(&bytes[2..], b"{\"t\":\"bbo\"}");
        let big = encode(Opcode::Text, &[b'x'; 200]);
        assert_eq!(big[1], 126);
        assert_eq!(u16::from_be_bytes([big[2], big[3]]), 200);
    }

    #[test]
    fn fuzz_no_panic() {
        let mut state: u64 = 0x0123_4567_89AB_CDEF;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut m = MessageBuf::default();
        for _ in 0..20_000 {
            let len = (next() % 40) as usize;
            let buf: Vec<u8> = (0..len).map(|_| (next() & 0xFF) as u8).collect();
            if let Decoded::Frame(f, _) = decode(&buf) {
                let _ = m.feed(f);
            }
        }
    }
}
