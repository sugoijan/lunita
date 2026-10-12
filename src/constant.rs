use std::{
    cmp::Ordering::{self, Equal, Greater, Less},
    hash::{Hash, Hasher},
};

use gc_arena::Collect;

use crate::compiler::string_utils::{read_float, read_integer, trim_whitespace};

#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum Constant<S> {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(S),
}

impl<S> Constant<S> {
    pub fn to_bool(&self) -> bool {
        match self {
            Self::Nil => false,
            Self::Boolean(false) => false,
            _ => true,
        }
    }

    pub fn not(&self) -> Constant<S> {
        Constant::Boolean(!self.to_bool())
    }

    pub fn as_string_ref(&self) -> Constant<&S> {
        match self {
            Constant::Nil => Constant::Nil,
            Constant::Boolean(b) => Constant::Boolean(*b),
            Constant::Integer(i) => Constant::Integer(*i),
            Constant::Number(n) => Constant::Number(*n),
            Constant::String(s) => Constant::String(s),
        }
    }

    pub fn map_string<S2>(self, f: impl FnOnce(S) -> S2) -> Constant<S2> {
        match self {
            Constant::Nil => Constant::Nil,
            Constant::Boolean(b) => Constant::Boolean(b),
            Constant::Integer(i) => Constant::Integer(i),
            Constant::Number(n) => Constant::Number(n),
            Constant::String(s) => Constant::String(f(s)),
        }
    }
}

impl<S: AsRef<[u8]>> Constant<S> {
    /// Converts the given constant to an integer or number, if possible.
    pub fn to_numeric(&self) -> Option<Constant<S>> {
        match self {
            &Self::Integer(a) => Some(Constant::Integer(a)),
            &Self::Number(a) => Some(Constant::Number(a)),
            Self::String(a) => {
                let a = trim_whitespace(a.as_ref());
                if let Some(i) = read_integer(a) {
                    Some(Constant::Integer(i))
                } else if let Some(n) = read_float(a) {
                    Some(Constant::Number(n))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Interprets Numbers, Integers, and Strings as a Number, if possible.
    pub fn to_number(&self) -> Option<f64> {
        match self.to_numeric() {
            Some(Self::Integer(a)) => Some(a as f64),
            Some(Self::Number(a)) => Some(a),
            _ => None,
        }
    }

    /// Interprets Numbers, Integers, and Strings as an Integer, if possible.
    pub fn to_integer(&self) -> Option<i64> {
        match self.to_numeric() {
            Some(Self::Integer(a)) => Some(a),
            Some(Self::Number(a)) => f64_to_i64(a),
            _ => None,
        }
    }

    // Mathematical operators

    pub fn add(&self, rhs: &Self) -> Option<Self> {
        Some(match (self.to_numeric()?, rhs.to_numeric()?) {
            (Self::Integer(a), Self::Integer(b)) => Self::Integer(a.wrapping_add(b)),
            (a, b) => Self::Number(a.to_number()? + b.to_number()?),
        })
    }

    pub fn subtract(&self, rhs: &Self) -> Option<Self> {
        Some(match (self.to_numeric()?, rhs.to_numeric()?) {
            (Self::Integer(a), Self::Integer(b)) => Self::Integer(a.wrapping_sub(b)),
            (a, b) => Self::Number(a.to_number()? - b.to_number()?),
        })
    }

    pub fn multiply(&self, rhs: &Self) -> Option<Self> {
        Some(match (self.to_numeric()?, rhs.to_numeric()?) {
            (Self::Integer(a), Self::Integer(b)) => Self::Integer(a.wrapping_mul(b)),
            (a, b) => Self::Number(a.to_number()? * b.to_number()?),
        })
    }

    /// This operation always returns a Number, even when called with Integer arguments.
    pub fn float_divide(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Number(self.to_number()? / rhs.to_number()?))
    }

    /// This operation returns an Integer only if both arguments are Integers. Rounding is towards
    /// negative infinity.
    pub fn floor_divide(&self, rhs: &Self) -> Option<Self> {
        match (self.to_numeric()?, rhs.to_numeric()?) {
            (Self::Integer(a), Self::Integer(b)) => {
                if b == 0 {
                    None
                } else {
                    Some(Self::Integer(integer_floor_divide(a, b)))
                }
            }
            (a, b) => Some(Self::Number(float_floor_divide(
                a.to_number()?,
                b.to_number()?,
            ))),
        }
    }

    /// Computes the Lua modulus (`%`) operator. This is unlike Rust's `%` operator which computes
    /// the remainder.
    pub fn modulo(&self, rhs: &Self) -> Option<Self> {
        match (self.to_numeric()?, rhs.to_numeric()?) {
            (Self::Integer(a), Self::Integer(b)) => {
                if b == 0 {
                    None
                } else {
                    Some(Self::Integer(integer_modulo(a, b)))
                }
            }
            (a, b) => Some(Self::Number(float_modulo(a.to_number()?, b.to_number()?))),
        }
    }

    /// This operation always returns a Number, even when called with Integer arguments.
    pub fn exponentiate(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Number(self.to_number()?.powf(rhs.to_number()?)))
    }

    pub fn negate(&self) -> Option<Self> {
        match self.to_numeric()? {
            Self::Integer(a) => Some(Self::Integer(a.wrapping_neg())),
            a => Some(Self::Number(-a.to_number()?)),
        }
    }

    // Bitwise operators

    pub fn bitwise_not(&self) -> Option<Self> {
        Some(Self::Integer(!self.to_integer()?))
    }

    pub fn bitwise_and(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Integer(self.to_integer()? & rhs.to_integer()?))
    }

    pub fn bitwise_or(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Integer(self.to_integer()? | rhs.to_integer()?))
    }

    pub fn bitwise_xor(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Integer(self.to_integer()? ^ rhs.to_integer()?))
    }

    pub fn shift_left(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Integer(shift_left(
            self.to_integer()?,
            rhs.to_integer()?,
        )))
    }

    pub fn shift_right(&self, rhs: &Self) -> Option<Self> {
        Some(Self::Integer(shift_left(
            self.to_integer()?,
            rhs.to_integer()?.wrapping_neg(),
        )))
    }

    // Comparison operators

    pub fn is_equal(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Nil, Self::Nil) => true,
            (Self::Nil, _) => false,

            (Self::Boolean(a), Self::Boolean(b)) => a == b,
            (Self::Boolean(_), _) => false,

            (Self::Integer(a), Self::Integer(b)) => a == b,
            (Self::Integer(a), Self::Number(b)) => compare_integer_float(*a, *b) == Some(Equal),
            (Self::Integer(_), _) => false,

            (Self::Number(a), Self::Number(b)) => a == b,
            (Self::Number(a), Self::Integer(b)) => compare_integer_float(*b, *a) == Some(Equal),
            (Self::Number(_), _) => false,

            (Self::String(a), Self::String(b)) => a.as_ref() == b.as_ref(),
            (Self::String(_), _) => false,
        }
    }

    pub fn less_than(&self, rhs: &Self) -> Option<bool> {
        Some(match (self, rhs) {
            (Self::Integer(a), Self::Integer(b)) => a < b,
            (Self::Integer(a), Self::Number(b)) => compare_integer_float(*a, *b) == Some(Less),
            (Self::Number(a), Self::Number(b)) => a < b,
            (Self::Number(a), Self::Integer(b)) => compare_integer_float(*b, *a) == Some(Greater),
            (Self::String(a), Self::String(b)) => a.as_ref() < b.as_ref(),
            _ => return None,
        })
    }

    pub fn less_equal(&self, rhs: &Self) -> Option<bool> {
        Some(match (self, rhs) {
            (Self::Integer(a), Self::Integer(b)) => a <= b,
            (Self::Integer(a), Self::Number(b)) => {
                matches!(compare_integer_float(*a, *b), Some(Less | Equal))
            }
            (Self::Number(a), Self::Number(b)) => a <= b,
            (Self::Number(a), Self::Integer(b)) => {
                matches!(compare_integer_float(*b, *a), Some(Greater | Equal))
            }
            (Self::String(a), Self::String(b)) => a.as_ref() <= b.as_ref(),
            _ => return None,
        })
    }
}

/// `a << b` as Lua 5.4's `luaV_shiftl`: a negative `b` shifts right (logically), and shifting by
/// 64 bits or more gives 0. `a >> b` is `a << -b`.
pub fn shift_left(a: i64, b: i64) -> i64 {
    if b <= -64 || b >= 64 {
        0
    } else if b < 0 {
        ((a as u64) >> b.unsigned_abs()) as i64
    } else {
        ((a as u64) << b) as i64
    }
}

/// Compares an integer with a float exactly, as Lua 5.4 does (converting the integer to a float
/// would round it above 2^53: `2^53 + 1` is not equal to `2.0^53`, and `math.maxinteger` is less than
/// `2.0^63`). `None` if the float is NaN.
pub fn compare_integer_float(i: i64, f: f64) -> Option<Ordering> {
    // 2^63, the first float above every integer.
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    if f.is_nan() {
        None
    } else if f >= TWO_POW_63 {
        Some(Less)
    } else if f < -TWO_POW_63 {
        Some(Greater)
    } else {
        // `f` truncated is in the range of `i64`, and converts exactly.
        let whole = f.trunc();
        Some(i.cmp(&(whole as i64)).then_with(|| {
            // Equal integral parts: the fraction decides.
            0.0f64.partial_cmp(&(f - whole)).unwrap_or(Equal)
        }))
    }
}

// The largest magnitude up to which every integer is exactly a float, 2^53.
const MAX_EXACT_FLOAT_INTEGER: u64 = 1 << 53;

/// Whether an integer is exactly a float, so that Lua 5.1 (which only has floats) holds the same
/// number.
pub(crate) fn is_exact_float_integer(i: i64) -> bool {
    i.unsigned_abs() <= MAX_EXACT_FLOAT_INTEGER
}

// DELIBERATE(puc): Lua51 mode keeps 64-bit integers, within 2^53 for literals, `+`, `-`, `*` and
// negation; hex literals past 2^63 wrap, the bitwise operators and `math.maxinteger` exist, and
// an integer `for` loop counts past 2^53 exactly (see COMPATIBILITY.md).
/// The result of an integer operation (computed exactly) in Lua 5.1, which only has floats: past
/// 2^53 it is the float the exact result rounds to, as float arithmetic gives it when the operands
/// are exactly floats.
pub(crate) fn lua51_integer_result<S>(r: i128) -> Constant<S> {
    match i64::try_from(r) {
        Ok(i) if is_exact_float_integer(i) => Constant::Integer(i),
        _ => Constant::Number(r as f64),
    }
}

/// Lua integer floor division, `b` must not be zero.
///
/// `i64::MIN // -1` wraps around to `i64::MIN`, as in PUC-Rio Lua.
pub fn integer_floor_divide(a: i64, b: i64) -> i64 {
    let d = a.wrapping_div(b);
    // If the remainder is non-zero and has a different sign than the divisor, round towards
    // negative infinity. `d` cannot be `i64::MIN` here, since the remainder is non-zero.
    if a.wrapping_rem(b) != 0 && (a ^ b) < 0 {
        d.wrapping_sub(1)
    } else {
        d
    }
}

/// Lua integer modulus (the remainder of floor division), `b` must not be zero.
///
/// `i64::MIN % -1` is 0, as in PUC-Rio Lua.
pub fn integer_modulo(a: i64, b: i64) -> i64 {
    let r = a.wrapping_rem(b);
    if r != 0 && (r ^ b) < 0 {
        // `r` and `b` have different signs, so this cannot overflow.
        r.wrapping_add(b)
    } else {
        r
    }
}

/// Lua float floor division.
pub fn float_floor_divide(a: f64, b: f64) -> f64 {
    (a / b).floor()
}

/// Lua float modulus, matching PUC-Rio Lua 5.4's `luai_nummod`.
pub fn float_modulo(a: f64, b: f64) -> f64 {
    let m = a % b;
    if if m > 0.0 { b < 0.0 } else { m < 0.0 && b > 0.0 } {
        m + b
    } else {
        m
    }
}

/// Converts a float to an integer if it has an exact integer representation.
pub fn f64_to_i64(n: f64) -> Option<i64> {
    // `-2^63` is exactly representable as an `f64` and as an `i64`, `2^63` is not representable as
    // an `i64`, and every float in between with no fractional part is an integer.
    const MIN: f64 = i64::MIN as f64;
    if n >= MIN && n < -MIN && n.fract() == 0.0 {
        Some(n as i64)
    } else {
        None
    }
}

impl<S: AsRef<[u8]>> PartialEq for Constant<S> {
    fn eq(&self, other: &Self) -> bool {
        self.is_equal(other)
    }
}

/// Wrapper for a `Constant` that implements Hash and Eq, and only compares equal when the types are
/// bit for bit identical.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct IdenticalConstant<S>(pub Constant<S>);

impl<S> From<Constant<S>> for IdenticalConstant<S> {
    fn from(value: Constant<S>) -> Self {
        Self(value)
    }
}

impl<S: AsRef<[u8]>> PartialEq for IdenticalConstant<S> {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Constant::Nil, Constant::Nil) => true,
            (Constant::Nil, _) => false,

            (Constant::Boolean(a), Constant::Boolean(b)) => a == b,
            (Constant::Boolean(_), _) => false,

            (Constant::Integer(a), Constant::Integer(b)) => a == b,
            (Constant::Integer(_), _) => false,

            (Constant::Number(a), Constant::Number(b)) => a.to_bits() == b.to_bits(),
            (Constant::Number(_), _) => false,

            (Constant::String(a), Constant::String(b)) => a.as_ref() == b.as_ref(),
            (Constant::String(_), _) => false,
        }
    }
}

impl<S: AsRef<[u8]>> Eq for IdenticalConstant<S> {}

impl<S: AsRef<[u8]>> Hash for IdenticalConstant<S> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match &self.0 {
            Constant::Nil => {
                Hash::hash(&0, state);
            }
            Constant::Boolean(b) => {
                Hash::hash(&1, state);
                b.hash(state);
            }
            Constant::Integer(i) => {
                Hash::hash(&2, state);
                i.hash(state);
            }
            Constant::Number(n) => {
                Hash::hash(&3, state);
                n.to_bits().hash(state);
            }
            Constant::String(s) => {
                Hash::hash(&4, state);
                s.as_ref().hash(state);
            }
        }
    }
}
