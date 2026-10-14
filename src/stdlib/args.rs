//! Argument checks and conversions of the standard library, following PUC-Rio Lua's
//! `luaL_check*` functions (and Lua 5.1's with [`Compat::Lua51`](crate::Compat::Lua51)).

use std::fmt;

use crate::{
    Compat, Context, Error, RuntimeError, Stack, String, Table, Value,
    compiler::string_utils::lua51_str2number,
    constant::f64_to_i64,
    error::LibraryError,
    fuel::{cost, fuel_for},
};

pub(crate) type Result<'gc, T> = std::result::Result<T, Error<'gc>>;

pub(crate) fn is_lua51(ctx: Context<'_>) -> bool {
    ctx.compat() == Compat::Lua51
}

/// An error with a string message.
///
/// When the library function returns it, the executor adds the position of the Lua code calling
/// the function, as `luaL_error` does.
pub(crate) fn error<'gc>(msg: impl fmt::Display) -> Error<'gc> {
    error_bytes(msg.to_string().into_bytes())
}

/// Like [`error`], with a message of any bytes.
pub(crate) fn error_bytes<'gc>(msg: Vec<u8>) -> Error<'gc> {
    Error::Runtime(RuntimeError::new(LibraryError(msg)))
}

// DELIBERATE(puc): names the function as registered ('floor'; 5.4 says 'math.floor', and PUC '?'
// from `pcall` or a metamethod) and counts arguments from `self` in method calls, where PUC
// subtracts one (see COMPATIBILITY.md).
/// "bad argument #i to 'name' (msg)", for the 0-based argument `i`.
pub(crate) fn arg_error<'gc>(name: &str, i: usize, msg: impl fmt::Display) -> Error<'gc> {
    error(format!("bad argument #{} to '{}' ({})", i + 1, name, msg))
}

// Converting a string argument to a number takes time proportional to its length.
fn charge_conversion<'gc>(ctx: Context<'gc>, value: Value<'gc>) {
    if let Value::String(s) = value {
        ctx.charge_fuel(fuel_for(s.as_bytes().len(), cost::BYTES_PROCESSED));
    }
}

/// `luaL_checkany`: the 0-based argument `i`, which may be nil but must be present.
pub(crate) fn check_any<'gc>(
    stack: &Stack<'gc, '_>,
    name: &str,
    i: usize,
) -> Result<'gc, Value<'gc>> {
    if i < stack.len() {
        Ok(stack.get(i))
    } else {
        Err(arg_error(name, i, "value expected"))
    }
}

pub(crate) fn type_error<'gc>(
    stack: &Stack<'gc, '_>,
    name: &str,
    i: usize,
    expected: &str,
) -> Error<'gc> {
    let found = if i >= stack.len() {
        "no value"
    } else {
        stack.get(i).type_name()
    };
    arg_error(name, i, format!("{expected} expected, got {found}"))
}

/// A table argument.
pub(crate) fn check_table<'gc>(
    stack: &Stack<'gc, '_>,
    name: &str,
    i: usize,
) -> Result<'gc, Table<'gc>> {
    match stack.get(i) {
        Value::Table(t) => Ok(t),
        _ => Err(type_error(stack, name, i, "table")),
    }
}

/// A string argument, numbers being converted to strings.
pub(crate) fn check_string<'gc>(
    ctx: Context<'gc>,
    stack: &Stack<'gc, '_>,
    name: &str,
    i: usize,
) -> Result<'gc, String<'gc>> {
    match stack.get(i) {
        Value::String(s) => Ok(s),
        v @ (Value::Integer(_) | Value::Number(_)) => v
            .into_string(ctx)
            .ok_or_else(|| type_error(stack, name, i, "string")),
        _ => Err(type_error(stack, name, i, "string")),
    }
}

/// An integer argument: Lua 5.4 requires numbers (or strings converting to numbers) with an
/// integer representation, Lua 5.1 truncates them.
pub(crate) fn check_integer<'gc>(
    ctx: Context<'gc>,
    stack: &Stack<'gc, '_>,
    name: &str,
    i: usize,
) -> Result<'gc, i64> {
    let value = stack.get(i);
    charge_conversion(ctx, value);
    if is_lua51(ctx) {
        let n = match value {
            Value::Integer(n) => return Ok(n),
            Value::Number(n) => Some(n),
            Value::String(s) => lua51_str2number(s.as_bytes()),
            _ => None,
        };
        return n
            .map(|n| n as i64)
            .ok_or_else(|| type_error(stack, name, i, "number"));
    }
    match value.to_numeric() {
        Some(Value::Integer(n)) => Ok(n),
        Some(Value::Number(n)) => {
            f64_to_i64(n).ok_or_else(|| arg_error(name, i, "number has no integer representation"))
        }
        _ => Err(type_error(stack, name, i, "number")),
    }
}

/// An optional integer argument.
pub(crate) fn opt_integer<'gc>(
    ctx: Context<'gc>,
    stack: &Stack<'gc, '_>,
    name: &str,
    i: usize,
    default: i64,
) -> Result<'gc, i64> {
    if stack.get(i).is_nil() {
        Ok(default)
    } else {
        check_integer(ctx, stack, name, i)
    }
}

/// An `int` argument of Lua 5.1 (`luaL_checkint`), truncated to 32 bits, or an integer argument.
pub(crate) fn check_int<'gc>(
    ctx: Context<'gc>,
    stack: &Stack<'gc, '_>,
    name: &str,
    i: usize,
) -> Result<'gc, i64> {
    let n = check_integer(ctx, stack, name, i)?;
    Ok(if is_lua51(ctx) {
        i64::from(n as i32)
    } else {
        n
    })
}

/// An optional [`check_int`] argument.
pub(crate) fn opt_int<'gc>(
    ctx: Context<'gc>,
    stack: &Stack<'gc, '_>,
    name: &str,
    i: usize,
) -> Result<'gc, Option<i64>> {
    if stack.get(i).is_nil() {
        Ok(None)
    } else {
        check_int(ctx, stack, name, i).map(Some)
    }
}

/// A number argument, strings being converted to numbers.
pub(crate) fn check_number<'gc>(
    ctx: Context<'gc>,
    stack: &Stack<'gc, '_>,
    name: &str,
    i: usize,
) -> Result<'gc, f64> {
    let value = stack.get(i);
    charge_conversion(ctx, value);
    let n = match value {
        Value::String(s) if is_lua51(ctx) => lua51_str2number(s.as_bytes()),
        v => v.to_number(),
    };
    n.ok_or_else(|| type_error(stack, name, i, "number"))
}
