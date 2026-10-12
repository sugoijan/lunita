//! The conversion specifications of `string.format`, emulating C's `printf` as PUC-Rio Lua uses it.

use std::io::Write as _;

use thiserror::Error;

use crate::compat::{format_float, FloatFormat};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FormatError {
    #[error("invalid format (repeated flags)")]
    RepeatedFlags,
    #[error("invalid format (width or precision too long)")]
    TooLong,
    #[error("invalid format (too long)")]
    FormatTooLong,
    #[error("invalid conversion specification: '%{0}'")]
    InvalidSpecification(std::string::String),
    #[error("invalid option '%{0}' to 'format'")]
    InvalidOption(char),
    #[error("invalid conversion '%{0}' to 'format'")]
    InvalidConversion(std::string::String),
    #[error("specifier '%q' cannot have modifiers")]
    QModifiers,
}

/// A parsed conversion specification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    pub left: bool,
    pub plus: bool,
    pub space: bool,
    pub alternate: bool,
    pub zero: bool,
    pub width: usize,
    pub precision: Option<usize>,
    pub conversion: u8,
    /// Whether the specification had any flag, width or precision.
    pub modifiers: bool,
}

impl Spec {
    // Sets the flag `c`, returning whether it is one.
    fn set_flag(&mut self, c: u8) -> bool {
        match c {
            b'-' => self.left = true,
            b'+' => self.plus = true,
            b' ' => self.space = true,
            b'#' => self.alternate = true,
            b'0' => self.zero = true,
            _ => return false,
        }
        true
    }
}

// Reads up to two digits from `s` at `i`, moving `i` past them.
fn read_digits(s: &[u8], i: &mut usize) -> usize {
    let mut value = 0;
    for _ in 0..2 {
        match s.get(*i) {
            Some(&d) if d.is_ascii_digit() => {
                value = value * 10 + usize::from(d - b'0');
                *i += 1;
            }
            _ => break,
        }
    }
    value
}

/// Parses the specification after a '%' (which is not "%%"), as Lua 5.1 does: flags `-+ #0`
/// (five at most), a width and a precision of two digits at most. Returns the specification and
/// its length (including the conversion character).
pub fn parse_spec_51(s: &[u8]) -> Result<(Spec, usize), FormatError> {
    let mut spec = Spec::default();
    let mut i = 0;
    while s.get(i).is_some_and(|&c| spec.set_flag(c)) {
        i += 1;
    }
    if i >= 6 {
        return Err(FormatError::RepeatedFlags);
    }
    spec.width = read_digits(s, &mut i);
    if s.get(i) == Some(&b'.') {
        i += 1;
        spec.precision = Some(read_digits(s, &mut i));
    }
    if s.get(i).is_some_and(|c| c.is_ascii_digit()) {
        return Err(FormatError::TooLong);
    }
    spec.modifiers = i > 0;
    spec.conversion = s.get(i).copied().unwrap_or(0);
    Ok((spec, i + 1))
}

/// Parses the specification after a '%' (which is not "%%"), as Lua 5.4 does: the flags allowed
/// depend on the conversion, the width and precision have two digits at most. Returns the
/// specification and its length (including the conversion character).
pub fn parse_spec_54(s: &[u8]) -> Result<(Spec, usize), FormatError> {
    const SPANNED: &[u8] = b"-+#0 123456789.";
    let len = s.iter().take_while(|c| SPANNED.contains(c)).count();
    if len + 1 >= 32 - 10 {
        return Err(FormatError::FormatTooLong);
    }
    let form = s.get(..=len).unwrap_or(s);
    let conversion = s.get(len).copied().unwrap_or(0);
    let display = || std::string::String::from_utf8_lossy(form).into_owned();

    let (flags, precision_allowed): (&[u8], bool) = match conversion {
        b'c' | b'p' => (b"-", false),
        b'd' | b'i' => (b"-+0 ", true),
        b'u' => (b"-0", true),
        b'o' | b'x' | b'X' => (b"-#0", true),
        b'a' | b'A' | b'e' | b'E' | b'f' | b'g' | b'G' => (b"-+#0 ", true),
        b's' => (b"-", true),
        b'q' => {
            if len > 0 {
                return Err(FormatError::QModifiers);
            }
            (b"", false)
        }
        _ => return Err(FormatError::InvalidConversion(display())),
    };

    let mut spec = Spec {
        conversion,
        modifiers: len > 0,
        ..Spec::default()
    };
    let mut i = 0;
    while s
        .get(i)
        .is_some_and(|&c| flags.contains(&c) && spec.set_flag(c))
    {
        i += 1;
    }
    // A width cannot start with '0'.
    if s.get(i) != Some(&b'0') {
        spec.width = read_digits(s, &mut i);
        if s.get(i) == Some(&b'.') && precision_allowed {
            i += 1;
            spec.precision = Some(read_digits(s, &mut i));
        }
    }
    if i != len {
        return Err(FormatError::InvalidSpecification(display()));
    }
    Ok((spec, len + 1))
}

// Pads `body` (whose sign or prefix is its first `prefix` bytes) to the width, with zeros after
// the prefix if `zero_pad`, or with spaces.
fn pad(out: &mut Vec<u8>, spec: &Spec, body: &[u8], prefix: usize, zero_pad: bool) {
    let fill = spec.width.saturating_sub(body.len());
    if spec.left {
        out.extend_from_slice(body);
        out.resize(out.len() + fill, b' ');
    } else if zero_pad {
        let (sign, digits) = body.split_at(prefix.min(body.len()));
        out.extend_from_slice(sign);
        out.resize(out.len() + fill, b'0');
        out.extend_from_slice(digits);
    } else {
        out.resize(out.len() + fill, b' ');
        out.extend_from_slice(body);
    }
}

fn sign_prefix(spec: &Spec, negative: bool) -> &'static [u8] {
    if negative {
        b"-"
    } else if spec.plus {
        b"+"
    } else if spec.space {
        b" "
    } else {
        b""
    }
}

/// Formats a signed integer (`%d`, `%i`).
pub fn format_signed(out: &mut Vec<u8>, spec: &Spec, n: i64) {
    if !spec.modifiers {
        let _ = write!(out, "{n}");
        return;
    }
    let mut body = sign_prefix(spec, n < 0).to_vec();
    let prefix = body.len();
    let mut digits = n.unsigned_abs().to_string().into_bytes();
    apply_precision(spec, &mut digits);
    body.extend_from_slice(&digits);
    pad(
        out,
        spec,
        &body,
        prefix,
        spec.zero && spec.precision.is_none(),
    );
}

/// Formats an unsigned integer (`%u`, `%o`, `%x`, `%X`).
pub fn format_unsigned(out: &mut Vec<u8>, spec: &Spec, n: u64) {
    if !spec.modifiers {
        let _ = match spec.conversion {
            b'o' => write!(out, "{n:o}"),
            b'x' => write!(out, "{n:x}"),
            b'X' => write!(out, "{n:X}"),
            _ => write!(out, "{n}"),
        };
        return;
    }
    let mut digits = match spec.conversion {
        b'o' => format!("{n:o}"),
        b'x' => format!("{n:x}"),
        b'X' => format!("{n:X}"),
        _ => n.to_string(),
    }
    .into_bytes();
    apply_precision(spec, &mut digits);
    let mut body = Vec::new();
    if spec.alternate {
        match spec.conversion {
            // The '#' flag makes the first digit of an octal number a zero.
            b'o' if digits.first() != Some(&b'0') => body.push(b'0'),
            b'x' if n != 0 => body.extend_from_slice(b"0x"),
            b'X' if n != 0 => body.extend_from_slice(b"0X"),
            _ => {}
        }
    }
    let prefix = body.len();
    body.extend_from_slice(&digits);
    pad(
        out,
        spec,
        &body,
        prefix,
        spec.zero && spec.precision.is_none(),
    );
}

// The precision of an integer conversion is its minimum number of digits, and a zero with a
// precision of zero has no digits.
fn apply_precision(spec: &Spec, digits: &mut Vec<u8>) {
    if let Some(precision) = spec.precision {
        if precision == 0 && digits.as_slice() == b"0" {
            digits.clear();
        } else if digits.len() < precision {
            let mut padded = vec![b'0'; precision - digits.len()];
            padded.append(digits);
            *digits = padded;
        }
    }
}

/// Formats a float (`%e`, `%E`, `%f`, `%F`, `%g`, `%G`).
pub fn format_double(out: &mut Vec<u8>, spec: &Spec, n: f64) {
    let format = match spec.conversion {
        b'e' | b'E' => FloatFormat::E,
        b'f' | b'F' => FloatFormat::F,
        _ => FloatFormat::G,
    };
    let upper = spec.conversion.is_ascii_uppercase();
    let mut body = sign_prefix(spec, n.is_sign_negative() && !n.is_nan()).to_vec();
    let prefix = body.len();
    let mut digits = Vec::new();
    format_float(
        &mut digits,
        n.abs(),
        format,
        spec.precision,
        spec.alternate,
        upper,
    );
    if n.is_nan() && n.is_sign_negative() {
        // `format_float` writes the sign of NaN itself.
        body.clear();
        body.extend_from_slice(if upper { b"-NAN" } else { b"-nan" });
    } else {
        body.extend_from_slice(&digits);
    }
    // Infinity and NaN are never padded with zeros.
    pad(out, spec, &body, prefix, spec.zero && n.is_finite());
}

// DELIBERATE(puc): `%a` is formatted here, not by the C library (COMPATIBILITY.md lists its rounding
// as a difference from PUC, since libcs differ among themselves).
/// Formats a float in hexadecimal (`%a`, `%A`) in glibc's style: subnormals with a zero leading
/// digit and the exponent -1022, and a precision rounding to nearest, ties to even, without
/// renormalizing (macOS prints subnormals normalized and rounds ties differently).
pub fn format_hex_double(out: &mut Vec<u8>, spec: &Spec, n: f64) {
    let upper = spec.conversion == b'A';
    let mut body = sign_prefix(spec, n.is_sign_negative()).to_vec();
    let prefix = body.len();
    let mut hex = Vec::new();
    if n.is_nan() {
        hex.extend_from_slice(b"nan");
    } else if n.is_infinite() {
        hex.extend_from_slice(b"inf");
    } else {
        let bits = n.abs().to_bits();
        let exponent = ((bits >> 52) & 0x7ff) as i64;
        let mut mantissa = bits & ((1 << 52) - 1);
        let (lead, exponent) = if exponent == 0 {
            if mantissa == 0 {
                (0, 0)
            } else {
                // Subnormal.
                (0, -1022)
            }
        } else {
            (1, exponent - 1023)
        };
        let mut digits = format!("{mantissa:013x}");
        match spec.precision {
            Some(precision) if precision < 13 => {
                // Round to nearest, ties to even, carrying into the leading digit (which is
                // the last digit kept when the precision is 0).
                let shift = 4 * (13 - precision);
                let rest = mantissa & ((1u64 << shift) - 1);
                let half = 1u64 << (shift - 1);
                mantissa >>= shift;
                let last = if precision == 0 { lead } else { mantissa };
                if rest > half || (rest == half && last & 1 == 1) {
                    mantissa += 1;
                }
                let lead = lead + (mantissa >> (4 * precision));
                let mantissa = mantissa & ((1u64 << (4 * precision)).wrapping_sub(1));
                digits = if precision == 0 {
                    std::string::String::new()
                } else {
                    format!("{mantissa:0precision$x}")
                };
                let _ = write!(hex, "0x{lead}");
            }
            Some(precision) => {
                let _ = write!(hex, "0x{lead}");
                digits.extend(std::iter::repeat_n('0', precision - 13));
            }
            None => {
                let _ = write!(hex, "0x{lead}");
                while digits.ends_with('0') {
                    digits.pop();
                }
            }
        }
        if !digits.is_empty() || spec.alternate {
            hex.push(b'.');
        }
        hex.extend_from_slice(digits.as_bytes());
        let _ = write!(hex, "p{exponent:+}");
    }
    if upper {
        hex.make_ascii_uppercase();
    }
    body.extend_from_slice(&hex);
    pad(out, spec, &body, prefix + 2, spec.zero && n.is_finite());
}

/// Formats a string (`%s`), truncated to the precision.
pub fn format_bytes(out: &mut Vec<u8>, spec: &Spec, s: &[u8]) {
    let s = match spec.precision {
        Some(precision) => s.get(..precision).unwrap_or(s),
        None => s,
    };
    pad(out, spec, s, 0, false);
}

/// Formats a character (`%c`).
pub fn format_char(out: &mut Vec<u8>, spec: &Spec, c: u8) {
    pad(out, spec, &[c], 0, false);
}

/// Quotes a string as Lua 5.1's `%q` does.
pub fn quote_51(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for &c in s {
        match c {
            b'"' | b'\\' | b'\n' => {
                out.push(b'\\');
                out.push(c);
            }
            b'\r' => out.extend_from_slice(b"\\r"),
            0 => out.extend_from_slice(b"\\000"),
            _ => out.push(c),
        }
    }
    out.push(b'"');
}

/// The length of `s` quoted by [`quote_51`].
pub fn quoted_len_51(s: &[u8]) -> usize {
    s.iter()
        .map(|&c| match c {
            b'"' | b'\\' | b'\n' | b'\r' => 2,
            0 => 4,
            _ => 1,
        })
        .fold(2, usize::saturating_add)
}

/// Quotes a string as Lua 5.4's `%q` does.
pub fn quote_54(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    for (i, &c) in s.iter().enumerate() {
        match c {
            b'"' | b'\\' | b'\n' => {
                out.push(b'\\');
                out.push(c);
            }
            c if c.is_ascii_control() => {
                out.push(b'\\');
                let digits = control_digits(c, s.get(i + 1));
                let decimal = [b'0' + c / 100, b'0' + c / 10 % 10, b'0' + c % 10];
                out.extend_from_slice(decimal.get(3 - digits..).unwrap_or(&decimal));
            }
            _ => out.push(c),
        }
    }
    out.push(b'"');
}

/// The length of `s` quoted by [`quote_54`].
pub fn quoted_len_54(s: &[u8]) -> usize {
    s.iter()
        .enumerate()
        .map(|(i, &c)| match c {
            b'"' | b'\\' | b'\n' => 2,
            c if c.is_ascii_control() => 1 + control_digits(c, s.get(i + 1)),
            _ => 1,
        })
        .fold(2, usize::saturating_add)
}

// The decimal digits Lua 5.4's `%q` writes for a control character: three if the next character
// is a digit, as few as possible otherwise.
fn control_digits(c: u8, next: Option<&u8>) -> usize {
    if next.is_some_and(u8::is_ascii_digit) || c >= 100 {
        3
    } else if c >= 10 {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(spec: &str, f: impl FnOnce(&mut Vec<u8>, &Spec)) -> std::string::String {
        let (spec, _) = parse_spec_51(spec.as_bytes()).unwrap();
        let mut out = Vec::new();
        f(&mut out, &spec);
        std::string::String::from_utf8(out).unwrap()
    }

    #[test]
    fn quoted_len() {
        // Every byte, alone, before a digit and before a letter.
        let mut inputs: Vec<Vec<u8>> = vec![b"".to_vec(), b"a\0\r\n\"\\9\x7f1".to_vec()];
        for c in 0..=255u8 {
            inputs.extend([vec![c], vec![c, b'7'], vec![c, b'x'], vec![b'0', c, c]]);
        }
        for s in &inputs {
            let mut out = Vec::new();
            quote_51(&mut out, s);
            assert_eq!(out.len(), quoted_len_51(s), "{s:?}");
            out.clear();
            quote_54(&mut out, s);
            assert_eq!(out.len(), quoted_len_54(s), "{s:?}");
        }
        // Lua 5.4's escapes, as `luaL_addquoted` writes them.
        let quote = |s: &[u8]| {
            let mut out = Vec::new();
            quote_54(&mut out, s);
            std::string::String::from_utf8(out).unwrap()
        };
        assert_eq!(quote(b"\0\x01\x1f\x7f"), "\"\\0\\1\\31\\127\"");
        assert_eq!(quote(b"\x011\x1f2\x7f3"), "\"\\0011\\0312\\1273\"");
        assert_eq!(quote(b"\r\n\"\\"), "\"\\13\\\n\\\"\\\\\"");
    }

    #[test]
    fn test_printf() {
        assert_eq!(fmt("5d", |o, s| format_signed(o, s, 42)), "   42");
        assert_eq!(fmt("-5d|", |o, s| format_signed(o, s, 42)), "42   ");
        assert_eq!(fmt("05d", |o, s| format_signed(o, s, -42)), "-0042");
        assert_eq!(fmt("+.3d", |o, s| format_signed(o, s, 7)), "+007");
        assert_eq!(fmt(".0d", |o, s| format_signed(o, s, 0)), "");
        assert_eq!(fmt("#x", |o, s| format_unsigned(o, s, 255)), "0xff");
        assert_eq!(fmt("#o", |o, s| format_unsigned(o, s, 8)), "010");
        assert_eq!(
            fmt("08.3f", |o, s| format_double(o, s, -2.71238)),
            "-002.712"
        );
        assert_eq!(
            fmt("e", |o, s| format_double(o, s, 12345.678)),
            "1.234568e+04"
        );
        assert_eq!(fmt("g", |o, s| format_double(o, s, 0.0001)), "0.0001");
        assert_eq!(fmt("#g", |o, s| format_double(o, s, 1.0)), "1.00000");
        assert_eq!(fmt("5.2s", |o, s| format_bytes(o, s, b"xxxx")), "   xx");
        assert_eq!(
            fmt("05f", |o, s| format_double(o, s, f64::INFINITY)),
            "  inf"
        );
    }

    #[test]
    fn test_hex_float() {
        let hex = |spec: &str, n: f64| {
            let (spec, _) = parse_spec_54(spec.as_bytes()).unwrap();
            let mut out = Vec::new();
            format_hex_double(&mut out, &spec, n);
            std::string::String::from_utf8(out).unwrap()
        };
        assert_eq!(hex("a", 1.0), "0x1p+0");
        assert_eq!(hex("a", 0.5), "0x1p-1");
        assert_eq!(hex("a", -0.1), "-0x1.999999999999ap-4");
        assert_eq!(hex("a", 0.0), "0x0p+0");
        assert_eq!(hex(".1a", 1.96875), "0x2.0p+0");
        assert_eq!(hex(".0a", 1.5), "0x2p+0");
        assert_eq!(hex(".0a", 1.25), "0x1p+0");
        assert_eq!(hex(".0a", 3.0), "0x2p+1");
        assert_eq!(hex("A", 255.0), "0X1.FEP+7");
    }
}
