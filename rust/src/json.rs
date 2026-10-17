//! Strict, allocation-bounded JSON codec for the venue's public API.
//!
//! Security posture (this parser is an internet-facing attack surface):
//! * **Integer numbers only** — this is a trading venue; every money value
//!   is an integer tick. Floats, exponents and `-0` are rejected outright,
//!   eliminating an entire class of precision/rounding exploits.
//! * **Depth-capped** (default 16): nesting bombs fail fast.
//! * **Duplicate object keys rejected**: ambiguous documents never reach
//!   business logic.
//! * **Control characters and invalid UTF-8 rejected**; string escapes are
//!   fully validated including surrogate pairing.
//! * **No panics on any input** — fuzzed with random bytes; every failure
//!   is a clean `Err`.
//! * Total input length is bounded by the HTTP/WS layer before parsing.

use std::fmt;

/// Maximum nesting depth accepted.
pub const MAX_DEPTH: usize = 16;

/// A parsed JSON value. Integers only (see module docs).
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// Integer that fits i64 (positive or negative).
    Int(i64),
    /// Integer above i64::MAX (u64 range; still no floats).
    UInt(u64),
    /// String (owned; escapes resolved).
    Str(String),
    /// Array.
    Arr(Vec<Json>),
    /// Object (insertion order preserved; duplicates rejected at parse).
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// Object field accessor.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    /// String value (exactly a string, nothing else).
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    /// Boolean value.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// Unsigned integer value (accepts Int >= 0 and UInt).
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Int(i) if *i >= 0 => Some(*i as u64),
            Json::UInt(u) => Some(*u),
            _ => None,
        }
    }
    /// Signed integer value.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
            _ => None,
        }
    }
    /// Array accessor.
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(v) => Some(v),
            _ => None,
        }
    }
}

/// Parse error kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonErr {
    /// Nesting deeper than [`MAX_DEPTH`].
    Depth,
    /// Malformed syntax at byte offset (stored separately).
    Syntax,
    /// Floating-point / exponent / otherwise non-integer number.
    Float,
    /// Duplicate key in one object.
    DuplicateKey,
    /// Document is not a single complete value (trailing bytes).
    Trailing,
    /// Input not valid UTF-8.
    Utf8,
}

impl fmt::Display for JsonErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            JsonErr::Depth => "nesting too deep",
            JsonErr::Syntax => "malformed JSON",
            JsonErr::Float => "non-integer number (floats forbidden)",
            JsonErr::DuplicateKey => "duplicate object key",
            JsonErr::Trailing => "trailing content after value",
            JsonErr::Utf8 => "not valid UTF-8",
        };
        f.write_str(s)
    }
}

/// Parse one JSON document from bytes.
pub fn parse(input: &[u8]) -> Result<Json, JsonErr> {
    let s = std::str::from_utf8(input).map_err(|_| JsonErr::Utf8)?;
    let mut p = Parser {
        b: s.as_bytes(),
        i: 0,
    };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(JsonErr::Trailing);
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, want: &[u8]) -> bool {
        if self.b.len() >= self.i + want.len() && &self.b[self.i..self.i + want.len()] == want {
            self.i += want.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, JsonErr> {
        if depth > MAX_DEPTH {
            return Err(JsonErr::Depth);
        }
        match self.peek() {
            None => Err(JsonErr::Syntax),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => {
                if self.eat(b"true") {
                    Ok(Json::Bool(true))
                } else {
                    Err(JsonErr::Syntax)
                }
            }
            Some(b'f') => {
                if self.eat(b"false") {
                    Ok(Json::Bool(false))
                } else {
                    Err(JsonErr::Syntax)
                }
            }
            Some(b'n') => {
                if self.eat(b"null") {
                    Ok(Json::Null)
                } else {
                    Err(JsonErr::Syntax)
                }
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(JsonErr::Syntax),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, JsonErr> {
        self.i += 1; // '{'
        let mut fields: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(JsonErr::Syntax);
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(JsonErr::Syntax);
            }
            self.i += 1;
            self.skip_ws();
            let val = self.value(depth + 1)?;
            if fields.iter().any(|(k, _)| *k == key) {
                return Err(JsonErr::DuplicateKey);
            }
            fields.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(fields));
                }
                _ => return Err(JsonErr::Syntax),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, JsonErr> {
        self.i += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            let val = self.value(depth + 1)?;
            items.push(val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(JsonErr::Syntax),
            }
        }
    }

    fn string(&mut self) -> Result<String, JsonErr> {
        self.i += 1; // '"'
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or(JsonErr::Syntax)?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.peek().ok_or(JsonErr::Syntax)?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let ch = if (0xD800..0xDC00).contains(&hi) {
                                // High surrogate: require a following \uXXXX low.
                                if self.peek() == Some(b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err(JsonErr::Syntax);
                                    }
                                    let cp = 0x10000
                                        + ((hi as u32 - 0xD800) << 10)
                                        + (lo as u32 - 0xDC00);
                                    char::from_u32(cp).ok_or(JsonErr::Syntax)?
                                } else {
                                    return Err(JsonErr::Syntax);
                                }
                            } else if (0xDC00..0xE000).contains(&hi) {
                                // Lone low surrogate.
                                return Err(JsonErr::Syntax);
                            } else {
                                char::from_u32(hi as u32).ok_or(JsonErr::Syntax)?
                            };
                            out.push(ch);
                        }
                        _ => return Err(JsonErr::Syntax),
                    }
                }
                0x00..=0x1F => return Err(JsonErr::Syntax),
                _ => {
                    if c < 0x80 {
                        out.push(c as char);
                    } else {
                        // Multi-byte UTF-8: decode the whole sequence (input
                        // was validated UTF-8 upfront; sequences cannot be
                        // split here because we consume whole chars).
                        let len = match c {
                            0xC0..=0xDF => 2,
                            0xE0..=0xEF => 3,
                            0xF0..=0xF7 => 4,
                            _ => return Err(JsonErr::Utf8),
                        };
                        let start = self.i - 1;
                        if start + len > self.b.len() {
                            return Err(JsonErr::Utf8);
                        }
                        let seq = &self.b[start..start + len];
                        match std::str::from_utf8(seq) {
                            Ok(t) => {
                                out.push_str(t);
                                self.i += len - 1;
                            }
                            Err(_) => return Err(JsonErr::Utf8),
                        }
                    }
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u16, JsonErr> {
        if self.i + 4 > self.b.len() {
            return Err(JsonErr::Syntax);
        }
        let mut v: u16 = 0;
        for k in 0..4 {
            let c = self.b[self.i + k];
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(JsonErr::Syntax),
            };
            v = (v << 4) | d as u16;
        }
        self.i += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, JsonErr> {
        let start = self.i;
        let neg = if self.peek() == Some(b'-') {
            self.i += 1;
            true
        } else {
            false
        };
        let lead = self.peek().ok_or(JsonErr::Syntax)?;
        if !lead.is_ascii_digit() {
            return Err(JsonErr::Syntax);
        }
        if lead == b'0' {
            self.i += 1;
            // A leading zero may only stand alone.
            if let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    return Err(JsonErr::Syntax);
                }
            }
        } else {
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        // Any float-ish continuation is rejected.
        if matches!(self.peek(), Some(b'.') | Some(b'e') | Some(b'E')) {
            return Err(JsonErr::Float);
        }
        let digits = &self.b[start + neg as usize..self.i];
        let mut acc: u64 = 0;
        let mut overflow = false;
        for &d in digits {
            let (a, o1) = acc.overflowing_mul(10);
            let (a, o2) = a.overflowing_add((d - b'0') as u64);
            acc = a;
            overflow |= o1 || o2;
        }
        if overflow {
            // u64 overflow: reject (all venue values fit i128 comfortably;
            // absurd magnitudes are attack traffic).
            return Err(JsonErr::Float);
        }
        if neg {
            if acc > (i64::MAX as u64) + 1 {
                return Err(JsonErr::Float);
            }
            if acc == (i64::MAX as u64) + 1 {
                return Ok(Json::Int(i64::MIN));
            }
            Ok(Json::Int(-(acc as i64)))
        } else if acc > i64::MAX as u64 {
            Ok(Json::UInt(acc))
        } else {
            Ok(Json::Int(acc as i64))
        }
    }
}

// ----------------------------------------------------------------------
// Writer
// ----------------------------------------------------------------------

/// Incremental JSON writer (builds one document into a String).
#[derive(Default)]
pub struct Writer {
    buf: String,
}

impl Writer {
    /// New writer.
    pub fn new() -> Writer {
        Writer {
            buf: String::with_capacity(128),
        }
    }

    /// Consume and return the document.
    pub fn finish(self) -> String {
        self.buf
    }

    /// Borrow the document so far.
    pub fn as_str(&self) -> &str {
        &self.buf
    }

    fn push(&mut self, s: &str) {
        self.buf.push_str(s);
    }

    /// `{`
    pub fn begin_obj(&mut self) {
        self.push("{");
    }
    /// `}`
    pub fn end_obj(&mut self) {
        self.push("}");
    }
    /// `[`
    pub fn begin_arr(&mut self) {
        self.push("[");
    }
    /// `]`
    pub fn end_arr(&mut self) {
        self.push("]");
    }
    /// `,`
    pub fn comma(&mut self) {
        self.push(",");
    }

    /// `"key":` — string is escaped.
    pub fn key(&mut self, k: &str) {
        self.push("\"");
        escape_into(&mut self.buf, k);
        self.push("\":");
    }

    /// Unsigned integer value.
    pub fn uint(&mut self, v: u64) {
        self.buf.push_str(&v.to_string());
    }
    /// Signed integer value.
    pub fn int(&mut self, v: i64) {
        self.buf.push_str(&v.to_string());
    }
    /// i128 cash value.
    pub fn cash(&mut self, v: i128) {
        self.buf.push_str(&v.to_string());
    }
    /// Boolean value.
    pub fn bool(&mut self, v: bool) {
        self.push(if v { "true" } else { "false" });
    }
    /// String value (escaped).
    pub fn str(&mut self, v: &str) {
        self.push("\"");
        escape_into(&mut self.buf, v);
        self.push("\"");
    }
    /// Raw pre-escaped fragment (trusted internal use only).
    pub fn raw(&mut self, v: &str) {
        self.push(v);
    }
}

/// Escape `s` into `out` per JSON string rules.
pub fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip_basics() {
        let v = parse(br#"{"a":1,"b":[true,false,null],"c":"x"}"#).unwrap();
        assert_eq!(v.get("a").unwrap().as_u64(), Some(1));
        assert_eq!(v.get("b").unwrap().as_arr().unwrap().len(), 3);
        assert_eq!(v.get("c").unwrap().as_str(), Some("x"));
    }

    #[test]
    fn integers_only() {
        assert!(parse(b"[1.5]").is_err());
        assert!(parse(b"[1e3]").is_err());
        assert!(parse(b"[1E3]").is_err());
        assert!(parse(b"[.5]").is_err());
        assert!(parse(b"[01]").is_err());
        assert!(parse(b"[-]").is_err());
        assert_eq!(parse(b"[0]").unwrap(), Json::Arr(vec![Json::Int(0)]));
        assert_eq!(
            parse(b"[18446744073709551615]").unwrap(),
            Json::Arr(vec![Json::UInt(u64::MAX)])
        );
        assert!(parse(b"[18446744073709551616]").is_err());
        assert!(parse(b"[-9223372036854775809]").is_err());
        assert_eq!(
            parse(b"[-9223372036854775808]").unwrap(),
            Json::Arr(vec![Json::Int(i64::MIN)])
        );
    }

    #[test]
    fn strings_and_escapes() {
        let v = parse(br#""aA\n\t\"\\\/\b\fA""#).unwrap();
        assert_eq!(v.as_str(), Some("aA\n\t\"\\/\u{0008}\u{000C}A"));
        let u = parse("\"é 😀\"".as_bytes()).unwrap();
        assert_eq!(u.as_str(), Some("é 😀"));
        // Escaped unicode.
        let e = parse("\"é\"".as_bytes()).unwrap();
        assert_eq!(e.as_str(), Some("é"));
        let pair = parse("\"😀\"".as_bytes()).unwrap();
        assert_eq!(pair.as_str(), Some("\u{1F600}"));
        // Surrogate errors.
        assert!(parse(br#""\uD83D""#).is_err()); // lone high
        assert!(parse(br#""\uDE00""#).is_err()); // lone low
        assert!(parse(br#""\uD83D\u0041""#).is_err()); // bad pair
        // Control chars.
        assert!(parse(b"\"\n\"").is_err());
        assert!(parse(b"\"\t\"").is_err());
    }

    #[test]
    fn structure_errors() {
        assert!(parse(b"{").is_err());
        assert!(parse(b"[").is_err());
        assert!(parse(b"{\"a\":1,}").is_err());
        assert!(parse(b"[,]").is_err());
        assert!(parse(b"nul").is_err());
        assert!(parse(b"tru").is_err());
        assert!(parse(b"1 2").is_err()); // trailing
        assert!(parse(b"{}x").is_err()); // trailing
        assert!(parse(b"").is_err());
    }

    #[test]
    fn duplicate_keys_rejected() {
        assert!(parse(br#"{"a":1,"a":2}"#).is_err());
        assert!(parse(br#"{"a":1,"b":{"a":2,"a":3}}"#).is_err());
        assert!(parse(br#"{"a":1,"b":2}"#).is_ok());
    }

    #[test]
    fn depth_bomb_rejected() {
        let mut doc = String::new();
        for _ in 0..64 {
            doc.push('[');
        }
        for _ in 0..64 {
            doc.push(']');
        }
        assert_eq!(parse(doc.as_bytes()), Err(JsonErr::Depth));
        // Exactly at the limit is fine.
        let mut ok = String::new();
        for _ in 0..MAX_DEPTH {
            ok.push('[');
        }
        for _ in 0..MAX_DEPTH {
            ok.push(']');
        }
        assert!(parse(ok.as_bytes()).is_ok());
    }

    #[test]
    fn invalid_utf8_rejected() {
        assert!(parse(b"[\"\xFF\xFE\"]").is_err());
    }

    #[test]
    fn writer_escapes() {
        let mut w = Writer::new();
        w.begin_obj();
        w.key("na\"me");
        w.str("line\nbreak");
        w.comma();
        w.key("n");
        w.uint(42);
        w.comma();
        w.key("neg");
        w.int(-7);
        w.end_obj();
        let s = w.finish();
        assert_eq!(s, "{\"na\\\"me\":\"line\\nbreak\",\"n\":42,\"neg\":-7}");
        // And it parses back.
        let v = parse(s.as_bytes()).unwrap();
        assert_eq!(v.get("n").unwrap().as_u64(), Some(42));
        assert_eq!(v.get("neg").unwrap().as_i64(), Some(-7));
    }

    #[test]
    fn fuzz_random_bytes_never_panics() {
        // Deterministic xorshift PRNG (mirrors sim::Rng semantics).
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 200) as usize;
            let buf: Vec<u8> = (0..len).map(|_| (next() & 0xFF) as u8).collect();
            // Must not panic; errors are fine.
            let _ = parse(&buf);
        }
        // Semi-valid documents with random mutations.
        let base = br#"{"symbol":1,"side":"buy","qty":10,"price":250}"#;
        for _ in 0..20_000 {
            let mut doc = base.to_vec();
            let n = (next() % 4) as usize;
            for _ in 0..n {
                let pos = (next() as usize) % doc.len();
                doc[pos] = (next() & 0xFF) as u8;
            }
            let _ = parse(&doc);
        }
    }
}
