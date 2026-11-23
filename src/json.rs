//! A small JSON reader and writer, enough for GG's preset blocks and Platinum's own files.
//!
//! Platinum has no dependency, so this replaces a JSON crate. Objects keep their keys in the order
//! they were read or built, so a preset written back has GG's field order. Numbers are `f64`, like
//! in JavaScript, which is what GG's interface uses.

use std::fmt::{self, Write as _};

/// A JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    /// Members in their order; a key read twice keeps both, and `get` returns the first.
    Object(Vec<(String, Json)>),
}

/// Nesting deeper than this is refused, so that a hostile file cannot overflow the stack.
const MAX_DEPTH: usize = 128;

impl Json {
    /// Parses a whole document. A leading UTF-8 byte-order mark, as Notepad writes, is skipped.
    pub fn parse(text: &str) -> Result<Json, String> {
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
        let mut parser = Parser { bytes: text.as_bytes(), text, pos: 0 };
        parser.skip_space();
        let value = parser.value(0)?;
        parser.skip_space();
        if parser.pos != parser.bytes.len() {
            return Err(parser.error("unexpected text after the value"));
        }
        Ok(value)
    }

    /// The member `key` of an object; `None` for a missing key or a value that is not an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        if let Json::Number(n) = self { Some(*n) } else { None }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if let Json::Bool(b) = self { Some(*b) } else { None }
    }

    pub fn as_str(&self) -> Option<&str> {
        if let Json::String(s) = self { Some(s) } else { None }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        if let Json::Array(items) = self { Some(items) } else { None }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        if let Json::Object(members) = self { Some(members) } else { None }
    }

    /// The value on one line, without spaces (`to_string()` gives the same).
    pub fn to_compact(&self) -> String {
        self.to_string()
    }

    /// The value indented by two spaces per level, as GG stores most presets.
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write_pretty(&self, out: &mut String, level: usize) {
        let indent = |out: &mut String, level: usize| out.extend(std::iter::repeat_n("  ", level));
        match self {
            Json::Array(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    indent(out, level + 1);
                    item.write_pretty(out, level + 1);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                indent(out, level);
                out.push(']');
            }
            Json::Object(members) if !members.is_empty() => {
                out.push_str("{\n");
                for (i, (key, value)) in members.iter().enumerate() {
                    indent(out, level + 1);
                    write_string(out, key);
                    out.push_str(": ");
                    value.write_pretty(out, level + 1);
                    out.push_str(if i + 1 < members.len() { ",\n" } else { "\n" });
                }
                indent(out, level);
                out.push('}');
            }
            other => {
                let _ = write!(out, "{other}");
            }
        }
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Json::Null => f.write_str("null"),
            Json::Bool(b) => write!(f, "{b}"),
            // JSON has no NaN or infinity; null is what JavaScript writes for them.
            Json::Number(n) if !n.is_finite() => f.write_str("null"),
            Json::Number(n) => write!(f, "{n}"),
            Json::String(s) => {
                let mut out = String::new();
                write_string(&mut out, s);
                f.write_str(&out)
            }
            Json::Array(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Json::Object(members) => {
                f.write_str("{")?;
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    let mut name = String::new();
                    write_string(&mut name, key);
                    write!(f, "{name}:{value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

/// Writes `text` as a JSON string. Characters above ASCII are written as they are (UTF-8).
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        format!("invalid JSON at byte {}: {what}", self.pos)
    }

    fn skip_space(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.pos) {
            self.pos += 1;
        }
    }

    fn expect_word(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.text[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("unknown word"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        match self.bytes.get(self.pos) {
            None => Err(self.error("unexpected end")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Json::String),
            Some(b't') => self.expect_word("true", Json::Bool(true)),
            Some(b'f') => self.expect_word("false", Json::Bool(false)),
            Some(b'n') => self.expect_word("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.pos += 1;
        let mut members = Vec::new();
        self.skip_space();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.skip_space();
            if self.bytes.get(self.pos) != Some(&b'"') {
                return Err(self.error("expected a key in quotes"));
            }
            let key = self.string()?;
            self.skip_space();
            if self.bytes.get(self.pos) != Some(&b':') {
                return Err(self.error("expected ':'"));
            }
            self.pos += 1;
            self.skip_space();
            members.push((key, self.value(depth + 1)?));
            self.skip_space();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(members));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_space();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_space();
            items.push(self.value(depth + 1)?);
            self.skip_space();
            match self.bytes.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    /// Reads the strict JSON number grammar, then lets Rust convert it.
    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        let digits = |p: &mut Self| {
            let from = p.pos;
            while let Some(b'0'..=b'9') = p.bytes.get(p.pos) {
                p.pos += 1;
            }
            p.pos - from
        };
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        match self.bytes.get(self.pos) {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(self.error("expected a digit")),
        }
        if self.bytes.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if digits(self) == 0 {
                return Err(self.error("expected a digit after '.'"));
            }
        }
        if let Some(b'e' | b'E') = self.bytes.get(self.pos) {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.bytes.get(self.pos) {
                self.pos += 1;
            }
            if digits(self) == 0 {
                return Err(self.error("expected a digit in the exponent"));
            }
        }
        self.text[start..self.pos].parse::<f64>().map(Json::Number).map_err(|_| self.error("bad number"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let hex = self.text.get(self.pos..self.pos + 4).ok_or_else(|| self.error("short \\u escape"))?;
        let value = u32::from_str_radix(hex, 16).map_err(|_| self.error("bad \\u escape"))?;
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(self.error("bad \\u escape"));
        }
        self.pos += 4;
        Ok(value)
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            // Copy the run of plain characters at once; the input is valid UTF-8 already.
            let run = self.pos;
            while let Some(&b) = self.bytes.get(self.pos) {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(&self.text[run..self.pos]);
            match self.bytes.get(self.pos) {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escape = *self.bytes.get(self.pos).ok_or_else(|| self.error("unterminated escape"))?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(self.error("unknown escape")),
                    }
                }
                Some(_) => return Err(self.error("control character in a string")),
            }
        }
    }

    /// A `\uXXXX` escape, joined with the next one when the two form a surrogate pair.
    fn unicode_escape(&mut self) -> Result<char, String> {
        let first = self.hex4()?;
        let code = match first {
            0xD800..=0xDBFF => {
                if !self.text[self.pos..].starts_with("\\u") {
                    return Err(self.error("lone surrogate in \\u escape"));
                }
                self.pos += 2;
                let second = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&second) {
                    return Err(self.error("bad surrogate pair"));
                }
                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(self.error("lone surrogate in \\u escape")),
            other => other,
        };
        char::from_u32(code).ok_or_else(|| self.error("bad \\u escape"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_keep_their_key_order() {
        let value = Json::parse(r#"{"zeta": 1, "alpha": [true, false, null], "mid": {"b": 2, "a": 3}}"#).unwrap();
        let keys: Vec<&str> = value.as_object().unwrap().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["zeta", "alpha", "mid"]);
        assert_eq!(value.to_compact(), r#"{"zeta":1,"alpha":[true,false,null],"mid":{"b":2,"a":3}}"#);
        assert_eq!(value.get("mid").and_then(|m| m.get("a")).and_then(Json::as_f64), Some(3.0));
        assert_eq!(value.get("missing"), None);
    }

    #[test]
    fn numbers_are_doubles() {
        let value = Json::parse("[0, 0.0, -60.0, 0.7071, 1e3, -2.5E-2, 16000]").unwrap();
        let numbers: Vec<f64> = value.as_array().unwrap().iter().map(|n| n.as_f64().unwrap()).collect();
        assert_eq!(numbers, [0.0, 0.0, -60.0, 0.7071, 1000.0, -0.025, 16000.0]);
        assert_eq!(value.to_compact(), "[0,0,-60,0.7071,1000,-0.025,16000]");
        assert_eq!(Json::Number(f64::NAN).to_compact(), "null");
        for bad in ["01", "1.", ".5", "-", "1e", "+1", "0x10"] {
            assert!(Json::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn strings_unescape_and_escape() {
        let value = Json::parse(r#""a\"b\\c\/d\n\t\u00e9\u2122\ud83c\udfb5 ō""#).unwrap();
        assert_eq!(value.as_str(), Some("a\"b\\c/d\n\té™🎵 ō"));
        assert_eq!(value.to_compact(), "\"a\\\"b\\\\c/d\\n\\té™🎵 ō\"");
        assert_eq!(Json::String("\u{01}".into()).to_compact(), "\"\\u0001\"");
        for bad in [r#""\ud83c""#, r#""\udfb5""#, r#""\x""#, "\"a\nb\"", r#""open"#, r#""\u12""#] {
            assert!(Json::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn round_trips_through_both_layouts() {
        let text = r#"{"enabled":true,"filter1":{"enabled":false,"qFactor":0.7071,"frequency":31,"gain":-3.5,"type":"lowShelving"},"list":[],"empty":{},"name":"Ça marche ™"}"#;
        let value = Json::parse(text).unwrap();
        assert_eq!(value.to_compact(), text);
        let pretty = value.to_pretty();
        assert!(pretty.starts_with("{\n  \"enabled\": true,\n  \"filter1\": {\n    \"enabled\": false,"), "{pretty}");
        assert!(pretty.contains("\"list\": [],\n  \"empty\": {},"), "{pretty}");
        assert_eq!(Json::parse(&pretty).unwrap(), value);
    }

    #[test]
    fn refuses_broken_documents() {
        for bad in ["", "{", "[1,]", "{\"a\" 1}", "{\"a\":1,}", "tru", "nul", "[1] 2", "{'a':1}"] {
            assert!(Json::parse(bad).is_err(), "{bad}");
        }
        let deep = "[".repeat(500) + &"]".repeat(500);
        assert!(Json::parse(&deep).is_err());
        assert_eq!(Json::parse("\u{FEFF} {\"a\": null} ").unwrap(), Json::Object(vec![("a".into(), Json::Null)]));
    }
}
