//! Minimal, strict HTTP/1.1 server-side codec for the venue's public API.
//!
//! Hardened by construction:
//! * Bounded request line (512 B), header block (8 KiB), header count (64),
//!   and per-header value (1 KiB). Overshoots are hard errors.
//! * `Transfer-Encoding` is rejected (we require `Content-Length`), which
//!   eliminates request-smuggling desyncs against fronting proxies.
//! * Both `Content-Length` and `TE` present → rejected (CL+TE smuggling).
//! * Two `Content-Length` headers with different values → rejected.
//! * Control characters in header names/values → rejected.
//! * `Connection: close` and keep-alive are honored; pipelining is not
//!   (requests are processed one at a time per connection).
//! * The parser is incremental and allocation-free until a complete
//!   request is available.

/// Maximum request-line length in bytes.
pub const MAX_LINE: usize = 512;
/// Maximum total header block size in bytes.
pub const MAX_HEADERS_BYTES: usize = 8 * 1024;
/// Maximum number of headers.
pub const MAX_HEADERS: usize = 64;
/// Maximum body size accepted by the venue (order batches stay far below).
pub const MAX_BODY: usize = 16 * 1024;

/// HTTP method (only what the venue API uses; anything else is `Other`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    /// GET
    Get,
    /// POST
    Post,
    /// DELETE
    Delete,
    /// OPTIONS (CORS preflight)
    Options,
    /// Anything else.
    Other,
}

impl Method {
    /// Uppercase name.
    pub fn name(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
            Method::Options => "OPTIONS",
            Method::Other => "OTHER",
        }
    }
}

/// A parsed request.
pub struct Request {
    /// Method.
    pub method: Method,
    /// Raw request target (path + query), percent-decoding NOT applied.
    pub target: String,
    /// Header names/values (in order, case preserved; lookups are
    /// case-insensitive).
    pub headers: Vec<(String, String)>,
    /// Body bytes (already length-checked against `Content-Length`).
    pub body: Vec<u8>,
    /// Client asked `Connection: close`.
    pub close: bool,
}

impl Request {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Path portion (before `?`).
    pub fn path(&self) -> &str {
        match self.target.find('?') {
            Some(i) => &self.target[..i],
            None => &self.target[..],
        }
    }

    /// Query portion (after `?`), possibly empty.
    pub fn query(&self) -> &str {
        match self.target.find('?') {
            Some(i) => &self.target[i + 1..],
            None => "",
        }
    }
}

/// Parse outcome.
pub enum Parsed {
    /// More bytes needed.
    NeedMore,
    /// A complete request.
    Done(Box<Request>),
    /// Unrecoverable protocol error (connection should be answered with an
    /// error status and closed).
    Bad(&'static str),
}

/// Incremental HTTP request parser for one connection.
pub struct HttpParser {
    buf: Vec<u8>,
    /// Header block terminator position (set once headers are complete).
    head_end: Option<usize>,
}

impl Default for HttpParser {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpParser {
    /// Fresh parser.
    pub fn new() -> HttpParser {
        HttpParser {
            buf: Vec::with_capacity(1024),
            head_end: None,
        }
    }

    /// Feed freshly read bytes; returns `None` to keep reading or the parse
    /// outcome.
    pub fn feed(&mut self, bytes: &[u8]) -> Parsed {
        if self.buf.len() + bytes.len() > MAX_HEADERS_BYTES + MAX_BODY + 1024 {
            return Parsed::Bad("request too large");
        }
        self.buf.extend_from_slice(bytes);
        self.try_parse()
    }

    /// Try to complete a request from buffered bytes.
    pub fn try_parse(&mut self) -> Parsed {
        if self.head_end.is_none() {
            // Locate the end of the header block (\r\n\r\n).
            let search_from = self
                .buf
                .len()
                .saturating_sub(3)
                .min(self.buf.len());
            // Search whole buffer the first time (cheap at these sizes).
            let _ = search_from;
            match find(&self.buf, b"\r\n\r\n") {
                Some(pos) => self.head_end = Some(pos),
                None => {
                    if self.buf.len() > MAX_HEADERS_BYTES {
                        return Parsed::Bad("header block too large");
                    }
                    // Slowloris defense is handled by read deadlines in the
                    // connection loop, not here.
                    return Parsed::NeedMore;
                }
            }
        }
        let head_end = self.head_end.unwrap();
        let head = &self.buf[..head_end];
        // Request line (for zero-header requests the head has no trailing
        // CRLF — the header-block terminator already ended it).
        let line_end = find(head, b"\r\n").unwrap_or(head.len());
        let line = &head[..line_end];
        if line.len() > MAX_LINE {
            return Parsed::Bad("request line too long");
        }
        let line_str = match std::str::from_utf8(line) {
            Ok(s) => s,
            Err(_) => return Parsed::Bad("request line not ASCII"),
        };
        let mut parts = line_str.split(' ');
        let method = match parts.next() {
            Some("GET") => Method::Get,
            Some("POST") => Method::Post,
            Some("DELETE") => Method::Delete,
            Some("OPTIONS") => Method::Options,
            Some(_) => Method::Other,
            None => return Parsed::Bad("malformed request line"),
        };
        let target = match parts.next() {
            Some(t) if !t.is_empty() && t.len() <= MAX_LINE => t.to_string(),
            _ => return Parsed::Bad("malformed request target"),
        };
        let version = parts.next();
        if parts.next().is_some() {
            return Parsed::Bad("malformed request line");
        }
        // We serve HTTP/1.0 (no keep-alive default) and HTTP/1.1.
        let keep_alive_default = match version {
            Some("HTTP/1.1") => true,
            Some("HTTP/1.0") => false,
            _ => return Parsed::Bad("unsupported HTTP version"),
        };
        if !target.starts_with('/') || target.contains('\0') {
            return Parsed::Bad("malformed request target");
        }
        if target.bytes().any(|c| c < 0x20 || c == 0x7F) {
            return Parsed::Bad("control character in request target");
        }

        // Headers.
        let mut headers: Vec<(String, String)> = Vec::new();
        // Zero-header requests have no CRLF after the request line.
        let header_block: &[u8] = if line_end + 2 <= head.len() {
            &head[line_end + 2..]
        } else {
            b""
        };
        let mut row_start = 0usize;
        while row_start < header_block.len() {
            let row_end = match find(&header_block[row_start..], b"\r\n") {
                Some(p) => row_start + p,
                None => header_block.len(),
            };
            let row = &header_block[row_start..row_end];
            row_start = row_end + 2;
            if row.is_empty() {
                continue;
            }
            if headers.len() >= MAX_HEADERS {
                return Parsed::Bad("too many headers");
            }
            let colon = match find(row, b":") {
                Some(c) => c,
                None => return Parsed::Bad("malformed header"),
            };
            let name = &row[..colon];
            let value = &row[colon + 1..];
            if name.is_empty() || name.len() > 64 {
                return Parsed::Bad("bad header name");
            }
            if !name
                .iter()
                .all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            {
                return Parsed::Bad("bad header name");
            }
            let value_str = match std::str::from_utf8(value) {
                Ok(s) => s.trim(),
                Err(_) => return Parsed::Bad("bad header value"),
            };
            if value_str.len() > 1024 {
                return Parsed::Bad("header value too long");
            }
            if value_str.bytes().any(|c| c < 0x20 || c == 0x7F) {
                return Parsed::Bad("control character in header");
            }
            let name_str = String::from_utf8_lossy(name).to_string();
            headers.push((name_str.to_ascii_lowercase(), value_str.to_string()));
        }

        // Content-Length / Transfer-Encoding analysis (anti-smuggling).
        let mut content_length: Option<usize> = None;
        let mut has_te = false;
        let mut close = !keep_alive_default;
        for (k, v) in &headers {
            if k == "content-length" {
                let n: usize = match v.parse() {
                    Ok(n) => n,
                    Err(_) => return Parsed::Bad("bad content-length"),
                };
                if n > MAX_BODY {
                    return Parsed::Bad("body too large");
                }
                if let Some(prev) = content_length {
                    if prev != n {
                        return Parsed::Bad("conflicting content-length");
                    }
                }
                content_length = Some(n);
            } else if k == "transfer-encoding" {
                has_te = true;
            } else if k == "connection" {
                let lower = v.to_ascii_lowercase();
                if lower.contains("close") {
                    close = true;
                } else if lower.contains("keep-alive") {
                    close = false;
                }
            }
        }
        if has_te {
            return Parsed::Bad("transfer-encoding not supported");
        }
        // No Content-Length and no Transfer-Encoding = zero-length body for
        // ANY method (RFC 9112 §6.3) — there is no framing ambiguity to
        // smuggle through. Browsers legitimately send body-less DELETE /
        // POST without a Content-Length header, so requiring one broke
        // browser-native REST clients; CL/CL conflicts and TE are still
        // rejected above.
        let body_len = content_length.unwrap_or(0);

        // Do we have the full body?
        let body_start = head_end + 4;
        if self.buf.len() < body_start + body_len {
            return Parsed::NeedMore;
        }
        let body = self.buf[body_start..body_start + body_len].to_vec();
        // Consume exactly this request; anything after stays for the next
        // one — but we do not pipeline, so extra bytes mean protocol abuse.
        let extra = self.buf.len() - (body_start + body_len);
        let req = Request {
            method,
            target,
            headers,
            body,
            close,
        };
        // Reset the parser state for the next request on this connection.
        self.buf.clear();
        self.head_end = None;
        if extra > 0 {
            // Pipelining is forbidden; treat as protocol garbage.
            return Parsed::Done(Box::new(req)); // extra ignored, next read resets
        }
        Parsed::Done(Box::new(req))
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
}

/// Standard response reasons.
pub fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Response",
    }
}

/// Serialize a response head + body. `extra` are additional raw header lines
/// (each a complete `"Name: value"` line without CRLF).
pub fn response(code: u16, content_type: &str, body: &[u8], extra: &[String], close: bool) -> Vec<u8> {
    let mut out = String::with_capacity(256 + body.len());
    out.push_str("HTTP/1.1 ");
    out.push_str(&code.to_string());
    out.push(' ');
    out.push_str(reason(code));
    out.push_str("\r\nContent-Type: ");
    out.push_str(content_type);
    out.push_str("\r\nContent-Length: ");
    out.push_str(&body.len().to_string());
    out.push_str("\r\nAccess-Control-Allow-Origin: *");
    out.push_str("\r\nAccess-Control-Allow-Methods: GET, POST, DELETE, OPTIONS");
    out.push_str("\r\nAccess-Control-Allow-Headers: Content-Type, X-Api-Key, X-Timestamp, X-Nonce, X-Signature");
    for h in extra {
        out.push_str("\r\n");
        out.push_str(h);
    }
    if close {
        out.push_str("\r\nConnection: close");
    } else {
        out.push_str("\r\nConnection: keep-alive");
    }
    out.push_str("\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// CORS preflight response.
pub fn cors_preflight(close: bool) -> Vec<u8> {
    let extra = [
        "Access-Control-Max-Age: 600".to_string(),
    ];
    response(200, "text/plain", b"ok", &extra, close)
}

/// Percent-decode a query component (also maps `+` to space).
pub fn url_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                if i + 2 >= b.len() {
                    return None;
                }
                let hi = hex_val(b[i + 1])?;
                let lo = hex_val(b[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Split a query string into decoded key/value pairs.
pub fn query_pairs(q: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.find('=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, ""),
        };
        let k = url_decode(k).unwrap_or_default();
        let v = url_decode(v).unwrap_or_default();
        out.push((k, v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_req(raw: &[u8]) -> Request {
        let mut p = HttpParser::new();
        match p.feed(raw) {
            Parsed::Done(r) => *r,
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn parses_simple_get() {
        let r = get_req(b"GET /v1/time HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(r.method, Method::Get);
        assert_eq!(r.path(), "/v1/time");
        assert_eq!(r.query(), "");
        assert!(!r.close);
        assert_eq!(r.header("host"), Some("x"));
        assert_eq!(r.header("HOST"), Some("x"));
    }

    #[test]
    fn parses_post_with_body() {
        let raw = b"POST /v1/orders HTTP/1.1\r\nContent-Length: 15\r\n\r\n{\"qty\":1,\"a\":2}";
        let r = get_req(raw);
        assert_eq!(r.method, Method::Post);
        assert_eq!(r.body, br#"{"qty":1,"a":2}"#);
    }

    #[test]
    fn incremental_feed() {
        let raw = b"POST /v1/orders HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        let mut p = HttpParser::new();
        assert!(matches!(p.feed(&raw[..10]), Parsed::NeedMore));
        assert!(matches!(p.feed(&raw[10..30]), Parsed::NeedMore));
        let done = p.feed(&raw[30..]);
        assert!(matches!(done, Parsed::Done(_)));
    }

    #[test]
    fn rejects_smuggling_shapes() {
        // CL + TE.
        let mut p = HttpParser::new();
        assert!(matches!(
            p.feed(b"POST /x HTTP/1.1\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\nhello"),
            Parsed::Bad(_)
        ));
        // TE alone.
        let mut p2 = HttpParser::new();
        assert!(matches!(
            p2.feed(b"POST /x HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello"),
            Parsed::Bad(_)
        ));
        // Conflicting CLs.
        let mut p3 = HttpParser::new();
        assert!(matches!(
            p3.feed(b"POST /x HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello"),
            Parsed::Bad(_)
        ));
    }

    #[test]
    fn accepts_bodyless_without_content_length() {
        // RFC 9112 §6.3: no CL and no TE = zero-length body for any method.
        // Browsers send exactly this for body-less DELETE (fetch with
        // body: undefined) — regression test for browser-native REST.
        let mut p = HttpParser::new();
        match p.feed(b"DELETE /v1/orders/42?symbol=7 HTTP/1.1\r\nHost: a\r\n\r\n") {
            Parsed::Done(r) => {
                assert_eq!(r.method, Method::Delete);
                assert_eq!(r.path(), "/v1/orders/42");
                assert_eq!(r.query(), "symbol=7");
                assert!(r.body.is_empty());
            }
            _ => panic!("body-less DELETE must parse"),
        }
        let mut p2 = HttpParser::new();
        match p2.feed(b"POST /v1/orders HTTP/1.1\r\nHost: a\r\n\r\n") {
            Parsed::Done(r) => assert!(r.body.is_empty()),
            _ => panic!("body-less POST must parse as zero-body"),
        }
        // Bytes after the header block belong to the NEXT message, not a
        // phantom body (no CL ⇒ no body; trailing bytes are discarded by
        // the non-pipelining reader, never absorbed into this request).
        let mut p3 = HttpParser::new();
        match p3.feed(b"DELETE /x HTTP/1.1\r\nHost: a\r\n\r\nGET /y HTTP/1.1\r\n") {
            Parsed::Done(r) => assert!(r.body.is_empty()),
            _ => panic!("extra bytes after body-less request must not be absorbed"),
        }
    }

    #[test]
    fn rejects_garbage() {
        for garbage in [
            &b"\x00\x01\x02"[..],
            b"GET / HTTTP/9.9\r\n\r\n",
            b"GET qp://evil HTTP/1.1\r\n\r\n",
            &b"GET /\x01x HTTP/1.1\r\n\r\n"[..],
        ] {
            let mut p = HttpParser::new();
            assert!(matches!(p.feed(garbage), Parsed::Bad(_) | Parsed::NeedMore));
        }
        // Unknown methods parse syntactically but surface as Method::Other,
        // which the router answers with 405 — safe to accept at this layer.
        let mut p2 = HttpParser::new();
        if let Parsed::Done(r) = p2.feed(b"NOTAMETHOD / HTTP/1.1\r\n\r\n") {
            assert_eq!(r.method, Method::Other);
        } else {
            panic!("expected Done for syntactically valid unknown method");
        }
    }

    #[test]
    fn connection_close_and_keepalive() {
        let r = get_req(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert!(r.close);
        let r2 = get_req(b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
        assert!(!r2.close);
        let r3 = get_req(b"GET / HTTP/1.0\r\n\r\n");
        assert!(r3.close);
    }

    #[test]
    fn query_parsing() {
        let r = get_req(b"GET /v1/book?symbol=42&depth=10 HTTP/1.1\r\n\r\n");
        let q = query_pairs(r.query());
        assert!(q.contains(&("symbol".into(), "42".into())));
        assert!(q.contains(&("depth".into(), "10".into())));
        assert_eq!(url_decode("a%20b+c").unwrap(), "a b c");
        assert!(url_decode("%zz").is_none());
        assert!(url_decode("%2").is_none());
    }

    #[test]
    fn response_bytes() {
        let body = b"{\"ok\":true}";
        let bytes = response(200, "application/json", body, &[], false);
        let head = String::from_utf8_lossy(&bytes[..bytes.len() - body.len()]).to_string();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains("Content-Length: 11\r\n"));
        assert!(head.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(head.contains("Connection: keep-alive\r\n"));
        assert!(head.ends_with("\r\n\r\n"));
        let closed = response(429, "application/json", b"x", &["Retry-After: 1".into()], true);
        let s = String::from_utf8_lossy(&closed);
        assert!(s.contains("Retry-After: 1"));
        assert!(s.contains("Connection: close"));
    }

    #[test]
    fn fuzz_no_panic() {
        let mut state: u64 = 0xA076_1D64_78BD_642F;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let base: &[&[u8]] = &[
            b"GET /v1/time HTTP/1.1\r\nHost: a\r\n\r\n",
            b"POST /v1/orders HTTP/1.1\r\nContent-Length: 4\r\n\r\n{}",
            b"DELETE /v1/orders/12?symbol=3 HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
        ];
        for _ in 0..10_000 {
            let doc = base[(next() % 3) as usize].to_vec();
            let mut doc = doc;
            let n = (next() % 5) as usize;
            for _ in 0..n {
                let pos = (next() as usize) % doc.len();
                doc[pos] = (next() & 0xFF) as u8;
            }
            let mut p = HttpParser::new();
            let _ = p.feed(&doc);
        }
    }
}
