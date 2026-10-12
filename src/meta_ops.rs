use std::{cmp::Ordering, pin::Pin};

use gc_arena::Collect;
use thiserror::Error;

use crate::async_callback::{AsyncSequence, Locals};
use crate::{
    async_sequence, BoxSequence, Execution, Sequence, SequencePoll, SequenceReturn, Stack,
};
use crate::{
    compat::{self, Compat},
    constant::{compare_integer_float, float_floor_divide, lua51_integer_result},
    fuel::{cost, fuel_for},
    table::{InvalidTableKey, TableError},
    Callback, CallbackReturn, Constant, Context, Function, IntoValue, LimitError, Table, Value,
};

/// An enum of every possible Lua metamethod.
///
/// The [`MetaMethod::name`] method will return the name that Lua expects to be the key
/// for the metamethod in a metatable. For example, `MetaMethod::Add.name()` is `"__add"`,
/// `MetaMethod::Sub.name()` is `"__sub"`, etc.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Collect)]
#[collect(require_static)]
pub enum MetaMethod {
    Len,
    Index,
    NewIndex,
    Call,
    Pairs,
    ToString,
    Eq,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Unm,
    IDiv,
    BAnd,
    BOr,
    BXor,
    BNot,
    Shl,
    Shr,
    Concat,
    Lt,
    Le,
}

impl MetaMethod {
    pub const fn name(self) -> &'static str {
        match self {
            MetaMethod::Len => "__len",
            MetaMethod::Index => "__index",
            MetaMethod::NewIndex => "__newindex",
            MetaMethod::Call => "__call",
            MetaMethod::Pairs => "__pairs",
            MetaMethod::ToString => "__tostring",
            MetaMethod::Eq => "__eq",
            MetaMethod::Add => "__add",
            MetaMethod::Sub => "__sub",
            MetaMethod::Mul => "__mul",
            MetaMethod::Div => "__div",
            MetaMethod::Mod => "__mod",
            MetaMethod::Pow => "__pow",
            MetaMethod::Unm => "__unm",
            MetaMethod::IDiv => "__idiv",
            MetaMethod::BAnd => "__band",
            MetaMethod::BOr => "__bor",
            MetaMethod::BXor => "__bxor",
            MetaMethod::BNot => "__bnot",
            MetaMethod::Shl => "__shl",
            MetaMethod::Shr => "__shr",
            MetaMethod::Concat => "__concat",
            MetaMethod::Lt => "__lt",
            MetaMethod::Le => "__le",
        }
    }

    /// Sentence-form verb of this metamethod's action
    ///
    /// - unary: "Could not {verb} a {type} value"
    /// - index: "Could not {verb} a {type} value"
    /// - binary: "Could not {verb} values of type {lhs_type} and {rhs_type}"
    pub const fn verb(self) -> &'static str {
        match self {
            MetaMethod::Len => "determine length of",
            MetaMethod::Call => "call",
            MetaMethod::Pairs => "get pairs of",
            MetaMethod::ToString => "convert to string", // a bit awkward, but works
            MetaMethod::Index => "index into",
            MetaMethod::NewIndex => "index-assign into",
            MetaMethod::Eq => "compare equality of",
            MetaMethod::Add => "add",
            MetaMethod::Sub => "subtract",
            MetaMethod::Mul => "multiply",
            MetaMethod::Div => "divide",
            MetaMethod::Mod => "take modulus of",
            MetaMethod::Pow => "exponentiate",
            MetaMethod::Unm => "negate",
            MetaMethod::IDiv => "flooring divide",
            MetaMethod::BAnd => "binary and",
            MetaMethod::BOr => "binary or",
            MetaMethod::BXor => "binary xor",
            MetaMethod::BNot => "binary negate",
            MetaMethod::Shl => "left shift",
            MetaMethod::Shr => "right shift",
            MetaMethod::Concat => "concatenate",
            MetaMethod::Lt => "compare",
            MetaMethod::Le => "compare",
        }
    }
}

impl<'gc> IntoValue<'gc> for MetaMethod {
    fn into_value(self, ctx: Context<'gc>) -> Value<'gc> {
        self.name().into_value(ctx)
    }
}

/// If invoking a metamethod must call Lua code, this will contain a function and arguments to call
/// to trigger it.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct MetaCall<'gc, const N: usize> {
    pub function: Function<'gc>,
    pub args: [Value<'gc>; N],
}

/// Return value for metamethods that return a value *or* require calling into Lua.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum MetaResult<'gc, const N: usize> {
    Value(Value<'gc>),
    Call(MetaCall<'gc, N>),
}

impl<'gc, const N: usize> From<Value<'gc>> for MetaResult<'gc, N> {
    fn from(value: Value<'gc>) -> Self {
        Self::Value(value)
    }
}

impl<'gc, const N: usize> From<MetaCall<'gc, N>> for MetaResult<'gc, N> {
    fn from(call: MetaCall<'gc, N>) -> Self {
        MetaResult::Call(call)
    }
}

#[derive(Debug, Clone, Error)]
// DELIBERATE(puc): the VM's runtime errors (indexing nil, arithmetic on bad types, calling a
// non-function, ...) are piccolo's `RuntimeError`s, without a position or PUC's wording; the
// maintainer left positioned strings for later (see COMPATIBILITY.md).
pub enum MetaOperatorError {
    #[error("could not call metamethod {}: {}", .0.name(), .1)]
    Call(MetaMethod, #[source] MetaCallError),
    #[error("could not {} a {} value", .0.verb(), .1)]
    Unary(MetaMethod, &'static str),
    #[error("could not {} values of type {} and {}", .0.verb(), .1, .2)]
    Binary(MetaMethod, &'static str, &'static str),
    #[error("invalid table key")]
    IndexKeyError(#[from] InvalidTableKey),
    #[error(transparent)]
    Limit(#[from] LimitError),
    #[error("concatenation result is too long")]
    ConcatOverflow,
    #[error("attempt to perform 'n%0'")]
    ModuloByZero,
    #[error("attempt to perform 'n//0'")]
    FloorDivideByZero,
    #[error("'{}' chain too long; possible loop", .0.name())]
    ChainTooLong(MetaMethod),
    /// [`MetaOperatorError::ChainTooLong`] in Lua 5.1.
    #[error("loop in {}", if matches!(.0, MetaMethod::Index) { "gettable" } else { "settable" })]
    Loop(MetaMethod),
}

impl From<TableError> for MetaOperatorError {
    fn from(err: TableError) -> Self {
        match err {
            TableError::InvalidKey(err) => MetaOperatorError::IndexKeyError(err),
            TableError::Limit(err) => MetaOperatorError::Limit(err),
        }
    }
}

#[derive(Debug, Copy, Clone, Error)]
#[error("could not call a {} value", .0)]
pub struct MetaCallError(&'static str);

fn get_metatable<'gc>(val: Value<'gc>) -> Option<Table<'gc>> {
    match val {
        Value::Table(t) => t.metatable(),
        Value::UserData(u) => u.metatable(),
        _ => None,
    }
}

fn get_metamethod<'gc>(
    ctx: Context<'gc>,
    val: Value<'gc>,
    method: MetaMethod,
) -> Option<Value<'gc>> {
    get_metatable(val)
        .map(|mt| mt.get_value(ctx, method))
        .filter(|v| !v.is_nil())
}

// The longest chain of `__index` or `__newindex` values that are not functions, as PUC-Rio Lua's
// `MAXTAGLOOP` (which differs between versions).
fn max_chain(ctx: Context<'_>) -> usize {
    if ctx.compat() == Compat::Lua51 {
        100
    } else {
        2000
    }
}

// Charges following `hops` links of a metamethod chain.
fn charge_chain(ctx: Context<'_>, hops: usize) {
    if hops > 0 {
        ctx.charge_fuel(fuel_for(hops, cost::TABLE_LOOKUPS));
    }
}

fn chain_error(ctx: Context<'_>, method: MetaMethod) -> MetaOperatorError {
    if ctx.compat() == Compat::Lua51 {
        MetaOperatorError::Loop(method)
    } else {
        MetaOperatorError::ChainTooLong(method)
    }
}

/// Indexes `table` with `key` as Lua does: following `__index` values that are not functions (up to
/// PUC-Rio Lua's limit), and returning a call for an `__index` function.
pub fn index<'gc>(
    ctx: Context<'gc>,
    table: Value<'gc>,
    key: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    let mut target = table;
    // Lua 5.4 reads the table itself before counting `MAXTAGLOOP` links, Lua 5.1 counts it.
    let max_hops = max_chain(ctx) + usize::from(ctx.compat() != Compat::Lua51);
    for hops in 0..max_hops {
        let idx = match target {
            Value::Table(table) => {
                let v = table.get_value(ctx, key);
                let idx = match table.metatable() {
                    Some(mt) if v.is_nil() => mt.get_value(ctx, MetaMethod::Index),
                    _ => Value::Nil,
                };
                if idx.is_nil() {
                    charge_chain(ctx, hops);
                    return Ok(MetaResult::Value(v));
                }
                idx
            }
            Value::UserData(u) => u
                .metatable()
                .map_or(Value::Nil, |mt| mt.get_value(ctx, MetaMethod::Index)),
            // All strings share a metatable.
            Value::String(_) => ctx
                .string_metatable()
                .map_or(Value::Nil, |mt| mt.get_value(ctx, MetaMethod::Index)),
            _ => Value::Nil,
        };
        match idx {
            Value::Nil => {
                charge_chain(ctx, hops);
                return Err(MetaOperatorError::Unary(
                    MetaMethod::Index,
                    target.type_name(),
                ));
            }
            Value::Function(function) => {
                charge_chain(ctx, hops);
                return Ok(MetaResult::Call(MetaCall {
                    function,
                    args: [target, key],
                }));
            }
            idx => target = idx,
        }
    }
    charge_chain(ctx, max_hops);
    Err(chain_error(ctx, MetaMethod::Index))
}

/// Sets `key` in `table` to `value` as Lua does: following `__newindex` values that are not
/// functions (up to PUC-Rio Lua's limit), and returning a call for a `__newindex` function.
pub fn new_index<'gc>(
    ctx: Context<'gc>,
    table: Value<'gc>,
    key: Value<'gc>,
    value: Value<'gc>,
) -> Result<Option<MetaCall<'gc, 3>>, MetaOperatorError> {
    let mut target = table;
    for hops in 0..max_chain(ctx) {
        let idx = match target {
            Value::Table(table) => {
                // The metamethod is only for keys not present in the table.
                let idx = match table.metatable() {
                    Some(mt) if table.get_value(ctx, key).is_nil() => {
                        mt.get_value(ctx, MetaMethod::NewIndex)
                    }
                    _ => Value::Nil,
                };
                if idx.is_nil() {
                    charge_chain(ctx, hops);
                    table.set(ctx, key, value)?;
                    return Ok(None);
                }
                idx
            }
            Value::UserData(u) => u
                .metatable()
                .map_or(Value::Nil, |mt| mt.get_value(ctx, MetaMethod::NewIndex)),
            _ => Value::Nil,
        };
        match idx {
            Value::Nil => {
                charge_chain(ctx, hops);
                return Err(MetaOperatorError::Unary(
                    MetaMethod::NewIndex,
                    target.type_name(),
                ));
            }
            Value::Function(function) => {
                charge_chain(ctx, hops);
                return Ok(Some(MetaCall {
                    function,
                    args: [target, key, value],
                }));
            }
            idx => target = idx,
        }
    }
    charge_chain(ctx, max_chain(ctx));
    Err(chain_error(ctx, MetaMethod::NewIndex))
}

pub fn call<'gc>(ctx: Context<'gc>, v: Value<'gc>) -> Result<Function<'gc>, MetaCallError> {
    let metatable = match v {
        Value::Function(f) => return Ok(f),
        Value::Table(t) => t.metatable(),
        Value::UserData(ud) => ud.metatable(),
        _ => None,
    }
    .ok_or(MetaCallError(v.type_name()))?;

    match metatable.get_value(ctx, MetaMethod::Call) {
        // Lua 5.1 only calls a `__call` that is a function.
        f @ (Value::Table(_) | Value::UserData(_)) if ctx.compat() == Compat::Lua51 => {
            Err(MetaCallError(f.type_name()))
        }
        f @ (Value::Function(_) | Value::Table(_) | Value::UserData(_)) => Ok(
            // A chain of `__call` values is followed one call at a time, bounded only by fuel
            // and the frame limit (unlike `__index`, which PUC-Rio Lua bounds too).
            //
            // Example: `t = {}; setmetatable(t, { __call = t }); t()`
            Callback::from_fn_with(&ctx, (v, f), |&(v, f), ctx, _, mut stack| {
                // Every hop of a chain moves all the arguments to put the callee first.
                ctx.charge_fuel(fuel_for(stack.len(), cost::VALUES));
                stack.push_front(v);
                Ok(CallbackReturn::Call {
                    function: call(ctx, f)?,
                    then: None,
                })
            })
            .into(),
        ),
        f => Err(MetaCallError(f.type_name())),
    }
}

pub fn len<'gc>(ctx: Context<'gc>, v: Value<'gc>) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    if let Some(metatable) = match v {
        // Lua 5.1 only consults `__len` for userdata.
        Value::Table(t) if ctx.compat() != Compat::Lua51 => t.metatable(),
        Value::UserData(u) => u.metatable(),
        _ => None,
    } {
        let len = metatable.get_value(ctx, MetaMethod::Len);
        if !len.is_nil() {
            return Ok(MetaResult::Call(MetaCall {
                function: call(ctx, len)
                    .map_err(|e| MetaOperatorError::Call(MetaMethod::Len, e))?,
                // Lua 5.4 passes the operand twice to a unary metamethod, Lua 5.1 calls
                // `__len` with nil as the second argument.
                args: [
                    v,
                    if ctx.compat() == Compat::Lua51 {
                        Value::Nil
                    } else {
                        v
                    },
                ],
            }));
        }
    }

    match v {
        Value::String(s) => Ok(MetaResult::Value(s.len().into())),
        Value::Table(t) => Ok(MetaResult::Value(t.length_charged(ctx).into())),
        f => Err(MetaOperatorError::Unary(MetaMethod::Len, f.type_name())),
    }
}

pub fn tostring<'gc>(
    ctx: Context<'gc>,
    v: Value<'gc>,
) -> Result<MetaResult<'gc, 1>, MetaOperatorError> {
    if let Some(metatable) = match v {
        Value::Table(t) => t.metatable(),
        Value::UserData(u) => u.metatable(),
        _ => None,
    } {
        let tostring = metatable.get_value(ctx, MetaMethod::ToString);
        if !tostring.is_nil() {
            return Ok(MetaResult::Call(MetaCall {
                function: call(ctx, tostring)
                    .map_err(|e| MetaOperatorError::Call(MetaMethod::ToString, e))?,
                args: [v],
            }));
        }
    }

    Ok(match v {
        v @ (Value::String(_) | Value::Integer(_) | Value::Number(_)) => {
            MetaResult::Value(v.into_string(ctx).map_or(Value::Nil, Value::String))
        }
        v => MetaResult::Value(ctx.intern(v.display().to_string().as_bytes()).into()),
    })
}

/// Equality without metamethods, as `rawequal` compares.
pub fn raw_equal<'gc>(lhs: Value<'gc>, rhs: Value<'gc>) -> bool {
    match (lhs, rhs) {
        (Value::Nil, Value::Nil) => true,
        (Value::Boolean(a), Value::Boolean(b)) => a == b,
        (Value::Integer(a), Value::Integer(b)) => a == b,
        (Value::Integer(a), Value::Number(b)) | (Value::Number(b), Value::Integer(a)) => {
            compare_integer_float(a, b) == Some(Ordering::Equal)
        }
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::String(a), Value::String(b)) => a.eq_string(b),
        (Value::Function(a), Value::Function(b)) => a == b,
        (Value::Thread(a), Value::Thread(b)) => a == b,
        (Value::Table(a), Value::Table(b)) => a == b,
        (Value::UserData(a), Value::UserData(b)) => a == b,
        _ => false,
    }
}

pub fn equal<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    Ok(match (lhs, rhs) {
        (Value::Table(_), Value::Table(_)) | (Value::UserData(_), Value::UserData(_))
            if !raw_equal(lhs, rhs) =>
        {
            let metamethod = if ctx.compat() == Compat::Lua51 {
                // Lua 5.1 calls `__eq` only when both operands have the same one.
                match (
                    get_metamethod(ctx, lhs, MetaMethod::Eq),
                    get_metamethod(ctx, rhs, MetaMethod::Eq),
                ) {
                    (Some(a), Some(b)) if raw_equal(a, b) => Some(a),
                    _ => None,
                }
            } else {
                get_metamethod(ctx, lhs, MetaMethod::Eq)
                    .or_else(|| get_metamethod(ctx, rhs, MetaMethod::Eq))
            };
            if let Some(m) = metamethod {
                MetaResult::Call(MetaCall {
                    function: call(ctx, m)
                        .map_err(|e| MetaOperatorError::Call(MetaMethod::Eq, e))?,
                    args: [lhs, rhs],
                })
            } else {
                Value::Boolean(false).into()
            }
        }
        _ => Value::Boolean(raw_equal(lhs, rhs)).into(),
    })
}

// With `Compat::Lua51`, converts a string operand of an arithmetic operation to a number as Lua 5.1
// does (which differs from Lua 5.4 for "inf", "nan" and hexadecimal floats).
fn coerce_lua51<'gc>(ctx: Context<'gc>, value: Value<'gc>) -> Value<'gc> {
    match value {
        Value::String(s) if ctx.compat() == Compat::Lua51 => {
            crate::compiler::string_utils::lua51_str2number(s.as_bytes())
                .map_or(value, Value::Number)
        }
        value => value,
    }
}

// Charges fuel for processing the strings among `values`.
pub(crate) fn charge_strings<'gc>(ctx: Context<'gc>, values: &[Value<'gc>]) {
    let len = values
        .iter()
        .map(|v| match v {
            Value::String(s) => s.as_bytes().len(),
            _ => 0,
        })
        .fold(0usize, usize::saturating_add);
    if len > 0 {
        ctx.charge_fuel(fuel_for(len, cost::BYTES_PROCESSED));
    }
}

fn meta_metaop<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
    method: MetaMethod,
    const_op: impl Fn(Context<'gc>, Value<'gc>, Value<'gc>) -> Option<Value<'gc>>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    Ok(match (lhs, rhs) {
        (Value::Table(_) | Value::UserData(_), Value::Table(_) | Value::UserData(_)) => {
            if let Some(m) = get_metamethod(ctx, lhs, method) {
                MetaResult::Call(MetaCall {
                    function: call(ctx, m).map_err(|e| MetaOperatorError::Call(method, e))?,
                    args: [lhs, rhs],
                })
            } else if let Some(m) = get_metamethod(ctx, rhs, method) {
                MetaResult::Call(MetaCall {
                    function: call(ctx, m).map_err(|e| MetaOperatorError::Call(method, e))?,
                    args: [lhs, rhs],
                })
            } else {
                return Err(MetaOperatorError::Binary(
                    method,
                    lhs.type_name(),
                    rhs.type_name(),
                ));
            }
        }
        (Value::Table(_) | Value::UserData(_), _) => {
            if let Some(m) = get_metamethod(ctx, lhs, method) {
                MetaResult::Call(MetaCall {
                    function: call(ctx, m).map_err(|e| MetaOperatorError::Call(method, e))?,
                    args: [lhs, rhs],
                })
            } else {
                return Err(MetaOperatorError::Binary(
                    method,
                    lhs.type_name(),
                    rhs.type_name(),
                ));
            }
        }
        (_, Value::Table(_) | Value::UserData(_)) => {
            if let Some(m) = get_metamethod(ctx, rhs, method) {
                MetaResult::Call(MetaCall {
                    function: call(ctx, m).map_err(|e| MetaOperatorError::Call(method, e))?,
                    args: [lhs, rhs],
                })
            } else {
                return Err(MetaOperatorError::Binary(
                    method,
                    lhs.type_name(),
                    rhs.type_name(),
                ));
            }
        }
        (a, b) => {
            // Coercing or comparing strings takes time proportional to their length.
            charge_strings(ctx, &[a, b]);
            let (a, b) = match method {
                MetaMethod::Lt | MetaMethod::Le | MetaMethod::Concat => (a, b),
                _ => (coerce_lua51(ctx, a), coerce_lua51(ctx, b)),
            };
            const_op(ctx, a, b)
                .ok_or_else(|| MetaOperatorError::Binary(method, lhs.type_name(), rhs.type_name()))?
                .into()
        }
    })
}

fn meta_unary_metaop<'gc>(
    ctx: Context<'gc>,
    arg: Value<'gc>,
    method: MetaMethod,
    const_op: impl Fn(Value<'gc>) -> Option<Value<'gc>>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    Ok(match arg {
        Value::Table(_) | Value::UserData(_) => {
            if let Some(m) = get_metamethod(ctx, arg, method) {
                MetaResult::Call(MetaCall {
                    function: call(ctx, m).map_err(|e| MetaOperatorError::Call(method, e))?,
                    // PUC-Rio Lua passes the operand twice to a unary metamethod.
                    args: [arg, arg],
                })
            } else {
                return Err(MetaOperatorError::Unary(method, arg.type_name()));
            }
        }
        val => {
            charge_strings(ctx, &[val]);
            let val = coerce_lua51(ctx, val);
            const_op(val)
                .ok_or_else(|| MetaOperatorError::Unary(method, arg.type_name()))?
                .into()
        }
    })
}

// Integer `//` and `%` by zero are errors (instead of the float result) when both operands are (or
// coerce to) integers.
fn is_integer_division_by_zero<'gc>(lhs: Value<'gc>, rhs: Value<'gc>) -> bool {
    // A table or userdata operand goes to its metamethod: no string is parsed for it here.
    if matches!(lhs, Value::Table(_) | Value::UserData(_))
        || matches!(rhs, Value::Table(_) | Value::UserData(_))
    {
        return false;
    }
    matches!(
        (lhs.to_numeric(), rhs.to_numeric()),
        (Some(Value::Integer(_)), Some(Value::Integer(0)))
    )
}

// Both operands as integers, if they are and `ctx` follows Lua 5.1 (where integer results past 2^53
// round, see `lua51_integer_result`).
fn lua51_integers<'gc>(ctx: Context<'gc>, a: Value<'gc>, b: Value<'gc>) -> Option<(i128, i128)> {
    match (ctx.compat(), a, b) {
        (Compat::Lua51, Value::Integer(a), Value::Integer(b)) => {
            Some((i128::from(a), i128::from(b)))
        }
        _ => None,
    }
}

pub fn add<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_metaop(ctx, lhs, rhs, MetaMethod::Add, |ctx, a, b| {
        if let Some((a, b)) = lua51_integers(ctx, a, b) {
            return Some(lua51_integer_result(a + b).into());
        }
        Some(a.to_constant()?.add(&b.to_constant()?)?.into())
    })
}

pub fn subtract<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_metaop(ctx, lhs, rhs, MetaMethod::Sub, |ctx, a, b| {
        if let Some((a, b)) = lua51_integers(ctx, a, b) {
            return Some(lua51_integer_result(a - b).into());
        }
        Some(a.to_constant()?.subtract(&b.to_constant()?)?.into())
    })
}

pub fn multiply<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_metaop(ctx, lhs, rhs, MetaMethod::Mul, |ctx, a, b| {
        if let Some((a, b)) = lua51_integers(ctx, a, b) {
            // Lua 5.1 only has floats, where a zero product with one negative factor is -0.
            if (a == 0 || b == 0) && (a < 0) != (b < 0) {
                return Some(Value::Number(-0.0));
            }
            return Some(lua51_integer_result(a * b).into());
        }
        Some(a.to_constant()?.multiply(&b.to_constant()?)?.into())
    })
}

pub fn float_divide<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_metaop(ctx, lhs, rhs, MetaMethod::Div, |_, a, b| {
        Some(a.to_constant()?.float_divide(&b.to_constant()?)?.into())
    })
}

pub fn floor_divide<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    if is_integer_division_by_zero(lhs, rhs) {
        if ctx.compat() == Compat::Lua51 {
            // Lua 5.1 only has floats.
            let (a, b) = (lhs.to_number(), rhs.to_number());
            return Ok(
                Value::Number(float_floor_divide(a.unwrap_or(0.0), b.unwrap_or(0.0))).into(),
            );
        }
        return Err(MetaOperatorError::FloorDivideByZero);
    }
    meta_metaop(ctx, lhs, rhs, MetaMethod::IDiv, |_, a, b| {
        Some(a.to_constant()?.floor_divide(&b.to_constant()?)?.into())
    })
}

pub fn modulo<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    let lua51 = ctx.compat() == Compat::Lua51;
    if is_integer_division_by_zero(lhs, rhs) {
        if lua51 {
            let (a, b) = (lhs.to_number(), rhs.to_number());
            return Ok(Value::Number(lua51_modulo(a.unwrap_or(0.0), b.unwrap_or(0.0))).into());
        }
        return Err(MetaOperatorError::ModuloByZero);
    }
    meta_metaop(ctx, lhs, rhs, MetaMethod::Mod, move |_, a, b| {
        let (a, b) = (
            a.to_constant()?.to_numeric()?,
            b.to_constant()?.to_numeric()?,
        );
        match (a, b) {
            (Constant::Integer(_), Constant::Integer(_)) => Some(a.modulo(&b)?.into()),
            // Lua 5.1 computes `%` on floats as `a - floor(a / b) * b`.
            (a, b) if lua51 => Some(Value::Number(lua51_modulo(a.to_number()?, b.to_number()?))),
            (a, b) => Some(a.modulo(&b)?.into()),
        }
    })
}

/// Lua 5.1's float modulus, `a - floor(a / b) * b`.
pub fn lua51_modulo(a: f64, b: f64) -> f64 {
    a - (a / b).floor() * b
}

pub fn exponentiate<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_metaop(ctx, lhs, rhs, MetaMethod::Pow, |_, a, b| {
        Some(a.to_constant()?.exponentiate(&b.to_constant()?)?.into())
    })
}

pub fn negate<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    let lua51 = ctx.compat() == Compat::Lua51;
    meta_unary_metaop(ctx, lhs, MetaMethod::Unm, move |val| {
        match (lua51, val) {
            // Lua 5.1 only has floats, where -0 is a negative zero.
            (true, Value::Integer(0)) => Some(Value::Number(-0.0)),
            (true, Value::Integer(i)) => Some(lua51_integer_result(-i128::from(i)).into()),
            _ => Some(val.to_constant()?.negate()?.into()),
        }
    })
}

pub fn bitwise_not<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_unary_metaop(ctx, lhs, MetaMethod::BNot, |val| {
        Some(val.to_constant()?.bitwise_not()?.into())
    })
}

pub fn bitwise_and<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_metaop(ctx, lhs, rhs, MetaMethod::BAnd, |_, a, b| {
        Some(a.to_constant()?.bitwise_and(&b.to_constant()?)?.into())
    })
}

pub fn bitwise_or<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_metaop(ctx, lhs, rhs, MetaMethod::BOr, |_, a, b| {
        Some(a.to_constant()?.bitwise_or(&b.to_constant()?)?.into())
    })
}

pub fn bitwise_xor<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_metaop(ctx, lhs, rhs, MetaMethod::BXor, |_, a, b| {
        Some(a.to_constant()?.bitwise_xor(&b.to_constant()?)?.into())
    })
}

pub fn shift_left<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_metaop(ctx, lhs, rhs, MetaMethod::Shl, |_, a, b| {
        Some(a.to_constant()?.shift_left(&b.to_constant()?)?.into())
    })
}

pub fn shift_right<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    meta_metaop(ctx, lhs, rhs, MetaMethod::Shr, |_, a, b| {
        Some(a.to_constant()?.shift_right(&b.to_constant()?)?.into())
    })
}

pub fn less_than<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    lua51_order_check(ctx, MetaMethod::Lt, lhs, rhs)?;
    meta_metaop(ctx, lhs, rhs, MetaMethod::Lt, |_, a, b| {
        Some(a.to_constant()?.less_than(&b.to_constant()?)?.into())
    })
}

pub fn less_equal<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    lua51_order_check(ctx, MetaMethod::Le, lhs, rhs)?;
    let result = meta_metaop(ctx, lhs, rhs, MetaMethod::Le, |_, a, b| {
        Some(a.to_constant()?.less_equal(&b.to_constant()?)?.into())
    });
    if let Err(MetaOperatorError::Binary(..)) = &result {
        // Without a `__le`, PUC-Rio Lua (5.1, and 5.4 built with its 5.3 compatibility) tries
        // `not (b < a)` with a `__lt`.
        if let Some(lt) = get_metamethod(ctx, lhs, MetaMethod::Lt)
            .or_else(|| get_metamethod(ctx, rhs, MetaMethod::Lt))
        {
            #[derive(Collect)]
            #[collect(require_static)]
            struct Not;

            impl<'gc> Sequence<'gc> for Not {
                fn poll(
                    self: Pin<&mut Self>,
                    ctx: Context<'gc>,
                    _: Execution<'gc, '_>,
                    mut stack: Stack<'gc, '_>,
                ) -> Result<SequencePoll<'gc>, crate::Error<'gc>> {
                    let result = !stack.get(0).to_bool();
                    stack.replace(ctx, result);
                    Ok(SequencePoll::Return)
                }
            }

            let lt = call(ctx, lt).map_err(|e| MetaOperatorError::Call(MetaMethod::Lt, e))?;
            let not_lt = Callback::from_fn_with(&ctx, lt, |&lt, ctx, _, _| {
                Ok(CallbackReturn::Call {
                    function: lt,
                    then: Some(BoxSequence::new(&ctx, Not)),
                })
            });
            return Ok(MetaResult::Call(MetaCall {
                function: not_lt.into(),
                args: [rhs, lhs],
            }));
        }
    }
    result
}

// Lua 5.1 compares two tables (or two userdata) through `__lt`/`__le` only when both have the
// same metamethod, and never a table with a value of another type: otherwise the comparison is
// an error. For `<=`, a shared `__lt` serves when the `__le` values are not the same one.
fn lua51_order_check<'gc>(
    ctx: Context<'gc>,
    method: MetaMethod,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<(), MetaOperatorError> {
    if ctx.compat() != Compat::Lua51 {
        return Ok(());
    }
    let objects = (
        matches!(lhs, Value::Table(_) | Value::UserData(_)),
        matches!(rhs, Value::Table(_) | Value::UserData(_)),
    );
    let same_metamethod = |method| match (
        get_metamethod(ctx, lhs, method),
        get_metamethod(ctx, rhs, method),
    ) {
        (Some(a), Some(b)) => Some(raw_equal(a, b)),
        (None, None) => None,
        _ => Some(false),
    };
    let same = match objects {
        (false, false) => return Ok(()),
        (true, true) => {
            std::mem::discriminant(&lhs) == std::mem::discriminant(&rhs)
                && (same_metamethod(method) == Some(true)
                    || (method == MetaMethod::Le && same_metamethod(MetaMethod::Lt) == Some(true)))
        }
        _ => false,
    };
    if same {
        Ok(())
    } else {
        Err(MetaOperatorError::Binary(
            method,
            lhs.type_name(),
            rhs.type_name(),
        ))
    }
}

/// The result of a concat metaoperation, either a completed [`Value`]
/// or a [`Function`] that must be called with the values to
/// concatenate.
#[derive(Debug, Clone, Collect)]
#[collect(no_drop)]
pub enum ConcatMetaResult<'gc> {
    Value(Value<'gc>),
    Call(Function<'gc>),
}

pub fn concat<'gc>(
    ctx: Context<'gc>,
    lhs: Value<'gc>,
    rhs: Value<'gc>,
) -> Result<MetaResult<'gc, 2>, MetaOperatorError> {
    if let Some(bytes) = concat_strings(ctx, &[lhs, rhs], b"")? {
        return Ok(MetaResult::Value(Value::String(ctx.new_string(bytes))));
    }
    meta_metaop(ctx, lhs, rhs, MetaMethod::Concat, |_, _, _| None)
}

/// Appends the string form of a string or number to `bytes`, returns false (appending nothing) for
/// any other value.
fn append_string<'gc>(compat: Compat, bytes: &mut Vec<u8>, value: Value<'gc>) -> bool {
    match value {
        Value::Integer(i) => compat::write_integer(bytes, compat, i),
        Value::Number(n) => compat::write_float(bytes, compat, n),
        Value::String(s) => bytes.extend(s.as_bytes()),
        _ => return false,
    }
    true
}

/// Returns an estimate of the length of the concatenation of a list of values,
/// or returns [`None`] if any value is not implicitly coercible to a string.
fn estimate_concatenated_len<'gc>(
    values: &[Value<'gc>],
) -> Result<Option<usize>, MetaOperatorError> {
    let mut len = 0usize;
    for value in values {
        let value_len = match value {
            // ilog10 panics for values <= 0
            Value::Integer(i) => {
                i.unsigned_abs().max(1).ilog10() as usize + 1 + i.is_negative() as usize
            }
            Value::Number(_n) => 10,
            Value::String(s) => s.as_bytes().len(),
            _ => return Ok(None),
        };
        len = len
            .checked_add(value_len)
            .ok_or(MetaOperatorError::ConcatOverflow)?;
    }
    Ok(Some(len))
}

/// Concatenates values that are all strings or numbers, with a separator between each of them.
///
/// Returns `None` if any value is not a string or a number. Checks the string length and memory
/// limits before building the result.
fn concat_strings<'gc>(
    ctx: Context<'gc>,
    values: &[Value<'gc>],
    separator: &[u8],
) -> Result<Option<Vec<u8>>, MetaOperatorError> {
    // Since we have to make two passes to check for complex types, estimate the length in the
    // first pass.
    let Some(len) = estimate_concatenated_len(values)? else {
        return Ok(None);
    };
    let total_len = values
        .len()
        .saturating_sub(1)
        .checked_mul(separator.len())
        .and_then(|l| l.checked_add(len))
        .ok_or(MetaOperatorError::ConcatOverflow)?;
    ctx.check_string(total_len)?;
    // Collecting the values (creating the result charges for its bytes).
    ctx.charge_fuel(fuel_for(values.len(), cost::VALUES));

    let compat = ctx.compat();
    let mut bytes = Vec::with_capacity(total_len);
    for (i, &value) in values.iter().enumerate() {
        if i > 0 {
            bytes.extend_from_slice(separator);
        }
        if !append_string(compat, &mut bytes, value) {
            return Ok(None);
        }
    }
    // Numbers may be longer than estimated.
    if bytes.len() > total_len {
        ctx.check_string(bytes.len())?;
    }
    Ok(Some(bytes))
}

pub fn concat_many<'gc>(
    ctx: Context<'gc>,
    values: &[Value<'gc>],
) -> Result<ConcatMetaResult<'gc>, MetaOperatorError> {
    if let Some(bytes) = concat_strings(ctx, values, b"")? {
        return Ok(ConcatMetaResult::Value(Value::String(
            ctx.new_string(bytes),
        )));
    }

    // Fall back to a sequence-based implemenation to handle metamethods
    let func = Callback::from_fn(&ctx, |ctx, _, stack| {
        let args = stack.len();
        let s = async_sequence(&ctx, |_, mut seq| async move {
            for i in (1..args).into_iter().rev() {
                let call = seq.try_enter(|ctx, locals, _, mut stack| {
                    let bottom = i - 1;
                    let call = concat(ctx, stack.get(i - 1), stack.get(i))?;
                    let p = prepare_async_metaop(ctx, &mut stack, locals, bottom, call, 1);
                    Ok(p)
                })?;
                call.execute(&mut seq).await?;
                if fuel_used_up(&mut seq) {
                    seq.pending().await;
                }
            }
            Ok(SequenceReturn::Return)
        });
        Ok(CallbackReturn::Sequence(s))
    });
    Ok(ConcatMetaResult::Call(func.into()))
}

// Whether the step's fuel is used up, with the debt charged so far (by the strings the
// concatenations made) consumed: a concatenation of many values with a metamethod among them
// would otherwise fold them all in one poll.
fn fuel_used_up(seq: &mut AsyncSequence) -> bool {
    seq.enter(|ctx, _, mut exec, _| {
        exec.fuel().consume(ctx.take_fuel_debt());
        exec.fuel().remaining() <= 0
    })
}

pub fn concat_separated<'gc>(
    ctx: Context<'gc>,
    values: &[Value<'gc>],
    separator: Value<'gc>,
) -> Result<ConcatMetaResult<'gc>, MetaOperatorError> {
    if separator.is_nil() {
        return concat_many(ctx, values);
    }

    if let Some(separator) = separator.into_string(ctx) {
        if let Some(bytes) = concat_strings(ctx, values, &separator)? {
            return Ok(ConcatMetaResult::Value(Value::String(
                ctx.new_string(bytes),
            )));
        }
    }

    // Fall back to a sequence-based implemenation to handle metamethods
    let func = Callback::from_fn_with(&ctx, separator, move |&sep, ctx, _, stack| {
        let args = stack.len();
        let b = async_sequence(&ctx, |locals, mut seq| {
            let sep = locals.stash(&ctx, sep);
            async move {
                for i in (1..args).into_iter().rev() {
                    let call = seq.try_enter(|ctx, locals, _, mut stack| {
                        let bottom = i;
                        let call = concat(ctx, locals.fetch(&sep), stack.get(i))?;
                        let p = prepare_async_metaop(ctx, &mut stack, locals, bottom, call, 1);
                        Ok(p)
                    })?;
                    call.execute(&mut seq).await?;

                    let call = seq.try_enter(|ctx, locals, _, mut stack| {
                        let bottom = i - 1;
                        let call = concat(ctx, stack.get(i - 1), stack.get(i))?;
                        let p = prepare_async_metaop(ctx, &mut stack, locals, bottom, call, 1);
                        Ok(p)
                    })?;
                    call.execute(&mut seq).await?;
                    if fuel_used_up(&mut seq) {
                        seq.pending().await;
                    }
                }
                Ok(SequenceReturn::Return)
            }
        });
        Ok(CallbackReturn::Sequence(b))
    });
    Ok(ConcatMetaResult::Call(func.into()))
}

#[must_use]
struct PreparedCall {
    func: Option<crate::StashedFunction>,
    bottom: usize,
    returns: usize,
}

impl PreparedCall {
    async fn execute(self, seq: &mut AsyncSequence) -> Result<(), crate::StashedError> {
        if let Some(func) = self.func {
            seq.call(&func, self.bottom).await?;
        }
        seq.enter(|_, _, _, mut stack| {
            stack.resize(self.bottom + self.returns);
        });
        Ok(())
    }
}

fn prepare_async_metaop<'gc, const N: usize>(
    ctx: Context<'gc>,
    stack: &mut Stack<'gc, '_>,
    locals: Locals<'gc, '_>,
    bottom: usize,
    call: MetaResult<'gc, N>,
    returns: usize,
) -> PreparedCall {
    match call {
        MetaResult::Value(v) => {
            stack.resize(bottom);
            stack.push_back(v);
            PreparedCall {
                func: None,
                bottom,
                returns,
            }
        }
        MetaResult::Call(MetaCall { function, args }) => {
            stack.resize(bottom);
            stack.extend(args);
            PreparedCall {
                func: Some(locals.stash(&ctx, function)),
                bottom,
                returns,
            }
        }
    }
}
