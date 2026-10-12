//! Behaviour that differs between Lua versions.

use std::{cell::Cell, io::Write as _};

use gc_arena::Collect;

/// Which Lua version to follow where versions differ, see [`Lua::set_compat`](crate::Lua::set_compat).
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Hash)]
pub enum Compat {
    /// piccolo's own behaviour, which mostly follows Lua 5.4.
    #[default]
    Default,
    /// Behave like PUC-Rio Lua 5.1.5 where versions differ. Set with
    /// [`Lua::set_compat`](crate::Lua::set_compat) before loading the standard library (`_VERSION`
    /// is set then). Tested byte for byte against PUC 5.1.5 by `tests/lua51/*.lua`;
    /// `COMPATIBILITY.md` lists what still differs.
    ///
    /// - Numbers convert to strings with `%.14g` everywhere (`tostring`, `..`, `print`,
    ///   coercions), integers as the floats 5.1 would hold; `string.format`'s `%e %f %g` exactly
    ///   as C's printf.
    /// - `%` and `//` by zero give the float result (`1 % 0` is nan); float `%` is
    ///   `a - floor(a / b) * b`; `-0` results of negation and multiplication; integer `+`, `-`,
    ///   `*`, negation and literals past 2^53 are the floats 5.1 rounds them to.
    /// - Number parsing as `strtod`/`strtoul` (`"inf"`, `"nan"`, trailing NUL, bases), base 10
    ///   as no base; `tonumber`'s base is truncated.
    /// - `math.floor`/`ceil` of a float return a float; `math.modf` as C's `modf`; `math.log` and
    ///   `math.atan` ignore their second argument; `math.min`/`max` take numbers only;
    ///   `math.random(0)` is an empty interval; `math.deg` divides by the radians per degree;
    ///   `math.randomseed` takes one seed and returns nothing.
    /// - String library: 5.1's `format` (flags, two-digit width and precision, truncating integer
    ///   conversions, `%q`), no `%g` class, `%z`, patterns ending at NUL, `find`'s clamped init,
    ///   `gmatch` without init, 5.1's empty matches, `gsub` copying `%x` as `x`, no separator for
    ///   `rep`, `int` arguments truncated to 32 bits.
    /// - `error` converts number messages to strings with a position; `assert` takes string or
    ///   number messages and positions them, up to their first zero byte; `xpcall` without extra
    ///   arguments, with any handler value; `__index` and `__newindex` chains end with "loop in
    ///   gettable"/"loop in settable" after 100 links; chunk names starting with `@` keep their
    ///   last 52 bytes.
    /// - Metamethods: `#t` ignores a table's `__len` (a userdata's gets nil as its second
    ///   argument); `pairs` has no `__pairs` and `ipairs` reads raw; `__eq`, `__lt` and `__le`
    ///   need the same metamethod on both operands, of one type (`<=` falls back to a shared
    ///   `__lt`); `__call` must be a function.
    /// - `coroutine.create` takes a Lua function; `coroutine.running` returns the thread alone
    ///   (nil in the main one); a `for` loop starts from `init - step + step`, runs no iteration
    ///   with a NaN or infinite step, and never ends with a zero step whose start is at or past
    ///   the limit; a thread holds at most 20,000 frames (`LUAI_MAXCALLS`).
    /// - `table.insert`/`remove`/`concat` with raw access and the raw length, growth past the
    ///   end, `remove` outside `1..#t` returning nothing, `concat` erroring on non-string values.
    Lua51,
}

#[derive(Debug, Default, Collect)]
#[collect(require_static)]
pub(crate) struct Settings {
    compat: Cell<Compat>,
}

impl Settings {
    pub(crate) fn compat(&self) -> Compat {
        self.compat.get()
    }

    pub(crate) fn set_compat(&self, compat: Compat) {
        self.compat.set(compat);
    }
}

/// Appends the string form of a float, as `tostring` converts it.
pub fn write_float(buf: &mut Vec<u8>, compat: Compat, n: f64) {
    let start = buf.len();
    format_float(buf, n, FloatFormat::G, Some(14), false, false);
    // Lua 5.4 marks a float that looks like an integer with ".0".
    if compat == Compat::Default
        && buf
            .get(start..)
            .unwrap_or_default()
            .iter()
            .all(|c| matches!(c, b'-' | b'0'..=b'9'))
    {
        buf.extend_from_slice(b".0");
    }
}

/// Appends the string form of an integer, as `tostring` converts it.
pub fn write_integer(buf: &mut Vec<u8>, compat: Compat, i: i64) {
    match compat {
        Compat::Default => {
            let _ = write!(buf, "{i}");
        }
        // Lua 5.1 only has floats, which print exactly up to 14 digits.
        Compat::Lua51 => {
            if i.unsigned_abs() < 100_000_000_000_000 {
                let _ = write!(buf, "{i}");
            } else {
                format_float(buf, i as f64, FloatFormat::G, Some(14), false, false);
            }
        }
    }
}

/// A floating point conversion of C's `printf`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FloatFormat {
    /// `%e`
    E,
    /// `%f`
    F,
    /// `%g`
    G,
}

/// Appends a float formatted like C's `printf` with the conversion `format`, the given precision
/// (defaulting to 6), the `#` flag (`alternate`) and in upper case (`%E`, `%F`, `%G`) if `upper`.
///
/// Infinity and NaN are `inf`, `-inf` and `nan` (`-nan` if the sign bit is set), as glibc prints
/// them (Apple's libc prints `nan` whatever the sign). Width, sign and padding flags are left to
/// the caller.
pub fn format_float(
    buf: &mut Vec<u8>,
    n: f64,
    format: FloatFormat,
    precision: Option<usize>,
    alternate: bool,
    upper: bool,
) {
    let start = buf.len();
    format_float_lower(buf, n, format, precision, alternate);
    if upper {
        if let Some(written) = buf.get_mut(start..) {
            written.make_ascii_uppercase();
        }
    }
}

fn format_float_lower(
    buf: &mut Vec<u8>,
    n: f64,
    format: FloatFormat,
    precision: Option<usize>,
    alternate: bool,
) {
    // Writing to a `Vec` never fails.
    if n.is_nan() {
        buf.extend_from_slice(if n.is_sign_negative() {
            b"-nan"
        } else {
            b"nan"
        });
        return;
    }
    if n.is_infinite() {
        buf.extend_from_slice(if n < 0.0 { b"-inf" } else { b"inf" });
        return;
    }

    let precision = precision.unwrap_or(6);
    match format {
        FloatFormat::F => {
            let _ = write!(buf, "{n:.precision$}");
            if alternate && precision == 0 {
                buf.push(b'.');
            }
        }
        FloatFormat::E => {
            let e = format!("{n:.precision$e}");
            push_exponential(buf, &e, alternate && precision == 0);
        }
        FloatFormat::G => {
            let precision = precision.max(1);
            // `n` with `precision` significant digits, and its exponent.
            let decimals = precision - 1;
            let e = format!("{n:.decimals$e}");
            let exponent = e
                .rsplit('e')
                .next()
                .and_then(|e| e.parse().ok())
                .unwrap_or(0);
            let start = buf.len();
            if exponent < -4 || exponent >= precision as i32 {
                push_exponential(buf, &e, alternate && decimals == 0);
                if !alternate {
                    // Remove trailing zeros (and the point) from the mantissa.
                    let e = buf.iter().rposition(|&c| c == b'e').unwrap_or(buf.len());
                    let mantissa_end = trim_fraction(buf.get(start..e).unwrap_or_default());
                    buf.drain(start + mantissa_end..e);
                }
            } else {
                let decimals = (precision as i32 - 1 - exponent).max(0) as usize;
                let _ = write!(buf, "{n:.decimals$}");
                if !alternate {
                    let end = trim_fraction(buf.get(start..).unwrap_or_default());
                    buf.truncate(start + end);
                } else if decimals == 0 {
                    buf.push(b'.');
                }
            }
        }
    }
}

// Writes a number formatted by Rust's `{:e}` as C's `%e` writes it (with a sign and at least two
// digits in the exponent), with a point after the mantissa if `point`.
fn push_exponential(buf: &mut Vec<u8>, s: &str, point: bool) {
    let (mantissa, exponent) = s.split_once('e').unwrap_or((s, "0"));
    buf.extend_from_slice(mantissa.as_bytes());
    if point {
        buf.push(b'.');
    }
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let _ = write!(
        buf,
        "e{}{:02}",
        if exponent < 0 { '-' } else { '+' },
        exponent.unsigned_abs()
    );
}

// The length of a decimal number without the trailing zeros of its fraction (and without the
// point if nothing is left after it).
fn trim_fraction(s: &[u8]) -> usize {
    if !s.contains(&b'.') {
        return s.len();
    }
    let mut end = s.len();
    while end > 0 && s.get(end - 1) == Some(&b'0') {
        end -= 1;
    }
    if end > 0 && s.get(end - 1) == Some(&b'.') {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g14(n: f64) -> std::string::String {
        let mut buf = Vec::new();
        format_float(&mut buf, n, FloatFormat::G, Some(14), false, false);
        std::string::String::from_utf8(buf).unwrap()
    }

    #[test]
    fn test_g14() {
        assert_eq!(g14(1.0 / 3.0), "0.33333333333333");
        assert_eq!(g14(1e15), "1e+15");
        assert_eq!(g14(2f64.powi(53)), "9.007199254741e+15");
        assert_eq!(g14(-0.0), "-0");
        assert_eq!(g14(100.0), "100");
        assert_eq!(g14(0.1), "0.1");
        assert_eq!(g14(1e-5), "1e-05");
        assert_eq!(g14(1e-4), "0.0001");
        assert_eq!(g14(1e14), "1e+14");
        assert_eq!(g14(123456789012345.0), "1.2345678901234e+14");
        assert_eq!(g14(1234567890123456.0), "1.2345678901235e+15");
        assert_eq!(g14(2f64.powi(63)), "9.2233720368548e+18");
        assert_eq!(g14(5e-324), "4.9406564584125e-324");
        assert_eq!(g14(f64::INFINITY), "inf");
        assert_eq!(g14(f64::NEG_INFINITY), "-inf");
        assert_eq!(g14(f64::NAN), "nan");
        assert_eq!(g14(99999999999999.5), "1e+14");
    }
}
