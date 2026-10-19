//! Decoding of literal token text into values.
//!
//! The lexer only classifies literals; the parser calls these helpers to obtain the
//! value and to report malformed literals (overflow, bad escapes) with precise spans.
//! Offsets in errors are relative to the start of the literal text.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntSuffix {
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
}

impl IntSuffix {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "i8" => Self::I8,
            "i16" => Self::I16,
            "i32" => Self::I32,
            "i64" => Self::I64,
            "i128" => Self::I128,
            "isize" => Self::Isize,
            "u8" => Self::U8,
            "u16" => Self::U16,
            "u32" => Self::U32,
            "u64" => Self::U64,
            "u128" => Self::U128,
            "usize" => Self::Usize,
            _ => return None,
        })
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::I128 => "i128",
            Self::Isize => "isize",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::U128 => "u128",
            Self::Usize => "usize",
        }
    }
}

impl fmt::Display for IntSuffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FloatSuffix {
    F32,
    F64,
}

impl FloatSuffix {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
}

impl fmt::Display for FloatSuffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiteralError {
    /// The digits do not fit in `u128`.
    IntegerTooLarge,
    /// A literal like `0x` with no digits after the radix prefix.
    NoDigits,
    /// An escape sequence that is not recognised; the range is relative to the literal.
    InvalidEscape { start: usize, end: usize },
    /// `\u{...}` naming a code point that is not a valid `char`.
    InvalidUnicodeEscape { start: usize, end: usize },
    /// A character literal holding zero or more than one character.
    NotASingleChar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntLiteral {
    pub value: u128,
    pub suffix: Option<IntSuffix>,
}

/// Decodes integer literal text as produced by the lexer (`42`, `1_000u32`, `0xFF`).
pub fn parse_int(text: &str) -> Result<IntLiteral, LiteralError> {
    let (body, suffix) = split_int_suffix(text);
    let (radix, digits) = match body.get(..2) {
        Some("0x") => (16, &body[2..]),
        Some("0o") => (8, &body[2..]),
        Some("0b") => (2, &body[2..]),
        _ => (10, body),
    };
    let mut value: u128 = 0;
    let mut saw_digit = false;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        saw_digit = true;
        let d = c
            .to_digit(radix)
            .expect("lexer only admits digits of the radix");
        value = value
            .checked_mul(u128::from(radix))
            .and_then(|v| v.checked_add(u128::from(d)))
            .ok_or(LiteralError::IntegerTooLarge)?;
    }
    if !saw_digit {
        return Err(LiteralError::NoDigits);
    }
    Ok(IntLiteral { value, suffix })
}

fn split_int_suffix(text: &str) -> (&str, Option<IntSuffix>) {
    // Suffixes start with `i` or `u`; neither is a digit in any supported radix, so the
    // first such character (after a possible radix prefix) begins the suffix.
    let start = if text.starts_with("0x") || text.starts_with("0o") || text.starts_with("0b") {
        2
    } else {
        0
    };
    match text[start..].find(['i', 'u']) {
        Some(i) => {
            let i = start + i;
            (&text[..i], IntSuffix::parse(&text[i..]))
        }
        None => (text, None),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloatLiteral {
    pub value: f64,
    pub suffix: Option<FloatSuffix>,
}

/// Decodes float literal text (`1.5`, `2e10`, `3.0f32`).
pub fn parse_float(text: &str) -> Result<FloatLiteral, LiteralError> {
    let (body, suffix) = if let Some(b) = text.strip_suffix("f32") {
        (b, Some(FloatSuffix::F32))
    } else if let Some(b) = text.strip_suffix("f64") {
        (b, Some(FloatSuffix::F64))
    } else {
        (text, None)
    };
    let cleaned: String = body.chars().filter(|&c| c != '_').collect();
    let value: f64 = cleaned.parse().map_err(|_| LiteralError::NoDigits)?;
    Ok(FloatLiteral { value, suffix })
}

/// Decodes the body of a string literal (text between the quotes).
pub fn unescape_str(body: &str) -> Result<String, LiteralError> {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(&(_, next)) = chars.peek() else {
            return Err(LiteralError::InvalidEscape {
                start: i,
                end: i + 1,
            });
        };
        if next == '\n' || (next == '\r' && body[i + 1..].starts_with("\r\n")) {
            // Line continuation (LF or CRLF): skip the newline and following whitespace.
            // The whitespace loop also swallows the LF of a CRLF pair.
            chars.next();
            while matches!(chars.peek(), Some(&(_, w)) if w.is_whitespace()) {
                chars.next();
            }
            continue;
        }
        let (ch, end) = decode_escape(body, i)?;
        out.push(ch);
        while matches!(chars.peek(), Some(&(j, _)) if j < end) {
            chars.next();
        }
    }
    Ok(out)
}

/// Decodes the body of a character literal (text between the quotes).
pub fn unescape_char(body: &str) -> Result<char, LiteralError> {
    let mut chars = body.chars();
    let first = chars.next().ok_or(LiteralError::NotASingleChar)?;
    if first != '\\' {
        return if chars.next().is_none() {
            Ok(first)
        } else {
            Err(LiteralError::NotASingleChar)
        };
    }
    let (ch, end) = decode_escape(body, 0)?;
    if end == body.len() {
        Ok(ch)
    } else {
        Err(LiteralError::NotASingleChar)
    }
}

/// How many characters after `\u{` are searched for the closing `}`: room for six hex
/// digits plus separators, matching the `\u{...}` form the lexer's `Char` regex admits.
const UNICODE_ESCAPE_WINDOW: usize = 8;

/// Decodes the escape starting at `start` (which must be a backslash). Returns the
/// character and the offset just past the escape.
fn decode_escape(text: &str, start: usize) -> Result<(char, usize), LiteralError> {
    let rest = &text[start + 1..];
    let mut it = rest.chars();
    let Some(kind) = it.next() else {
        return Err(LiteralError::InvalidEscape {
            start,
            end: start + 1,
        });
    };
    let simple = match kind {
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        '\\' => Some('\\'),
        '0' => Some('\0'),
        '\'' => Some('\''),
        '"' => Some('"'),
        _ => None,
    };
    if let Some(c) = simple {
        return Ok((c, start + 2));
    }
    match kind {
        'x' => {
            let hex = rest.get(1..3).ok_or(LiteralError::InvalidEscape {
                start,
                end: start + 2,
            })?;
            // `from_str_radix` would accept a leading `+`; only hex digits are allowed.
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(LiteralError::InvalidEscape {
                    start,
                    end: start + 4,
                });
            }
            let v = u8::from_str_radix(hex, 16).map_err(|_| LiteralError::InvalidEscape {
                start,
                end: start + 4,
            })?;
            if v > 0x7F {
                return Err(LiteralError::InvalidEscape {
                    start,
                    end: start + 4,
                });
            }
            Ok((v as char, start + 4))
        }
        'u' => {
            if !rest[1..].starts_with('{') {
                return Err(LiteralError::InvalidEscape {
                    start,
                    end: start + 2,
                });
            }
            // `\u{` + up to six hex digits (with optional `_` separators) + `}`. The
            // closing brace must appear within [`UNICODE_ESCAPE_WINDOW`] so a malformed
            // escape is reported over `\u{...` and not over the whole rest of the literal.
            let inner = &rest[2..];
            let window_end = inner
                .char_indices()
                .nth(UNICODE_ESCAPE_WINDOW)
                .map_or(inner.len(), |(j, _)| j);
            let Some(close) = inner[..window_end].find('}') else {
                return Err(LiteralError::InvalidEscape {
                    start,
                    end: start + 3 + window_end,
                });
            };
            let end = start + 3 + close + 1;
            let digits: String = inner[..close].chars().filter(|&c| c != '_').collect();
            // `from_str_radix` would accept a leading `+`; require real hex digits, and
            // at least one of them.
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(LiteralError::InvalidEscape { start, end });
            }
            let v = u32::from_str_radix(&digits, 16)
                .map_err(|_| LiteralError::InvalidUnicodeEscape { start, end })?;
            let c = char::from_u32(v).ok_or(LiteralError::InvalidUnicodeEscape { start, end })?;
            Ok((c, end))
        }
        other => Err(LiteralError::InvalidEscape {
            start,
            end: start + 1 + other.len_utf8(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints() {
        assert_eq!(
            parse_int("42"),
            Ok(IntLiteral {
                value: 42,
                suffix: None
            })
        );
        assert_eq!(
            parse_int("1_000u32"),
            Ok(IntLiteral {
                value: 1000,
                suffix: Some(IntSuffix::U32)
            })
        );
        assert_eq!(
            parse_int("0xFFi64"),
            Ok(IntLiteral {
                value: 255,
                suffix: Some(IntSuffix::I64)
            })
        );
        assert_eq!(
            parse_int("0o17"),
            Ok(IntLiteral {
                value: 15,
                suffix: None
            })
        );
        assert_eq!(
            parse_int("0b1010usize"),
            Ok(IntLiteral {
                value: 10,
                suffix: Some(IntSuffix::Usize)
            })
        );
        assert_eq!(parse_int("0x_"), Err(LiteralError::NoDigits));
        assert_eq!(
            parse_int("340282366920938463463374607431768211456"),
            Err(LiteralError::IntegerTooLarge)
        );
        assert_eq!(
            parse_int("340282366920938463463374607431768211455").map(|l| l.value),
            Ok(u128::MAX)
        );
    }

    #[test]
    fn floats() {
        let f = parse_float("1_0.5e1f32").unwrap();
        assert!((f.value - 105.0).abs() < f64::EPSILON);
        assert_eq!(f.suffix, Some(FloatSuffix::F32));
        assert!((parse_float("2e3").unwrap().value - 2000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn strings() {
        assert_eq!(
            unescape_str(r#"a\nb\t\\\"\'\0"#),
            Ok("a\nb\t\\\"'\0".to_owned())
        );
        assert_eq!(
            unescape_str(r"\x41\u{1F600}\u{00_41}"),
            Ok("A😀A".to_owned())
        );
        assert_eq!(
            unescape_str("line\\\n    continued"),
            Ok("linecontinued".to_owned())
        );
        assert_eq!(
            unescape_str("a\\\r\n   b"),
            Ok("ab".to_owned()),
            "CRLF line continuation"
        );
        assert_eq!(
            unescape_str(r"bad \q"),
            Err(LiteralError::InvalidEscape { start: 4, end: 6 })
        );
        assert_eq!(
            unescape_str(r"\u{D800}"),
            Err(LiteralError::InvalidUnicodeEscape { start: 0, end: 8 })
        );
        assert_eq!(
            unescape_str(r"\xFF"),
            Err(LiteralError::InvalidEscape { start: 0, end: 4 })
        );
    }

    #[test]
    fn signed_or_malformed_hex_escapes_are_rejected() {
        assert_eq!(
            unescape_str(r"\x+1"),
            Err(LiteralError::InvalidEscape { start: 0, end: 4 })
        );
        assert_eq!(
            unescape_str(r"\u{+41}"),
            Err(LiteralError::InvalidEscape { start: 0, end: 7 })
        );
        assert_eq!(
            unescape_str(r"\u{}"),
            Err(LiteralError::InvalidEscape { start: 0, end: 4 })
        );
        assert_eq!(
            unescape_str(r"\u{41 x}"),
            Err(LiteralError::InvalidEscape { start: 0, end: 8 })
        );
        assert_eq!(
            unescape_str(r"\u{_}"),
            Err(LiteralError::InvalidEscape { start: 0, end: 5 })
        );
        // The search for `}` is bounded: the span covers `\u{` plus the window, not the
        // rest of the literal.
        assert_eq!(
            unescape_str(r"\u{41414141414141} tail"),
            Err(LiteralError::InvalidEscape { start: 0, end: 11 })
        );
        assert_eq!(
            unescape_char(r"\x+1"),
            Err(LiteralError::InvalidEscape { start: 0, end: 4 })
        );
        assert_eq!(
            unescape_char(r"\u{+41}"),
            Err(LiteralError::InvalidEscape { start: 0, end: 7 })
        );
    }

    #[test]
    fn chars() {
        assert_eq!(unescape_char("a"), Ok('a'));
        assert_eq!(unescape_char(r"\n"), Ok('\n'));
        assert_eq!(unescape_char(r"\u{1F600}"), Ok('😀'));
        assert_eq!(unescape_char("ab"), Err(LiteralError::NotASingleChar));
        assert_eq!(unescape_char(""), Err(LiteralError::NotASingleChar));
    }
}
