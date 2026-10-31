//! A small, strict JSON reader and writer (RFC 8259 subset) for the TCP protocol.
//!
//! What it accepts: objects, arrays, strings (all RFC escapes, surrogate pairs
//! validated), `true`, `false`, `null`, and **integers only** (decimal, optional
//! leading `-`, no leading zeros, no fraction, no exponent, range `i128`).
//!
//! What it rejects: floats, `-0`, duplicate object keys, trailing garbage,
//! unescaped control characters, lone surrogates, comments, trailing commas.
//!
//! Hard limits (see [`Limits`]) bound input size, nesting depth, string length,
//! object members and array length, so memory use is bounded by the caller's
//! `max_bytes`. The parser is depth-bounded recursive descent; the default
//! depth limit is 8, so hostile nesting cannot overflow the stack. Input is a
//! `&str`, so invalid UTF-8 never reaches the parser.
//!
//! Parse errors carry a static kind and a byte offset and never echo the
//! input, so a malformed line containing a secret cannot leak through an error
//! message.

use std::fmt;

/// Default limit on input size in bytes.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;
/// Default limit on nesting depth (top-level container = depth 1).
pub const DEFAULT_MAX_DEPTH: usize = 8;
/// Default limit on the decoded length of one string, in bytes.
pub const DEFAULT_MAX_STRING: usize = 1024;
/// Default limit on object members.
pub const DEFAULT_MAX_MEMBERS: usize = 64;
/// Default limit on array length.
pub const DEFAULT_MAX_ARRAY: usize = 64;

/// A parsed JSON value. Objects keep member order and have unique keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer.
    Int(i128),
    /// A string.
    Str(String),
    /// An array.
    Arr(Vec<Json>),
    /// An object, members in source order.
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// Member of an object by key; `None` for other kinds or missing keys.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The string, if this is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The integer, if this is one.
    pub fn as_int(&self) -> Option<i128> {
        match self {
            Json::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// The boolean, if this is one.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The array elements, if this is an array.
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    /// The object members, if this is an object.
    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(members) => Some(members),
            _ => None,
        }
    }

    /// Append the compact serialization of this value to `out`.
    ///
    /// Recursion depth equals the value's nesting depth; values produced by
    /// [`parse`] are bounded by [`Limits::max_depth`].
    pub fn write_to(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(v) => write_int(out, *v),
            Json::Str(s) => write_string(out, s),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_to(out);
                }
                out.push(']');
            }
            Json::Obj(members) => {
                out.push('{');
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(out, k);
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.write_to(&mut s);
        f.write_str(&s)
    }
}

/// Hard limits applied by [`parse_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum input length in bytes.
    pub max_bytes: usize,
    /// Maximum nesting depth; a top-level container has depth 1.
    pub max_depth: usize,
    /// Maximum decoded string length in bytes (keys and values).
    pub max_string: usize,
    /// Maximum members of one object.
    pub max_members: usize,
    /// Maximum elements of one array.
    pub max_array: usize,
}

impl Limits {
    /// Default limits with a caller-chosen input size.
    pub fn with_max_bytes(max_bytes: usize) -> Limits {
        Limits {
            max_bytes,
            ..Limits::default()
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bytes: DEFAULT_MAX_BYTES,
            max_depth: DEFAULT_MAX_DEPTH,
            max_string: DEFAULT_MAX_STRING,
            max_members: DEFAULT_MAX_MEMBERS,
            max_array: DEFAULT_MAX_ARRAY,
        }
    }
}

/// What went wrong while parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonErrorKind {
    /// Input longer than `max_bytes`.
    TooLarge,
    /// Nesting deeper than `max_depth`.
    TooDeep,
    /// A string longer than `max_string`.
    StringTooLong,
    /// An object with more than `max_members` members.
    TooManyMembers,
    /// An array with more than `max_array` elements.
    ArrayTooLong,
    /// The same key twice in one object.
    DuplicateKey,
    /// Non-whitespace after the top-level value.
    TrailingGarbage,
    /// Input ended inside a value.
    UnexpectedEnd,
    /// A byte that cannot start or continue the expected token.
    UnexpectedChar,
    /// Unknown or malformed escape sequence.
    BadEscape,
    /// Lone or mismatched UTF-16 surrogate in a `\u` escape.
    BadSurrogate,
    /// Unescaped control character (below U+0020) inside a string.
    ControlChar,
    /// Malformed number.
    BadNumber,
    /// Number with leading zeros such as `01`.
    LeadingZero,
    /// `-0` is rejected as ambiguous.
    NegativeZero,
    /// Number with a fraction or exponent; only integers are accepted.
    FloatNotAllowed,
    /// Integer outside the `i128` range.
    NumberOutOfRange,
}

impl JsonErrorKind {
    fn text(self) -> &'static str {
        match self {
            JsonErrorKind::TooLarge => "input too large",
            JsonErrorKind::TooDeep => "nesting too deep",
            JsonErrorKind::StringTooLong => "string too long",
            JsonErrorKind::TooManyMembers => "too many object members",
            JsonErrorKind::ArrayTooLong => "array too long",
            JsonErrorKind::DuplicateKey => "duplicate object key",
            JsonErrorKind::TrailingGarbage => "trailing characters after value",
            JsonErrorKind::UnexpectedEnd => "unexpected end of input",
            JsonErrorKind::UnexpectedChar => "unexpected character",
            JsonErrorKind::BadEscape => "invalid escape sequence",
            JsonErrorKind::BadSurrogate => "invalid surrogate escape",
            JsonErrorKind::ControlChar => "control character in string",
            JsonErrorKind::BadNumber => "malformed number",
            JsonErrorKind::LeadingZero => "leading zero in number",
            JsonErrorKind::NegativeZero => "negative zero is not accepted",
            JsonErrorKind::FloatNotAllowed => "only integers are accepted",
            JsonErrorKind::NumberOutOfRange => "integer out of range",
        }
    }
}

/// A parse failure: a static kind and the byte offset where it was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonError {
    /// What went wrong.
    pub kind: JsonErrorKind,
    /// Byte offset into the input.
    pub pos: usize,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.kind.text(), self.pos)
    }
}

impl std::error::Error for JsonError {}

/// Parse `input` with default limits except `max_bytes`.
pub fn parse(input: &str, max_bytes: usize) -> Result<Json, JsonError> {
    parse_with(input, &Limits::with_max_bytes(max_bytes))
}

/// Parse `input` with explicit limits.
pub fn parse_with(input: &str, limits: &Limits) -> Result<Json, JsonError> {
    if input.len() > limits.max_bytes {
        return Err(JsonError {
            kind: JsonErrorKind::TooLarge,
            pos: 0,
        });
    }
    let mut p = Parser {
        src: input,
        bytes: input.as_bytes(),
        pos: 0,
        lim: limits,
    };
    let v = p.value(0)?;
    p.skip_ws();
    if p.pos < p.bytes.len() {
        return Err(p.err(JsonErrorKind::TrailingGarbage));
    }
    Ok(v)
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    lim: &'a Limits,
}

type PResult<T> = Result<T, JsonError>;

impl Parser<'_> {
    fn err(&self, kind: JsonErrorKind) -> JsonError {
        JsonError {
            kind,
            pos: self.pos,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, want: u8) -> PResult<()> {
        match self.peek() {
            Some(b) if b == want => {
                self.pos += 1;
                Ok(())
            }
            Some(_) => Err(self.err(JsonErrorKind::UnexpectedChar)),
            None => Err(self.err(JsonErrorKind::UnexpectedEnd)),
        }
    }

    fn value(&mut self, depth: usize) -> PResult<Json> {
        self.skip_ws();
        match self.peek() {
            None => Err(self.err(JsonErrorKind::UnexpectedEnd)),
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal(b"true", Json::Bool(true)),
            Some(b'f') => self.literal(b"false", Json::Bool(false)),
            Some(b'n') => self.literal(b"null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err(JsonErrorKind::UnexpectedChar)),
        }
    }

    fn literal(&mut self, lit: &[u8], v: Json) -> PResult<Json> {
        let matches = self
            .bytes
            .get(self.pos..)
            .is_some_and(|rest| rest.starts_with(lit));
        if matches {
            self.pos += lit.len();
            Ok(v)
        } else {
            Err(self.err(JsonErrorKind::UnexpectedChar))
        }
    }

    fn object(&mut self, depth: usize) -> PResult<Json> {
        if depth > self.lim.max_depth {
            return Err(self.err(JsonErrorKind::TooDeep));
        }
        self.pos += 1; // '{'
        let mut members: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err(if self.peek().is_none() {
                    JsonErrorKind::UnexpectedEnd
                } else {
                    JsonErrorKind::UnexpectedChar
                }));
            }
            let key_pos = self.pos;
            let key = self.string()?;
            if members.iter().any(|(k, _)| *k == key) {
                return Err(JsonError {
                    kind: JsonErrorKind::DuplicateKey,
                    pos: key_pos,
                });
            }
            if members.len() >= self.lim.max_members {
                return Err(JsonError {
                    kind: JsonErrorKind::TooManyMembers,
                    pos: key_pos,
                });
            }
            self.skip_ws();
            self.expect(b':')?;
            let v = self.value(depth)?;
            members.push((key, v));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(members));
                }
                Some(_) => return Err(self.err(JsonErrorKind::UnexpectedChar)),
                None => return Err(self.err(JsonErrorKind::UnexpectedEnd)),
            }
        }
    }

    fn array(&mut self, depth: usize) -> PResult<Json> {
        if depth > self.lim.max_depth {
            return Err(self.err(JsonErrorKind::TooDeep));
        }
        self.pos += 1; // '['
        let mut items: Vec<Json> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            if items.len() >= self.lim.max_array {
                return Err(self.err(JsonErrorKind::ArrayTooLong));
            }
            let v = self.value(depth)?;
            items.push(v);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                Some(_) => return Err(self.err(JsonErrorKind::UnexpectedChar)),
                None => return Err(self.err(JsonErrorKind::UnexpectedEnd)),
            }
        }
    }

    fn string(&mut self) -> PResult<String> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // Runs are split only at ASCII bytes, so they are valid UTF-8.
            let run = self
                .src
                .get(start..self.pos)
                .ok_or_else(|| self.err(JsonErrorKind::UnexpectedChar))?;
            self.push_checked(&mut out, run)?;
            match self.peek() {
                None => return Err(self.err(JsonErrorKind::UnexpectedEnd)),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let c = self.escape()?;
                    let mut tmp = [0u8; 4];
                    self.push_checked(&mut out, c.encode_utf8(&mut tmp))?;
                }
                Some(_) => return Err(self.err(JsonErrorKind::ControlChar)),
            }
        }
    }

    fn push_checked(&self, out: &mut String, s: &str) -> PResult<()> {
        let new_len = out
            .len()
            .checked_add(s.len())
            .ok_or_else(|| self.err(JsonErrorKind::StringTooLong))?;
        if new_len > self.lim.max_string {
            return Err(self.err(JsonErrorKind::StringTooLong));
        }
        out.push_str(s);
        Ok(())
    }

    fn escape(&mut self) -> PResult<char> {
        let b = self
            .peek()
            .ok_or_else(|| self.err(JsonErrorKind::UnexpectedEnd))?;
        self.pos += 1;
        let c = match b {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => return self.unicode_escape(),
            _ => {
                self.pos -= 1;
                return Err(self.err(JsonErrorKind::BadEscape));
            }
        };
        Ok(c)
    }

    fn hex4(&mut self) -> PResult<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            let b = self
                .peek()
                .ok_or_else(|| self.err(JsonErrorKind::UnexpectedEnd))?;
            let d = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(self.err(JsonErrorKind::BadEscape)),
            };
            v = (v << 4) | u32::from(d);
            self.pos += 1;
        }
        Ok(v)
    }

    fn unicode_escape(&mut self) -> PResult<char> {
        let first = self.hex4()?;
        let code = match first {
            0xD800..=0xDBFF => {
                // High surrogate: must be followed by `\u` and a low surrogate.
                if self.peek() != Some(b'\\') {
                    return Err(self.err(JsonErrorKind::BadSurrogate));
                }
                self.pos += 1;
                if self.peek() != Some(b'u') {
                    return Err(self.err(JsonErrorKind::BadSurrogate));
                }
                self.pos += 1;
                let second = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&second) {
                    return Err(self.err(JsonErrorKind::BadSurrogate));
                }
                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(self.err(JsonErrorKind::BadSurrogate)),
            other => other,
        };
        char::from_u32(code).ok_or_else(|| self.err(JsonErrorKind::BadSurrogate))
    }

    fn number(&mut self) -> PResult<Json> {
        let start = self.pos;
        let neg = self.peek() == Some(b'-');
        if neg {
            self.pos += 1;
        }
        let mut mag: u128 = 0;
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err(JsonErrorKind::LeadingZero));
                }
            }
            Some(b'1'..=b'9') => {
                while let Some(b @ b'0'..=b'9') = self.peek() {
                    mag = mag
                        .checked_mul(10)
                        .and_then(|m| m.checked_add(u128::from(b - b'0')))
                        .ok_or(JsonError {
                            kind: JsonErrorKind::NumberOutOfRange,
                            pos: start,
                        })?;
                    self.pos += 1;
                }
            }
            Some(_) => return Err(self.err(JsonErrorKind::BadNumber)),
            None => return Err(self.err(JsonErrorKind::UnexpectedEnd)),
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(self.err(JsonErrorKind::FloatNotAllowed));
        }
        let out_of_range = JsonError {
            kind: JsonErrorKind::NumberOutOfRange,
            pos: start,
        };
        if neg && mag == 0 {
            return Err(JsonError {
                kind: JsonErrorKind::NegativeZero,
                pos: start,
            });
        }
        let value = if neg {
            if mag == i128::MIN.unsigned_abs() {
                i128::MIN
            } else {
                i128::try_from(mag)
                    .ok()
                    .and_then(i128::checked_neg)
                    .ok_or(out_of_range)?
            }
        } else {
            i128::try_from(mag).map_err(|_| out_of_range)?
        };
        Ok(Json::Int(value))
    }
}

/// Append `s` as a JSON string literal (with quotes) to `out`.
///
/// Escapes `"`, `\`, and all control characters below U+0020; everything else
/// is written as UTF-8.
pub fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u00");
                let n = c as u32;
                out.push(char::from_digit((n >> 4) & 15, 16).unwrap_or('0'));
                out.push(char::from_digit(n & 15, 16).unwrap_or('0'));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Append a decimal integer to `out`.
pub fn write_int(out: &mut String, v: i128) {
    out.push_str(&v.to_string());
}

/// Incremental writer for one JSON object, appending to a borrowed `String`.
///
/// Keys are escaped like any other string. Call [`ObjWriter::finish`] to close
/// the object.
pub struct ObjWriter<'a> {
    out: &'a mut String,
    first: bool,
}

impl<'a> ObjWriter<'a> {
    /// Start an object (writes `{`).
    pub fn new(out: &'a mut String) -> ObjWriter<'a> {
        out.push('{');
        ObjWriter { out, first: true }
    }

    fn key(&mut self, key: &str) {
        if !self.first {
            self.out.push(',');
        }
        self.first = false;
        write_string(self.out, key);
        self.out.push(':');
    }

    /// Member with an integer value.
    pub fn int<T: Into<i128>>(&mut self, key: &str, v: T) -> &mut Self {
        self.key(key);
        write_int(self.out, v.into());
        self
    }

    /// Member with an unsigned 128-bit integer value.
    pub fn uint128(&mut self, key: &str, v: u128) -> &mut Self {
        self.key(key);
        self.out.push_str(&v.to_string());
        self
    }

    /// Member with a string value.
    pub fn str(&mut self, key: &str, v: &str) -> &mut Self {
        self.key(key);
        write_string(self.out, v);
        self
    }

    /// Member with a boolean value.
    pub fn bool(&mut self, key: &str, v: bool) -> &mut Self {
        self.key(key);
        self.out.push_str(if v { "true" } else { "false" });
        self
    }

    /// Member with an optional integer: `null` when absent.
    pub fn opt_int<T: Into<i128>>(&mut self, key: &str, v: Option<T>) -> &mut Self {
        self.key(key);
        match v {
            Some(v) => write_int(self.out, v.into()),
            None => self.out.push_str("null"),
        }
        self
    }

    /// Member whose value is already valid JSON text. The caller guarantees
    /// validity; nothing is escaped.
    pub fn raw(&mut self, key: &str, json: &str) -> &mut Self {
        self.key(key);
        self.out.push_str(json);
        self
    }

    /// Close the object (writes `}`).
    pub fn finish(self) {
        self.out.push('}');
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Json, JsonError> {
        parse(s, 1 << 20)
    }

    fn kind(s: &str) -> JsonErrorKind {
        p(s).unwrap_err().kind
    }

    #[test]
    fn scalars() {
        assert_eq!(p("null").unwrap(), Json::Null);
        assert_eq!(p("true").unwrap(), Json::Bool(true));
        assert_eq!(p(" false ").unwrap(), Json::Bool(false));
        assert_eq!(p("0").unwrap(), Json::Int(0));
        assert_eq!(p("-17").unwrap(), Json::Int(-17));
        assert_eq!(p("\"hi\"").unwrap(), Json::Str("hi".into()));
    }

    #[test]
    fn integer_range_i128() {
        let max = i128::MAX.to_string();
        let min = i128::MIN.to_string();
        assert_eq!(p(&max).unwrap(), Json::Int(i128::MAX));
        assert_eq!(p(&min).unwrap(), Json::Int(i128::MIN));
        // One past each end.
        assert_eq!(
            kind("170141183460469231731687303715884105728"),
            JsonErrorKind::NumberOutOfRange
        );
        assert_eq!(
            kind("-170141183460469231731687303715884105729"),
            JsonErrorKind::NumberOutOfRange
        );
    }

    #[test]
    fn huge_numbers_rejected() {
        let big = "9".repeat(5000);
        assert_eq!(kind(&big), JsonErrorKind::NumberOutOfRange);
        let neg = format!("-{big}");
        assert_eq!(kind(&neg), JsonErrorKind::NumberOutOfRange);
    }

    #[test]
    fn floats_rejected() {
        assert_eq!(kind("1.5"), JsonErrorKind::FloatNotAllowed);
        assert_eq!(kind("1e3"), JsonErrorKind::FloatNotAllowed);
        assert_eq!(kind("1E3"), JsonErrorKind::FloatNotAllowed);
        assert_eq!(kind("0.0"), JsonErrorKind::FloatNotAllowed);
        assert_eq!(kind("-2.5e-3"), JsonErrorKind::FloatNotAllowed);
    }

    #[test]
    fn malformed_numbers_rejected() {
        assert_eq!(kind("01"), JsonErrorKind::LeadingZero);
        assert_eq!(kind("-01"), JsonErrorKind::LeadingZero);
        assert_eq!(kind("00"), JsonErrorKind::LeadingZero);
        assert_eq!(kind("-"), JsonErrorKind::UnexpectedEnd);
        assert_eq!(kind("-x"), JsonErrorKind::BadNumber);
        assert_eq!(kind("-0"), JsonErrorKind::NegativeZero);
        assert_eq!(kind("+1"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind(".5"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("1x"), JsonErrorKind::TrailingGarbage);
        assert_eq!(kind("0x10"), JsonErrorKind::TrailingGarbage);
        assert_eq!(kind("NaN"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("Infinity"), JsonErrorKind::UnexpectedChar);
    }

    #[test]
    fn objects_and_arrays() {
        let v = p(r#" {"a": [1, 2, {"b": null}], "c": "x", "d": true} "#).unwrap();
        assert_eq!(v.get("c").and_then(Json::as_str), Some("x"));
        assert_eq!(v.get("d").and_then(Json::as_bool), Some(true));
        let a = v.get("a").and_then(Json::as_array).unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].as_int(), Some(1));
        assert_eq!(a[2].get("b"), Some(&Json::Null));
        assert_eq!(v.get("missing"), None);
        assert_eq!(Json::Int(1).get("a"), None);
        assert_eq!(p("{}").unwrap(), Json::Obj(vec![]));
        assert_eq!(p("[]").unwrap(), Json::Arr(vec![]));
    }

    #[test]
    fn structure_errors() {
        assert_eq!(kind(""), JsonErrorKind::UnexpectedEnd);
        assert_eq!(kind("   "), JsonErrorKind::UnexpectedEnd);
        assert_eq!(kind("{"), JsonErrorKind::UnexpectedEnd);
        assert_eq!(kind("[1,"), JsonErrorKind::UnexpectedEnd);
        assert_eq!(kind("[1,]"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("{\"a\":1,}"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("{\"a\" 1}"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("{a:1}"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("{\"a\":1 \"b\":2}"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("[1 2]"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("tru"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("nul"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("'a'"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("// c\n1"), JsonErrorKind::UnexpectedChar);
    }

    #[test]
    fn trailing_garbage_rejected() {
        assert_eq!(kind("{} x"), JsonErrorKind::TrailingGarbage);
        assert_eq!(kind("1 2"), JsonErrorKind::TrailingGarbage);
        assert_eq!(kind("{}{}"), JsonErrorKind::TrailingGarbage);
        assert_eq!(kind("null,"), JsonErrorKind::TrailingGarbage);
        assert!(p("{}  \n\t\r ").is_ok());
    }

    #[test]
    fn duplicate_keys_rejected() {
        assert_eq!(kind(r#"{"a":1,"a":2}"#), JsonErrorKind::DuplicateKey);
        assert_eq!(
            kind(r#"{"a":1,"b":{"x":1,"x":2}}"#),
            JsonErrorKind::DuplicateKey
        );
        // Same key at different levels is fine.
        assert!(p(r#"{"a":{"a":1}}"#).is_ok());
        // Keys that differ only by escape form are still equal after decoding.
        assert_eq!(kind(r#"{"a":1,"\u0061":2}"#), JsonErrorKind::DuplicateKey);
    }

    #[test]
    fn string_escapes() {
        let v = p(r#""\" \\ \/ \b \f \n \r \t \u0041 \u00e9 \u20AC""#).unwrap();
        assert_eq!(
            v.as_str().unwrap(),
            "\" \\ / \u{8} \u{c} \n \r \t A \u{e9} \u{20ac}"
        );
    }

    #[test]
    fn string_non_ascii_passthrough() {
        let v = p("\"h\u{e9}llo \u{4e16}\u{754c} \u{1f600}\"").unwrap();
        assert_eq!(v.as_str().unwrap(), "h\u{e9}llo \u{4e16}\u{754c} \u{1f600}");
    }

    #[test]
    fn surrogate_pairs() {
        let v = p(r#""\ud83d\ude00""#).unwrap();
        assert_eq!(v.as_str().unwrap(), "\u{1f600}");
        // Lone high, lone low, high followed by non-low, reversed order.
        assert_eq!(kind(r#""\ud83d""#), JsonErrorKind::BadSurrogate);
        assert_eq!(kind(r#""\ude00""#), JsonErrorKind::BadSurrogate);
        assert_eq!(kind(r#""\ud83dx""#), JsonErrorKind::BadSurrogate);
        assert_eq!(kind(r#""\ud83d\u0041""#), JsonErrorKind::BadSurrogate);
        assert_eq!(kind(r#""\ude00\ud83d""#), JsonErrorKind::BadSurrogate);
        assert_eq!(kind(r#""\ud83d\n""#), JsonErrorKind::BadSurrogate);
    }

    #[test]
    fn bad_escapes() {
        assert_eq!(kind(r#""\x""#), JsonErrorKind::BadEscape);
        assert_eq!(kind(r#""\u12""#), JsonErrorKind::BadEscape);
        assert_eq!(kind(r#""\u12G4""#), JsonErrorKind::BadEscape);
        assert_eq!(kind(r#""\"#), JsonErrorKind::UnexpectedEnd);
        assert_eq!(kind(r#""\u00"#), JsonErrorKind::UnexpectedEnd);
        assert_eq!(kind(r#""abc"#), JsonErrorKind::UnexpectedEnd);
        assert_eq!(kind(r#""\'""#), JsonErrorKind::BadEscape);
    }

    #[test]
    fn control_characters_rejected_in_strings() {
        for b in 0u8..0x20 {
            let s = format!("\"a{}b\"", char::from(b));
            assert_eq!(kind(&s), JsonErrorKind::ControlChar, "byte {b}");
        }
        // DEL is allowed by RFC 8259.
        assert!(p("\"a\u{7f}b\"").is_ok());
        // Escaped control characters are fine.
        assert_eq!(p(r#""\u0000""#).unwrap().as_str().unwrap(), "\u{0}");
    }

    #[test]
    fn invalid_whitespace_rejected() {
        // Vertical tab, form feed and NBSP are not JSON whitespace.
        assert_eq!(kind("\u{b}1"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("\u{c}1"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("\u{a0}1"), JsonErrorKind::UnexpectedChar);
        assert_eq!(kind("\u{feff}1"), JsonErrorKind::UnexpectedChar);
    }

    #[test]
    fn limit_input_bytes() {
        assert!(parse("[1,2]", 5).is_ok());
        let e = parse("[1,2]", 4).unwrap_err();
        assert_eq!(e.kind, JsonErrorKind::TooLarge);
        assert_eq!(parse("", 0).unwrap_err().kind, JsonErrorKind::UnexpectedEnd);
    }

    #[test]
    fn limit_depth() {
        let ok = format!("{}1{}", "[".repeat(8), "]".repeat(8));
        assert!(p(&ok).is_ok());
        let bad = format!("{}1{}", "[".repeat(9), "]".repeat(9));
        assert_eq!(kind(&bad), JsonErrorKind::TooDeep);
        let objs = format!("{}1{}", "{\"a\":".repeat(9), "}".repeat(9));
        assert_eq!(kind(&objs), JsonErrorKind::TooDeep);
        let objs_ok = format!("{}1{}", "{\"a\":".repeat(8), "}".repeat(8));
        assert!(p(&objs_ok).is_ok());
    }

    #[test]
    fn deeply_nested_hostile_input_does_not_overflow_stack() {
        let n = 200_000;
        let bad = "[".repeat(n);
        assert_eq!(kind(&bad), JsonErrorKind::TooDeep);
        let bad = "{\"a\":".repeat(n);
        assert_eq!(kind(&bad), JsonErrorKind::TooDeep);
        // Even with a generous (but finite) depth limit it is bounded by the limit.
        let lim = Limits {
            max_depth: 64,
            max_bytes: 1 << 20,
            ..Limits::default()
        };
        assert_eq!(
            parse_with(&"[".repeat(n), &lim).unwrap_err().kind,
            JsonErrorKind::TooDeep
        );
    }

    #[test]
    fn limit_string_length() {
        let lim = Limits {
            max_string: 4,
            ..Limits::default()
        };
        assert!(parse_with("\"abcd\"", &lim).is_ok());
        assert_eq!(
            parse_with("\"abcde\"", &lim).unwrap_err().kind,
            JsonErrorKind::StringTooLong
        );
        // Escapes count by decoded length: 2 x 2-byte chars = 4 bytes.
        assert!(parse_with("\"\\u00e9\\u00e9\"", &lim).is_ok());
        assert_eq!(
            parse_with("\"\\u00e9\\u00e9a\"", &lim).unwrap_err().kind,
            JsonErrorKind::StringTooLong
        );
        // Keys are limited too.
        assert_eq!(
            parse_with("{\"abcde\":1}", &lim).unwrap_err().kind,
            JsonErrorKind::StringTooLong
        );
        // Default limit.
        let long = format!("\"{}\"", "a".repeat(DEFAULT_MAX_STRING + 1));
        assert_eq!(kind(&long), JsonErrorKind::StringTooLong);
        let fits = format!("\"{}\"", "a".repeat(DEFAULT_MAX_STRING));
        assert!(p(&fits).is_ok());
    }

    #[test]
    fn limit_object_members() {
        let mk = |n: usize| {
            let body: Vec<String> = (0..n).map(|i| format!("\"k{i}\":{i}")).collect();
            format!("{{{}}}", body.join(","))
        };
        assert!(p(&mk(64)).is_ok());
        assert_eq!(kind(&mk(65)), JsonErrorKind::TooManyMembers);
    }

    #[test]
    fn limit_array_length() {
        let mk = |n: usize| {
            let body: Vec<String> = (0..n).map(|i| i.to_string()).collect();
            format!("[{}]", body.join(","))
        };
        assert!(p(&mk(64)).is_ok());
        assert_eq!(kind(&mk(65)), JsonErrorKind::ArrayTooLong);
        assert_eq!(kind(&mk(10_000)), JsonErrorKind::ArrayTooLong);
    }

    #[test]
    fn error_positions_point_into_input() {
        let e = p("[1, x]").unwrap_err();
        assert_eq!(e.kind, JsonErrorKind::UnexpectedChar);
        assert_eq!(e.pos, 4);
        let e = p("{\"a\":1,\"a\":2}").unwrap_err();
        assert_eq!(e.pos, 7);
    }

    #[test]
    fn error_text_never_echoes_input() {
        let secret = "s3cr3t-value";
        let e = p(&format!("{{\"token\":\"{secret}\" x}}")).unwrap_err();
        assert!(!e.to_string().contains(secret));
        assert!(e.to_string().contains("unexpected character"));
    }

    #[test]
    fn write_string_escapes() {
        let mut s = String::new();
        write_string(&mut s, "a\"b\\c\n\r\t\u{8}\u{c}\u{1}\u{1f}\u{e9}\u{1f600}/");
        assert_eq!(
            s,
            "\"a\\\"b\\\\c\\n\\r\\t\\b\\f\\u0001\\u001f\u{e9}\u{1f600}/\""
        );
    }

    #[test]
    fn write_string_round_trips_every_control_char() {
        let all: String = (0u8..=0x7f).map(char::from).collect();
        let mut s = String::new();
        write_string(&mut s, &all);
        assert_eq!(p(&s).unwrap().as_str().unwrap(), all);
    }

    #[test]
    fn write_int_extremes() {
        let mut s = String::new();
        write_int(&mut s, i128::MIN);
        s.push(' ');
        write_int(&mut s, i128::MAX);
        s.push(' ');
        write_int(&mut s, 0);
        assert_eq!(
            s,
            "-170141183460469231731687303715884105728 170141183460469231731687303715884105727 0"
        );
    }

    #[test]
    fn round_trip_values() {
        let cases = [
            r#"null"#,
            r#"[true,false,null,-5,"x"]"#,
            r#"{"a":{"b":[1,2,3],"c":"d\ne"},"e":170141183460469231731687303715884105727}"#,
            r#"{}"#,
            r#"[[],{}]"#,
        ];
        for c in cases {
            let v = p(c).unwrap();
            assert_eq!(v.to_string(), c);
            assert_eq!(p(&v.to_string()).unwrap(), v);
        }
    }

    #[test]
    fn obj_writer_builds_valid_json() {
        let mut s = String::new();
        let mut o = ObjWriter::new(&mut s);
        o.int("a", 1u64)
            .int("b", -2i128)
            .str("c", "x\"y")
            .bool("d", true)
            .opt_int("e", None::<u64>)
            .opt_int("f", Some(7u32))
            .uint128("g", u128::MAX)
            .raw("h", "[1,2]");
        o.finish();
        assert_eq!(
            s,
            "{\"a\":1,\"b\":-2,\"c\":\"x\\\"y\",\"d\":true,\"e\":null,\"f\":7,\"g\":340282366920938463463374607431768211455,\"h\":[1,2]}"
        );
        let mut empty = String::new();
        ObjWriter::new(&mut empty).finish();
        assert_eq!(empty, "{}");
    }

    #[test]
    fn accessors_return_none_on_wrong_kind() {
        let v = Json::Int(1);
        assert_eq!(v.as_str(), None);
        assert_eq!(v.as_bool(), None);
        assert_eq!(v.as_array(), None);
        assert_eq!(v.as_object(), None);
        assert_eq!(Json::Null.as_int(), None);
    }

    #[test]
    fn hostile_table_never_panics() {
        let table: &[&str] = &[
            "\u{0}",
            "{\"",
            "{\"a",
            "{\"a\"",
            "{\"a\":",
            "[\"\\ud800",
            "[\"\\udc00\\ud800\"]",
            "-",
            "--1",
            "1-",
            "[,]",
            "{,}",
            "{\"a\":}",
            "\"\\u",
            "\"\\u{41}\"",
            "}",
            "]",
            ":",
            ",",
            "\"\u{1f600}",
            "tr\u{1f600}",
            "[1e5]",
            "[-]",
            "{\"a\":{\"b\":{\"c\":{\"d\":{\"e\":{\"f\":{\"g\":{\"h\":{}}}}}}}}}",
        ];
        for t in table {
            let _ = p(t);
        }
    }

    #[test]
    fn prefix_truncations_of_valid_input_never_panic() {
        let full = r#"{"op":"place","qty":123,"s":"a\u00e9\ud83d\ude00","a":[1,-2,true,null]}"#;
        let mut count_err = 0;
        for (i, _) in full.char_indices() {
            if p(&full[..i]).is_err() {
                count_err += 1;
            }
        }
        assert!(count_err > 10);
        assert!(p(full).is_ok());
    }
}
