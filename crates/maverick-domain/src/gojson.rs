//! JSON with Go `encoding/json` semantics, where the engine's contract depends on them.
//!
//! - [`decode_one`] reads one value like `json.Decoder.Decode`. Object members stay in
//!   document order (duplicates included) and numbers stay as their literal text, so a
//!   caller can apply Go's rules: case-insensitive field names, last duplicate wins,
//!   `null` leaves a field unchanged, and `number 1.5` in type errors.
//! - [`marshal`] writes compact JSON like `json.Marshal`: `<`, `>`, `&`, U+2028 and
//!   U+2029 escaped, `\b` and `\f` short escapes (Go 1.22+), other control characters as
//!   `\u00XX`.
//! - [`from_yaml`] is the grader-spec re-encoding (`graders.JSONFromValue`): YAML maps
//!   become objects with sorted keys, integers stay integers, and floats use Go's
//!   `float64` JSON form (`1.0` becomes `1`).

use std::fmt::Write as _;

use crate::gofmt;

/// A JSON value as Go's decoder sees it before it is stored into a Go type.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    /// The number literal exactly as written (`1`, `1.0`, `1e309`).
    Number(String),
    String(String),
    Array(Vec<JsonValue>),
    /// Members in document order. Duplicate keys are kept; Go's last-wins rule is the
    /// caller's to apply.
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    /// The Go `UnmarshalTypeError` name of this value's JSON kind: `string`, `number`,
    /// `bool`, `array`, `object`, or `null`.
    pub fn kind_name(&self) -> &'static str {
        match self {
            JsonValue::Null => "null",
            JsonValue::Bool(_) => "bool",
            JsonValue::Number(_) => "number",
            JsonValue::String(_) => "string",
            JsonValue::Array(_) => "array",
            JsonValue::Object(_) => "object",
        }
    }

    /// Converts to a `serde_json::Value`, for code that decodes a grader spec into its
    /// own types. Duplicate keys resolve to the last occurrence, as in Go. Fails when a
    /// number literal does not fit an `f64` (Go fails the same decode).
    pub fn to_serde(&self) -> Result<serde_json::Value, String> {
        Ok(match self {
            JsonValue::Null => serde_json::Value::Null,
            JsonValue::Bool(b) => serde_json::Value::Bool(*b),
            JsonValue::Number(lit) => serde_json::from_str(lit)
                .map_err(|_| format!("json: cannot unmarshal number {lit} into a float64"))?,
            JsonValue::String(s) => serde_json::Value::String(s.clone()),
            JsonValue::Array(items) => serde_json::Value::Array(
                items
                    .iter()
                    .map(JsonValue::to_serde)
                    .collect::<Result<_, _>>()?,
            ),
            JsonValue::Object(members) => {
                let mut map = serde_json::Map::new();
                for (key, value) in members {
                    map.insert(key.clone(), value.to_serde()?);
                }
                serde_json::Value::Object(map)
            }
        })
    }
}

/// A decode failure. The text is Go's (`unexpected EOF`, `invalid character 'x'
/// looking for beginning of value`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError(pub String);

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The maximum nesting depth. Go allows 10,000; this recursive parser stops at 128,
/// the same limit serde_json applies everywhere else in the port, so a hostile
/// document cannot overflow the stack. Grader specs and judge replies nest a few
/// levels deep. The error text is Go's.
const MAX_DEPTH: usize = 128;

/// Reads one JSON value after optional leading whitespace, like one
/// `json.Decoder.Decode` call. Returns the value and the text after it.
///
/// Empty input fails with `EOF`, input that ends inside a value with
/// `unexpected EOF`.
pub fn decode_one(input: &str) -> Result<(JsonValue, &str), SyntaxError> {
    let mut parser = Parser {
        text: input,
        bytes: input.as_bytes(),
        pos: 0,
        depth: 0,
    };
    parser.skip_ws();
    if parser.pos == parser.bytes.len() {
        return Err(SyntaxError("EOF".to_owned()));
    }
    let value = parser.value()?;
    Ok((value, &input[parser.pos..]))
}

/// Parses a whole document like `json.Unmarshal`: one value and nothing but whitespace
/// after it. Truncated or empty input is `unexpected end of JSON input`.
pub fn unmarshal(input: &str) -> Result<JsonValue, SyntaxError> {
    let (value, rest) = decode_one(input).map_err(|e| match e.0.as_str() {
        "EOF" | "unexpected EOF" => SyntaxError("unexpected end of JSON input".to_owned()),
        _ => e,
    })?;
    match rest
        .trim_start_matches([' ', '\t', '\n', '\r'])
        .chars()
        .next()
    {
        None => Ok(value),
        Some(c) => Err(SyntaxError(format!(
            "invalid character {} after top-level value",
            quote_char(c)
        ))),
    }
}

/// Go's struct field matching for a JSON key: an exact field name first, then a
/// case-insensitive match that folds each rune to its simple case-fold class (so
/// `NAME`, `Name`, and `ſpec` for `spec` match).
pub fn match_field(key: &str, fields: &[&'static str]) -> Option<&'static str> {
    fields
        .iter()
        .find(|f| **f == key)
        .or_else(|| fields.iter().find(|f| fold_eq(f, key)))
        .copied()
}

fn fold_eq(a: &str, b: &str) -> bool {
    let fold = |c: char| -> String { c.to_uppercase().collect::<String>().to_lowercase() };
    let mut a = a.chars();
    let mut b = b.chars();
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if fold(x) == fold(y) => {}
            _ => return false,
        }
    }
}

/// True if `rest` holds only JSON whitespace.
pub fn is_blank(rest: &str) -> bool {
    rest.bytes()
        .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.pos) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eof() -> SyntaxError {
        SyntaxError("unexpected EOF".to_owned())
    }

    /// The character at `pos`, in constant time. `pos` only moves by whole characters,
    /// so it is always a char boundary of `text`.
    fn current_char(&self) -> Option<char> {
        self.text
            .get(self.pos..)
            .and_then(|rest| rest.chars().next())
    }

    fn invalid(&self, context: &str) -> SyntaxError {
        let c = self.current_char().unwrap_or('\u{fffd}');
        SyntaxError(format!("invalid character {} {context}", quote_char(c)))
    }

    fn value(&mut self) -> Result<JsonValue, SyntaxError> {
        match self.peek() {
            None => Err(Self::eof()),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(JsonValue::String(self.string()?)),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.literal("true", JsonValue::Bool(true)),
            Some(b'f') => self.literal("false", JsonValue::Bool(false)),
            Some(b'n') => self.literal("null", JsonValue::Null),
            Some(_) => Err(self.invalid("looking for beginning of value")),
        }
    }

    fn enter(&mut self) -> Result<(), SyntaxError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(SyntaxError("exceeded max depth".to_owned()));
        }
        Ok(())
    }

    fn object(&mut self) -> Result<JsonValue, SyntaxError> {
        self.enter()?;
        self.pos += 1;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonValue::Object(members));
        }
        loop {
            self.skip_ws();
            match self.peek() {
                None => return Err(Self::eof()),
                Some(b'"') => {}
                Some(_) => return Err(self.invalid("looking for beginning of object key string")),
            }
            let key = self.string()?;
            self.skip_ws();
            match self.peek() {
                None => return Err(Self::eof()),
                Some(b':') => self.pos += 1,
                Some(_) => return Err(self.invalid("after object key")),
            }
            self.skip_ws();
            let value = self.value()?;
            members.push((key, value));
            self.skip_ws();
            match self.peek() {
                None => return Err(Self::eof()),
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(JsonValue::Object(members));
                }
                Some(_) => return Err(self.invalid("after object key:value pair")),
            }
        }
    }

    fn array(&mut self) -> Result<JsonValue, SyntaxError> {
        self.enter()?;
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                None => return Err(Self::eof()),
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(JsonValue::Array(items));
                }
                Some(_) => return Err(self.invalid("after array element")),
            }
        }
    }

    fn literal(&mut self, word: &str, value: JsonValue) -> Result<JsonValue, SyntaxError> {
        for (i, expected) in word.bytes().enumerate().skip(1) {
            self.pos += 1;
            match self.peek() {
                None => return Err(Self::eof()),
                Some(b) if b == expected => {}
                Some(_) => {
                    let context = format!(
                        "in literal {word} (expecting {})",
                        quote_char(char::from(word.as_bytes()[i]))
                    );
                    return Err(self.invalid(&context));
                }
            }
        }
        self.pos += 1;
        Ok(value)
    }

    fn number(&mut self) -> Result<JsonValue, SyntaxError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            None => return Err(Self::eof()),
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.digits(),
            Some(_) => return Err(self.invalid("in numeric literal")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            match self.peek() {
                None => return Err(Self::eof()),
                Some(b'0'..=b'9') => self.digits(),
                Some(_) => return Err(self.invalid("after decimal point in numeric literal")),
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.pos += 1;
            }
            match self.peek() {
                None => return Err(Self::eof()),
                Some(b'0'..=b'9') => self.digits(),
                Some(_) => return Err(self.invalid("in exponent of numeric literal")),
            }
        }
        let literal = std::str::from_utf8(&self.bytes[start..self.pos]).expect("ASCII digits");
        Ok(JsonValue::Number(literal.to_owned()))
    }

    fn digits(&mut self) {
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
    }

    fn string(&mut self) -> Result<String, SyntaxError> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let Some(c) = self.current_char() else {
                return Err(Self::eof());
            };
            match c {
                '"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                '\\' => {
                    self.pos += 1;
                    let Some(escape) = self.peek() else {
                        return Err(Self::eof());
                    };
                    let decoded = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            self.pos += 1;
                            out.push(self.unicode_escape()?);
                            continue;
                        }
                        _ => return Err(self.invalid("in string escape code")),
                    };
                    self.pos += 1;
                    out.push(decoded);
                }
                c if (c as u32) < 0x20 => return Err(self.invalid("in string literal")),
                c => {
                    self.pos += c.len_utf8();
                    out.push(c);
                }
            }
        }
    }

    /// Reads `XXXX` after `\u`, joining a surrogate pair. A lone surrogate becomes
    /// U+FFFD, as in Go.
    fn unicode_escape(&mut self) -> Result<char, SyntaxError> {
        let first = self.hex4()?;
        if (0xD800..0xDC00).contains(&first)
            && self.bytes.get(self.pos) == Some(&b'\\')
            && self.bytes.get(self.pos + 1) == Some(&b'u')
        {
            let save = self.pos;
            self.pos += 2;
            let second = self.hex4()?;
            if (0xDC00..0xE000).contains(&second) {
                let combined = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                return Ok(char::from_u32(combined).unwrap_or('\u{fffd}'));
            }
            self.pos = save;
        }
        Ok(char::from_u32(first).unwrap_or('\u{fffd}'))
    }

    fn hex4(&mut self) -> Result<u32, SyntaxError> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = match self.peek() {
                None => return Err(Self::eof()),
                Some(b) => char::from(b).to_digit(16),
            };
            let Some(digit) = digit else {
                return Err(self.invalid("in \\u hexadecimal character escape"));
            };
            value = value * 16 + digit;
            self.pos += 1;
        }
        Ok(value)
    }
}

/// Go's `quoteChar` in `encoding/json` syntax errors: `'x'`, `'\''`, `'"'`.
pub fn quote_char(c: char) -> String {
    match c {
        '\'' => "'\\''".to_owned(),
        '"' => "'\"'".to_owned(),
        c => {
            let quoted = gofmt::quote(&c.to_string());
            format!("'{}'", &quoted[1..quoted.len() - 1])
        }
    }
}

/// Compact JSON exactly as Go `json.Marshal` writes it for these values (numbers are
/// written as their literal text; object members in stored order).
pub fn marshal(value: &JsonValue) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &JsonValue) {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JsonValue::Number(lit) => out.push_str(lit),
        JsonValue::String(s) => write_string(out, s),
        JsonValue::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        JsonValue::Object(members) => {
            out.push('{');
            for (i, (key, item)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

/// A JSON string with Go's HTML-safe escaping.
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
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Go's JSON form of a `float64`: shortest digits, plain notation unless the magnitude
/// is `< 1e-6` or `>= 1e21`, then `1e+21` / `1e-7`. NaN and infinities cannot be
/// encoded (`json: unsupported value: +Inf`).
pub fn encode_f64(x: f64) -> Result<String, String> {
    if !x.is_finite() {
        return Err(format!("json: unsupported value: {}", gofmt::fmt_v_f64(x)));
    }
    let abs = x.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let s = format!("{x:e}");
        // Rust writes `1e21` and `1e-7`; Go writes `1e+21` and (after its cleanup)
        // `1e-7`.
        Ok(match s.split_once('e') {
            Some((mantissa, exp)) if !exp.starts_with('-') => format!("{mantissa}e+{exp}"),
            _ => s,
        })
    } else {
        Ok(format!("{x}"))
    }
}

/// `graders.JSONFromValue` for a YAML value: map keys must be strings (else
/// `graders: envelope: non-string map key <key>`), then the value is marshaled.
pub fn from_yaml(value: &serde_norway::Value) -> Result<JsonValue, String> {
    check_keys(value)?;
    convert(value).map_err(|e| format!("graders: envelope: {e}"))
}

/// Go converts every map key before it marshals, so a key error wins over a value that
/// cannot be marshaled.
fn check_keys(value: &serde_norway::Value) -> Result<(), String> {
    use serde_norway::Value;
    match value {
        Value::Sequence(items) => items.iter().try_for_each(check_keys),
        Value::Mapping(map) => {
            for (key, item) in map {
                if !matches!(key, Value::String(_)) {
                    return Err(format!(
                        "graders: envelope: non-string map key {}",
                        go_v_yaml(key)
                    ));
                }
                check_keys(item)?;
            }
            Ok(())
        }
        Value::Tagged(tagged) => check_keys(&tagged.value),
        _ => Ok(()),
    }
}

fn convert(value: &serde_norway::Value) -> Result<JsonValue, String> {
    use serde_norway::Value;
    Ok(match value {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Number(n) => JsonValue::Number(yaml_number(n)?),
        Value::String(s) => JsonValue::String(s.clone()),
        Value::Sequence(items) => {
            JsonValue::Array(items.iter().map(convert).collect::<Result<_, _>>()?)
        }
        Value::Mapping(map) => {
            let mut members: Vec<(String, JsonValue)> = Vec::with_capacity(map.len());
            for (key, item) in map {
                let Value::String(key) = key else {
                    unreachable!("check_keys rejected non-string keys");
                };
                members.push((key.clone(), convert(item)?));
            }
            members.sort_by(|a, b| a.0.cmp(&b.0));
            JsonValue::Object(members)
        }
        Value::Tagged(tagged) => convert(&tagged.value)?,
    })
}

fn yaml_number(n: &serde_norway::Number) -> Result<String, String> {
    if let Some(u) = n.as_u64() {
        Ok(u.to_string())
    } else if let Some(i) = n.as_i64() {
        Ok(i.to_string())
    } else {
        encode_f64(n.as_f64().unwrap_or(f64::NAN))
    }
}

/// Go `%v` of a decoded YAML scalar, for the non-string key message.
fn go_v_yaml(value: &serde_norway::Value) -> String {
    use serde_norway::Value;
    match value {
        Value::Null => "<nil>".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match (n.as_u64(), n.as_i64()) {
            (Some(u), _) => u.to_string(),
            (_, Some(i)) => i.to_string(),
            _ => gofmt::fmt_v_f64(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => s.clone(),
        Value::Sequence(_) => "[...]".to_owned(),
        Value::Mapping(_) => "map[...]".to_owned(),
        Value::Tagged(tagged) => go_v_yaml(&tagged.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(members: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::Object(
            members
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        )
    }

    fn num(lit: &str) -> JsonValue {
        JsonValue::Number(lit.to_owned())
    }

    #[test]
    fn long_strings_decode_in_linear_time() {
        // Revalidating the rest of the input per character made this quadratic: a 2 MiB
        // request body or model reply took hours in a debug build.
        let text = "é".repeat(1 << 20);
        let input = format!("\"{text}\" x");
        let (value, rest) = decode_one(&input).unwrap();
        assert_eq!(value, JsonValue::String(text.clone()));
        assert_eq!(rest, " x");
        let error = decode_one(&format!("\"{text}\u{1}\"")).unwrap_err();
        assert_eq!(error.0, "invalid character '\\x01' in string literal");
        assert_eq!(
            decode_one(&format!("\"{text}")).unwrap_err().0,
            "unexpected EOF"
        );
    }

    #[test]
    fn decode_keeps_order_duplicates_and_literals() {
        let (value, rest) = decode_one(r#" {"b":1.0,"a":[true,null],"b":"x"} tail"#).unwrap();
        assert_eq!(
            value,
            obj(&[
                ("b", num("1.0")),
                (
                    "a",
                    JsonValue::Array(vec![JsonValue::Bool(true), JsonValue::Null])
                ),
                ("b", JsonValue::String("x".to_owned())),
            ])
        );
        assert_eq!(rest, " tail");
        assert_eq!(decode_one("1e309").unwrap().0, num("1e309"));
        assert_eq!(decode_one("-0.5E+3").unwrap().0, num("-0.5E+3"));
    }

    #[test]
    fn decode_strings_and_escapes() {
        let (value, _) = decode_one(r#""a\"\\\/\b\f\n\r\t\u00e9\ud83d\ude00\ud800x""#).unwrap();
        assert_eq!(
            value,
            JsonValue::String("a\"\\/\u{8}\u{c}\n\r\té😀\u{fffd}x".to_owned())
        );
    }

    #[test]
    fn decode_errors_use_go_text() {
        for (input, want) in [
            ("", "EOF"),
            ("   ", "EOF"),
            ("{", "unexpected EOF"),
            ("[1,", "unexpected EOF"),
            ("\"abc", "unexpected EOF"),
            ("x", "invalid character 'x' looking for beginning of value"),
            (
                "{1:2}",
                "invalid character '1' looking for beginning of object key string",
            ),
            ("{\"a\" 1}", "invalid character '1' after object key"),
            (
                "{\"a\":1 \"b\"}",
                "invalid character '\"' after object key:value pair",
            ),
            ("[1 2]", "invalid character '2' after array element"),
            ("\"a\u{1}\"", "invalid character '\\x01' in string literal"),
            ("\"\\q\"", "invalid character 'q' in string escape code"),
            ("tru", "unexpected EOF"),
            (
                "trux",
                "invalid character 'x' in literal true (expecting 'e')",
            ),
            ("-x", "invalid character 'x' in numeric literal"),
            (
                "'a'",
                "invalid character '\\'' looking for beginning of value",
            ),
        ] {
            assert_eq!(decode_one(input).unwrap_err().0, want, "{input:?}");
        }
    }

    #[test]
    fn unmarshal_uses_go_whole_document_errors() {
        assert_eq!(
            unmarshal(" [1] ").unwrap(),
            JsonValue::Array(vec![num("1")])
        );
        for (input, want) in [
            ("", "unexpected end of JSON input"),
            ("  ", "unexpected end of JSON input"),
            ("[1", "unexpected end of JSON input"),
            ("[1] x", "invalid character 'x' after top-level value"),
            ("{} {}", "invalid character '{' after top-level value"),
            (
                "{b",
                "invalid character 'b' looking for beginning of object key string",
            ),
        ] {
            assert_eq!(unmarshal(input).unwrap_err().0, want, "{input:?}");
        }
    }

    #[test]
    fn match_field_prefers_exact_then_folds() {
        const FIELDS: [&str; 2] = ["name", "content"];
        assert_eq!(match_field("name", &FIELDS), Some("name"));
        assert_eq!(match_field("NAME", &FIELDS), Some("name"));
        assert_eq!(match_field("Content", &FIELDS), Some("content"));
        assert_eq!(match_field("names", &FIELDS), None);
        assert_eq!(match_field("nam", &FIELDS), None);
        assert_eq!(match_field("", &FIELDS), None);
        assert_eq!(match_field("ſpec", &["spec"]), Some("spec"));
        assert_eq!(match_field("K", &["k"]), Some("k"));
    }

    #[test]
    fn syntax_error_displays_go_text() {
        let error = decode_one("x").unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid character 'x' looking for beginning of value"
        );
    }

    #[test]
    fn unicode_escape_lookahead_needs_backslash_u_after_a_high_surrogate() {
        // Not a high surrogate: the following text is ordinary string content.
        assert_eq!(
            decode_one(r#""AXuZZZZ""#).unwrap().0,
            JsonValue::String("AXuZZZZ".to_owned())
        );
        // A high surrogate not followed by `\u`: U+FFFD, then ordinary content.
        assert_eq!(
            decode_one(r#""\ud800XuZZZZ""#).unwrap().0,
            JsonValue::String("\u{fffd}XuZZZZ".to_owned())
        );
        // A high surrogate followed by a non-low `\u` escape: both decode on their own.
        assert_eq!(
            decode_one(r#""\ud800A""#).unwrap().0,
            JsonValue::String("\u{fffd}A".to_owned())
        );
    }

    #[test]
    fn depth_is_released_when_containers_close() {
        // Far more sibling containers than the depth cap, of each kind and shape.
        let item = r#"{},{"a":1},[],[1]"#;
        let many = format!("[{}]", vec![item; MAX_DEPTH].join(","));
        assert!(decode_one(&many).is_ok());
    }

    #[test]
    fn deep_nesting_is_capped() {
        let deep = "[".repeat(MAX_DEPTH + 1);
        assert_eq!(decode_one(&deep).unwrap_err().0, "exceeded max depth");
        let deepest = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(decode_one(&deepest).is_ok());
        let deepest_objects = "{\"a\":".repeat(MAX_DEPTH) + "1" + &"}".repeat(MAX_DEPTH);
        assert!(decode_one(&deepest_objects).is_ok());
    }

    #[test]
    fn marshal_escapes_like_go() {
        let value = obj(&[
            ("html", JsonValue::String("<a href=\"x\">&</a>".to_owned())),
            (
                "ctl",
                JsonValue::String("\u{0}\u{8}\u{c}\u{1f}\u{7f}".to_owned()),
            ),
            ("ls", JsonValue::String("\u{2028}\u{2029}é".to_owned())),
            ("n", num("12500")),
        ]);
        assert_eq!(
            marshal(&value),
            r#"{"html":"\u003ca href=\"x\"\u003e\u0026\u003c/a\u003e","ctl":"\u0000\b\f\u001f"#
                .to_owned()
                + "\u{7f}"
                + r#"","ls":"\u2028\u2029é","n":12500}"#
        );
    }

    #[test]
    fn encode_f64_matches_go() {
        for (x, want) in [
            (1.0, "1"),
            (0.5, "0.5"),
            (1842.5, "1842.5"),
            (1e3, "1000"),
            (-0.0, "-0"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1.5e300, "1.5e+300"),
            (0.000_001, "0.000001"),
            (1e-7, "1e-7"),
            (-2.5e-10, "-2.5e-10"),
            (0.1 + 0.2, "0.30000000000000004"),
        ] {
            assert_eq!(encode_f64(x).unwrap(), want, "{x:?}");
        }
        assert_eq!(
            encode_f64(f64::INFINITY).unwrap_err(),
            "json: unsupported value: +Inf"
        );
        assert_eq!(
            encode_f64(f64::NAN).unwrap_err(),
            "json: unsupported value: NaN"
        );
    }

    #[test]
    fn from_yaml_sorts_keys_and_normalizes_numbers() {
        let yaml: serde_norway::Value = serde_norway::from_str(
            "type: key_facts\nversion: 1\nweight_dimensions: { completeness: 1.0 }\nspec: { facts: [ { id: a, any_of: [x] } ] }\n",
        )
        .unwrap();
        // graders_framework.json#json_from_yaml_value_example
        assert_eq!(
            marshal(&from_yaml(&yaml).unwrap()),
            r#"{"spec":{"facts":[{"any_of":["x"],"id":"a"}]},"type":"key_facts","version":1,"weight_dimensions":{"completeness":1}}"#
        );
        let yaml: serde_norway::Value =
            serde_norway::from_str("n: [-3, 1e3, 0.5, 18446744073709551615, ~, 'x<y>&']").unwrap();
        assert_eq!(
            marshal(&from_yaml(&yaml).unwrap()),
            r#"{"n":[-3,1000,0.5,18446744073709551615,null,"x\u003cy\u003e\u0026"]}"#
        );
    }

    #[test]
    fn from_yaml_rejects_non_string_keys_before_bad_values() {
        let yaml: serde_norway::Value = serde_norway::from_str("spec: {1: x}\nw: .inf\n").unwrap();
        // graders_framework.json#parse_envelope_value_non_string_keys
        assert_eq!(
            from_yaml(&yaml).unwrap_err(),
            "graders: envelope: non-string map key 1"
        );
        let yaml: serde_norway::Value = serde_norway::from_str("w: .inf\n").unwrap();
        assert_eq!(
            from_yaml(&yaml).unwrap_err(),
            "graders: envelope: json: unsupported value: +Inf"
        );
        let yaml: serde_norway::Value = serde_norway::from_str("{true: 1}").unwrap();
        assert_eq!(
            from_yaml(&yaml).unwrap_err(),
            "graders: envelope: non-string map key true"
        );
        // Keys are checked inside sequences and tagged values too.
        for (text, key) in [
            ("spec: [{1: x}]", "1"),
            ("spec: !custom {2: x}", "2"),
            ("{18446744073709551615: x}", "18446744073709551615"),
            ("{-1000000: x}", "-1000000"),
            ("{1.5: x}", "1.5"),
            ("{~: x}", "<nil>"),
        ] {
            let yaml: serde_norway::Value = serde_norway::from_str(text).unwrap();
            assert_eq!(
                from_yaml(&yaml).unwrap_err(),
                format!("graders: envelope: non-string map key {key}"),
                "{text}"
            );
        }
        let yaml: serde_norway::Value = serde_norway::from_str("spec: !custom {a: 1}").unwrap();
        assert_eq!(marshal(&from_yaml(&yaml).unwrap()), r#"{"spec":{"a":1}}"#);
    }

    #[test]
    fn to_serde_applies_last_duplicate() {
        let (value, _) = decode_one(r#"{"a":1,"a":2.5,"b":[null]}"#).unwrap();
        assert_eq!(
            value.to_serde().unwrap(),
            serde_json::json!({"a": 2.5, "b": [null]})
        );
    }
}
