//! Minimal JSON codec (RFC 8259 subset, writer-focused).
//!
//! The gateway embeds this to serialize WS/REST payloads without pulling
//! serde into the workspace (the engine stays dependency-free; a
//! production deployment can swap in serde behind this same shape).
//! Numbers serialize exactly as integers when they are, and as
//! formatted decimals otherwise — money never round-trips through f64
//! at this layer.

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// Boolean.
    Bool(bool),
    /// Integer (all engine money and ids).
    Int(i128),
    /// Floating-point (analytics only: greeks, IV).
    Num(f64),
    /// String.
    Str(String),
    /// Array.
    Array(Vec<Json>),
    /// Object — BTreeMap keeps key order deterministic (replayable logs).
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Build an object from key-value pairs.
    #[must_use]
    pub fn obj(pairs: Vec<(&str, Json)>) -> Self {
        Json::Object(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }

    /// Serialize to a compact JSON string.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(i) => {
                out.push_str(&i.to_string());
            }
            Json::Num(x) => {
                if x.is_finite() {
                    if x.fract() == 0.0 && x.abs() < 1e15 {
                        out.push_str(&format!("{}", *x as i64));
                    } else {
                        out.push_str(&format!("{x}"));
                    }
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => write_escaped(out, s),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(out, k);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_encode() {
        assert_eq!(Json::Null.encode(), "null");
        assert_eq!(Json::Bool(true).encode(), "true");
        assert_eq!(Json::Int(-42).encode(), "-42");
        assert_eq!(Json::Str("hi".into()).encode(), "\"hi\"");
    }

    #[test]
    fn escaping_is_correct() {
        assert_eq!(
            Json::Str("a\"b\\c\nd".into()).encode(),
            "\"a\\\"b\\\\c\\nd\""
        );
        assert_eq!(Json::Str("\u{1}".into()).encode(), "\"\\u0001\"");
    }

    #[test]
    fn composites_encode() {
        let v = Json::obj(vec![
            ("symbol", Json::Str("BTC-PERP".into())),
            ("best_bid", Json::Int(79_950)),
            ("best_ask", Json::Null),
            (
                "greeks",
                Json::Array(vec![Json::Num(0.5), Json::Num(-12.25)]),
            ),
        ]);
        assert_eq!(
            v.encode(),
            "{\"symbol\":\"BTC-PERP\",\"best_bid\":79950,\"best_ask\":null,\"greeks\":[0.5,-12.25]}"
        );
    }

    #[test]
    fn nan_becomes_null_never_invalid_json() {
        assert_eq!(Json::Num(f64::NAN).encode(), "null");
        assert_eq!(Json::Num(f64::INFINITY).encode(), "null");
    }

    #[test]
    fn object_key_order_is_deterministic() {
        let a = Json::obj(vec![("x", Json::Int(1)), ("y", Json::Int(2))]);
        let b = Json::obj(vec![("x", Json::Int(1)), ("y", Json::Int(2))]);
        assert_eq!(a.encode(), b.encode());
    }
}
