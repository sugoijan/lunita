use std::{
    fmt::{self, Write as _},
    str,
};

pub fn trim_whitespace(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&c| !is_space(c)).unwrap_or(s.len());
    let end = s.iter().rposition(|&c| !is_space(c)).map_or(0, |i| i + 1);
    s.get(start..end).unwrap_or_default()
}

/// Display potentially non-UTF8 string in a lossy way, *without* allocating a new buffer to hold
/// the string.
pub fn display_utf8_lossy(input: &[u8]) -> impl fmt::Display + '_ {
    struct StringDisplay<'a>(&'a [u8]);

    impl<'a> fmt::Display for StringDisplay<'a> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let mut input = self.0;
            loop {
                match str::from_utf8(input) {
                    Ok(valid) => {
                        f.write_str(valid)?;
                        break;
                    }
                    Err(error) => {
                        let (valid, after_valid) = input.split_at(error.valid_up_to());
                        // `valid` is valid UTF-8 up to `valid_up_to`.
                        f.write_str(str::from_utf8(valid).unwrap_or_default())?;
                        f.write_str("\u{FFFD}")?;

                        if let Some(invalid_sequence_length) = error.error_len() {
                            input = after_valid
                                .get(invalid_sequence_length..)
                                .unwrap_or_default();
                        } else {
                            break;
                        }
                    }
                }
            }
            Ok(())
        }
    }

    StringDisplay(input)
}

/// Like [`display_utf8_lossy`], returns an `impl fmt::Debug` type that uses debug character escapes
/// and surrounds the string by '"' characters.
pub fn debug_utf8_lossy(input: &[u8]) -> impl fmt::Debug + '_ {
    struct StringDebug<'a>(&'a [u8]);

    impl<'a> fmt::Debug for StringDebug<'a> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fn write_escape_str(f: &mut fmt::Formatter, s: &str) -> fmt::Result {
                for c in s.chars() {
                    for c in c.escape_debug() {
                        f.write_char(c)?;
                    }
                }
                Ok(())
            }

            fn write_escape_bytes(f: &mut fmt::Formatter, s: &[u8]) -> fmt::Result {
                for b in s {
                    write!(f, "\\x{b:02x}")?;
                }
                Ok(())
            }

            f.write_char('"')?;
            let mut input = self.0;
            loop {
                match str::from_utf8(input) {
                    Ok(valid) => {
                        write_escape_str(f, valid)?;
                        break;
                    }
                    Err(error) => {
                        let (valid, after_valid) = input.split_at(error.valid_up_to());
                        // `valid` is valid UTF-8 up to `valid_up_to`.
                        write_escape_str(f, str::from_utf8(valid).unwrap_or_default())?;

                        if let Some(invalid_sequence_length) = error.error_len() {
                            let (invalid, rest) = after_valid
                                .split_at_checked(invalid_sequence_length)
                                .unwrap_or((after_valid, &[]));
                            write_escape_bytes(f, invalid)?;
                            input = rest;
                        } else {
                            write_escape_bytes(f, after_valid)?;
                            break;
                        }
                    }
                }
            }
            f.write_char('"')?;

            Ok(())
        }
    }

    StringDebug(input)
}

pub fn read_integer(s: &[u8]) -> Option<i64> {
    read_hex_integer(s).or_else(|| read_dec_integer(s))
}

pub fn read_dec_integer(s: &[u8]) -> Option<i64> {
    let (is_neg, s) = read_neg(s);

    if s.is_empty() {
        return None;
    }

    let mut i: u64 = 0;
    for &c in s {
        let d = from_digit(c)? as u64;
        i = i.checked_mul(10)?.checked_add(d)?;
    }

    if is_neg {
        if i <= i64::MAX as u64 {
            Some(-(i as i64))
        } else if i == i64::MAX as u64 + 1 {
            Some(i64::MIN)
        } else {
            None
        }
    } else {
        i.try_into().ok()
    }
}

/// Strips a `0x` or `0X` prefix.
fn strip_hex_prefix(s: &[u8]) -> Option<&[u8]> {
    s.strip_prefix(b"0x").or_else(|| s.strip_prefix(b"0X"))
}

pub fn read_hex_integer(s: &[u8]) -> Option<i64> {
    let (is_neg, s) = read_neg(s);

    let digits = strip_hex_prefix(s).filter(|d| !d.is_empty())?;

    // DELIBERATE(puc): hexadecimal integers wrap around as in Lua 5.4, in Lua51 mode too (5.1 gives
    // a float from 2^63; see COMPATIBILITY.md).
    let mut i: u64 = 0;
    for &c in digits {
        let d = from_hex_digit(c)? as u64;
        i = i.wrapping_mul(16).wrapping_add(d);
    }

    let i = i as i64;
    Some(if is_neg { i.wrapping_neg() } else { i })
}

pub fn read_float(s: &[u8]) -> Option<f64> {
    read_hex_float(s).or_else(|| read_dec_float(s))
}

pub fn read_dec_float(s: &[u8]) -> Option<f64> {
    // Only Lua's decimal numerals: Rust also parses "inf", "infinity" and "nan", Lua 5.4 does not.
    if s.iter().any(|c| matches!(c, b'n' | b'N')) {
        return None;
    }
    let s = str::from_utf8(s).ok()?;
    str::parse(s).ok()
}

/// Parses the longest prefix of `s` that C99's `strtod` would (as glibc does; Apple's libc also
/// takes any characters up to a `)` after `nan(`), returning the value and the length of the
/// prefix, or `None` if there is no number.
///
/// Accepts leading whitespace, a sign, decimal numerals with an optional exponent, hexadecimal
/// numerals with an optional binary exponent, and "inf", "infinity" and "nan" in any case.
pub fn parse_c_double(s: &[u8]) -> Option<(f64, usize)> {
    let (i, neg) = c_number_start(s);
    let sign = |n: f64| if neg { -n } else { n };

    let rest = s.get(i..).unwrap_or_default();
    let starts_with_ignore_case = |word: &[u8]| {
        rest.get(..word.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(word))
    };
    if starts_with_ignore_case(b"infinity") {
        return Some((sign(f64::INFINITY), i + 8));
    }
    if starts_with_ignore_case(b"inf") {
        return Some((sign(f64::INFINITY), i + 3));
    }
    if starts_with_ignore_case(b"nan") {
        let mut end = i + 3;
        // An optional "(n-char-sequence)".
        if s.get(end) == Some(&b'(') {
            if let Some(close) = s
                .get(end + 1..)
                .and_then(|r| {
                    r.iter()
                        .position(|&c| !(c.is_ascii_alphanumeric() || c == b'_'))
                })
                .filter(|&p| s.get(end + 1 + p) == Some(&b')'))
            {
                end += close + 2;
            }
        }
        return Some((sign(f64::NAN), end));
    }

    // Hexadecimal: "0x", hex digits with an optional point, then an optional "p" exponent.
    if matches!(rest, [b'0', b'x' | b'X', ..]) {
        let Some(end) = numeral_end(s, i + 2, true) else {
            // Without hex digits only the "0" is parsed.
            return Some((sign(0.0), i + 1));
        };
        let n = read_hex_float(s.get(i..end).unwrap_or_default())?;
        return Some((sign(n), end));
    }

    // Decimal: digits with an optional point, then an optional "e" exponent.
    let end = numeral_end(s, i, false)?;
    let text = str::from_utf8(s.get(i..end).unwrap_or_default()).ok()?;
    Some((sign(str::parse::<f64>(text).ok()?), end))
}

// Skips the leading whitespace and sign of a number for C's `strtod` and `strtoul`, returning where
// its digits start and whether it is negative.
fn c_number_start(s: &[u8]) -> (usize, bool) {
    let mut i = s.iter().position(|&c| !is_space(c)).unwrap_or(s.len());
    let neg = s.get(i) == Some(&b'-');
    if matches!(s.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    (i, neg)
}

// The end of the numeral at `start` as `strtod` reads it: digits (hexadecimal if `hex`) with an
// optional point, then an optional exponent ("p" if `hex`, "e" otherwise, with decimal digits), or
// `None` without digits.
fn numeral_end(s: &[u8], start: usize, hex: bool) -> Option<usize> {
    let digits_len = |from: usize, hex: bool| {
        s.get(from..)
            .unwrap_or_default()
            .iter()
            .take_while(|&&c| {
                if hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                }
            })
            .count()
    };
    let int_len = digits_len(start, hex);
    let mut end = start + int_len;
    let mut frac_len = 0;
    if s.get(end) == Some(&b'.') {
        frac_len = digits_len(end + 1, hex);
        if int_len + frac_len > 0 {
            end += 1 + frac_len;
        }
    }
    if int_len + frac_len == 0 {
        return None;
    }
    let exponent: &[u8] = if hex { b"pP" } else { b"eE" };
    if s.get(end).is_some_and(|c| exponent.contains(c)) {
        let mut exp_start = end + 1;
        if matches!(s.get(exp_start), Some(b'-' | b'+')) {
            exp_start += 1;
        }
        let exp_len = digits_len(exp_start, false);
        if exp_len > 0 {
            end = exp_start + exp_len;
        }
    }
    Some(end)
}

/// The bytes of `s` up to its first zero byte, as C reads a string.
pub fn c_str(s: &[u8]) -> &[u8] {
    s.split(|&c| c == 0).next().unwrap_or_default()
}

/// Converts a string to a number as Lua 5.1's `luaO_str2d` does: with `strtod` (see
/// [`parse_c_double`]) on the string up to its first zero byte, then `strtoul` for hexadecimal if
/// `strtod` stopped at an "x", allowing trailing whitespace.
pub fn lua51_str2number(s: &[u8]) -> Option<f64> {
    let s = c_str(s);
    let (mut n, mut end) = parse_c_double(s)?;
    if matches!(s.get(end), Some(b'x' | b'X')) {
        let (u, e) = parse_c_ulong(s, 16)?;
        n = u as f64;
        end = e;
    }
    s.get(end..)
        .unwrap_or_default()
        .iter()
        .all(|&c| is_space(c))
        .then_some(n)
}

/// Parses the longest prefix of `s` that C's `strtoul` would with the given base (2 to 36),
/// returning the value and the length of the prefix, or `None` if there are no digits.
///
/// Accepts leading whitespace, a sign (a negative number wraps around), and a "0x" prefix in
/// base 16. Values too large are `u64::MAX`.
pub fn parse_c_ulong(s: &[u8], base: u32) -> Option<(u64, usize)> {
    let (mut i, neg) = c_number_start(s);
    if base == 16
        && matches!(s.get(i..i + 2), Some([b'0', b'x' | b'X']))
        && s.get(i + 2).is_some_and(|c| c.is_ascii_hexdigit())
    {
        i += 2;
    }
    let digit = |c: u8| (c as char).to_digit(36).filter(|&d| d < base);
    let len = s
        .get(i..)
        .unwrap_or_default()
        .iter()
        .take_while(|&&c| digit(c).is_some())
        .count();
    if len == 0 {
        return None;
    }
    let mut n: Option<u64> = Some(0);
    for &c in s.get(i..i + len).unwrap_or_default() {
        n = n
            .and_then(|n| n.checked_mul(u64::from(base)))
            .and_then(|n| n.checked_add(u64::from(digit(c).unwrap_or(0))));
    }
    let n = match n {
        // Out of range, even when negated.
        None => u64::MAX,
        Some(n) if neg => n.wrapping_neg(),
        Some(n) => n,
    };
    Some((n, i + len))
}

pub fn read_hex_float(s: &[u8]) -> Option<f64> {
    const MAX_SIGNIFICANT_DIGITS: u32 = 30;

    let (is_neg, s) = read_neg(s);

    let s = strip_hex_prefix(s).filter(|d| !d.is_empty())?;

    let mut significant_digits: u32 = 0;
    let mut non_significant_digits: u32 = 0;
    let mut found_dot = false;
    let mut base: f64 = 0.0;
    let mut exp: i32 = 0;
    let mut i = 0;

    while let Some(&c) = s.get(i) {
        if c == b'.' {
            if found_dot {
                return None;
            }
            found_dot = true;
        } else if let Some(d) = from_hex_digit(c) {
            if significant_digits == 0 && d == 0 {
                non_significant_digits += 1;
            } else if significant_digits < MAX_SIGNIFICANT_DIGITS {
                significant_digits += 1;
                base = (base * 16.0) + d as f64;
            } else {
                // ignore the digit, but count it towards the expontent
                exp = exp.checked_add(4)?;
            }
            if found_dot {
                // Correct exponent for the fractional part
                exp = exp.checked_sub(4)?;
            }
        } else {
            break;
        }
        i += 1;
    }

    if non_significant_digits + significant_digits == 0 {
        return None;
    }

    if let (Some(b'p' | b'P'), Some(exp_s @ [_, ..])) = (s.get(i), s.get(i + 1..)) {
        let (exp_neg, exp_s) = read_neg(exp_s);
        if exp_s.is_empty() {
            return None;
        }
        let mut exp1: i32 = 0;
        for &c in exp_s {
            let d = from_digit(c)?;
            exp1 = exp1.saturating_mul(10).saturating_add(d as i32);
        }
        if exp_neg {
            exp1 = -exp1;
        }
        exp = exp.saturating_add(exp1);
    } else if i != s.len() {
        return None;
    }

    if is_neg {
        base = -base;
    }

    Some(ldexp(base, exp))
}

fn ldexp(mut val: f64, exp: i32) -> f64 {
    fn iexp2(exp: i32) -> f64 {
        debug_assert!((-1022..=1023).contains(&exp));
        f64::from_bits(((exp + 1023) as u64) << 52)
    }
    fn extract_exp(val: f64) -> u64 {
        (val.to_bits() >> 52) & 0x7ff
    }
    fn extract_mantissa(val: f64) -> u64 {
        val.to_bits() & ((1 << 52) - 1)
    }

    let mut orig_exp = extract_exp(val) as i32;
    let mantissa = extract_mantissa(val);
    if orig_exp == 0 {
        if mantissa == 0 {
            return val;
        }
        // input is subnormal
        val *= iexp2(54);
        orig_exp = extract_exp(val) as i32 - 54;
    } else if orig_exp == 2047 {
        // input is NaN of Inf
        return val;
    }

    let new_exp = orig_exp.saturating_add(exp);
    if new_exp >= 2047 {
        f64::copysign(f64::INFINITY, val) // overflow
    } else if new_exp <= -54 {
        f64::copysign(0.0, val) // underflow
    } else if new_exp <= 0 {
        // output is subnormal
        let sign = val.is_sign_negative() as u64;
        f64::from_bits((sign << 63) | (((new_exp + 54) as u64) << 52) | mantissa) * iexp2(-54)
    } else {
        let sign = val.is_sign_negative() as u64;
        f64::from_bits((sign << 63) | ((new_exp as u64) << 52) | mantissa)
    }
}

/// Read an optional '-' or '+' prefix and return whether the value is negated (starts with a '-'
/// prefix).
pub fn read_neg(s: &[u8]) -> (bool, &[u8]) {
    match s.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, s),
    }
}

pub const ALERT_BEEP: u8 = 0x07;
pub const BACKSPACE: u8 = 0x08;
pub const VERTICAL_TAB: u8 = 0x0b;
pub const FORM_FEED: u8 = 0x0c;

pub fn is_newline(c: u8) -> bool {
    c == b'\n' || c == b'\r'
}

pub fn is_space(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == VERTICAL_TAB || c == FORM_FEED || is_newline(c)
}

/// Is this character a Lua alpha, which is A-Z, a-z, and _
pub fn is_alpha(c: u8) -> bool {
    (c >= b'a' && c <= b'z') || (c >= b'A' && c <= b'Z') || c == b'_'
}

pub fn from_digit(c: u8) -> Option<u8> {
    if c >= b'0' && c <= b'9' {
        Some(c - b'0')
    } else {
        None
    }
}

pub fn is_digit(c: u8) -> bool {
    from_digit(c).is_some()
}

pub fn from_hex_digit(c: u8) -> Option<u8> {
    if c >= b'0' && c <= b'9' {
        Some(c - b'0')
    } else if c >= b'a' && c <= b'f' {
        Some(10 + c - b'a')
    } else if c >= b'A' && c <= b'F' {
        Some(10 + c - b'A')
    } else {
        None
    }
}

pub fn is_hex_digit(c: u8) -> bool {
    from_hex_digit(c).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_whitespace_works() {
        assert_eq!(trim_whitespace(b"foo  "), b"foo");
        assert_eq!(trim_whitespace(b"  foo"), b"foo");
        assert_eq!(trim_whitespace(b"  foo  "), b"foo");
        assert_eq!(trim_whitespace(b"   "), b"");
        assert_eq!(trim_whitespace(b""), b"");
        assert_eq!(trim_whitespace(b" . "), b".");
    }
}
