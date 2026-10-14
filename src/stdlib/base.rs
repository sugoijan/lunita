use std::pin::Pin;

use gc_arena::{Collect, Gc};

use super::args::{
    arg_error, check_any, check_int, check_integer, check_string, check_table, error, error_bytes,
    is_lua51, opt_integer, type_error,
};
use crate::compiler::string_utils::c_str;
use crate::{
    BoxSequence, Callback, CallbackReturn, Closure, CompilerError, Context, Error, Execution,
    Function, FunctionPrototype, IntoValue, LimitError, MetaMethod, Sequence, SequencePoll, Stack,
    String, Table, TypeError, Value, Variadic,
    closure::{UpValue, UpValueState},
    compat::Compat,
    compiler::{CompileBudget, CompileLimits},
    fuel::{cost, fuel_for},
    limits::ByteBuffer,
    memory::TrackedVec,
    meta_ops::{self, MetaResult},
    table::NextValue,
};

// DELIBERATE(puc): the library leaves to the host `_G`, `coroutine.wrap`, `isyieldable` and
// `close`, `collectgarbage` options other than "count", `os`, `io` beyond `print`, `debug`,
// `utf8`, `package`, and the 5.1 aliases (`unpack`, `loadstring`, `table.getn`, `math.mod`,
// `math.pow`, `string.gfind`); see COMPATIBILITY.md.
pub fn load_base<'gc>(ctx: Context<'gc>) -> Result<(), LimitError> {
    ctx.set_global(
        "tonumber",
        Callback::from_fn(&ctx, |ctx, mut exec, mut stack| {
            use crate::compiler::string_utils::{
                lua51_str2number, parse_c_ulong, read_neg, trim_whitespace,
            };

            if let Value::String(s) = stack.get(0) {
                exec.fuel()
                    .consume(fuel_for(s.as_bytes().len(), cost::BYTES_PROCESSED));
            }
            let lua51 = is_lua51(ctx);

            check_any(&stack, "tonumber", 0)?;
            if stack.len() == 1
                || stack.get(1).is_nil()
                // Lua 5.1 converts in base 10 like without a base (the base is truncated).
                || (lua51 && stack.get(1).to_number().map(|n| n as i64) == Some(10))
            {
                stack.drain(1..);
                let value = stack.consume::<Value>(ctx)?;
                let number = match value {
                    Value::String(s) if lua51 => {
                        lua51_str2number(s.as_bytes()).map_or(Value::Nil, Value::Number)
                    }
                    value => value.to_numeric().unwrap_or(Value::Nil),
                };
                stack.replace(ctx, number);
            } else {
                let (value, base) = (stack.get(0), check_integer(ctx, &stack, "tonumber", 1)?);
                let s = match value {
                    Value::String(s) => s,
                    // Lua 5.1 converts numbers to strings, 5.4 does not.
                    Value::Integer(_) | Value::Number(_) if lua51 => {
                        value.into_string(ctx).unwrap_or_else(|| ctx.intern(b""))
                    }
                    _ => {
                        return Err(TypeError {
                            expected: "string",
                            found: value.type_name(),
                        }
                        .into());
                    }
                };
                if !(2..=36).contains(&base) {
                    return Err(arg_error("tonumber", 1, "base out of range"));
                }

                let result = if lua51 {
                    // `strtoul` on the string up to its first zero byte.
                    let bytes = c_str(s.as_bytes());
                    parse_c_ulong(bytes, base as u32).and_then(|(n, end)| {
                        trim_whitespace(bytes.get(end..).unwrap_or_default())
                            .is_empty()
                            .then_some(Value::Number(n as f64))
                    })
                } else {
                    let (is_neg, digits) = read_neg(trim_whitespace(s.as_bytes()));
                    if digits.is_empty() {
                        None
                    } else {
                        digits
                            .iter()
                            .map(|&b| (b as char).to_digit(36).map(i64::from))
                            .try_fold(0i64, |acc, d| match d {
                                Some(d) if d < base => Some(acc.wrapping_mul(base).wrapping_add(d)),
                                _ => None,
                            })
                            .map(|v| Value::Integer(if is_neg { v.wrapping_neg() } else { v }))
                    }
                };
                stack.replace(ctx, result.unwrap_or(Value::Nil));
            }

            Ok(CallbackReturn::Return)
        }),
    )?;

    ctx.set_global(
        "tostring",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            // The result of a `__tostring` call: the first value in Lua 5.1, a string (or a number
            // converted to one) in Lua 5.4. The frame also keeps `tostring` a level for `error`,
            // as the C function is in PUC-Rio Lua.
            #[derive(Collect)]
            #[collect(require_static)]
            struct ToStringResult;

            impl<'gc> Sequence<'gc> for ToStringResult {
                fn poll(
                    self: Pin<&mut Self>,
                    ctx: Context<'gc>,
                    _: Execution<'gc, '_>,
                    mut stack: Stack<'gc, '_>,
                ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                    let result = stack.get(0);
                    let result = match result {
                        _ if is_lua51(ctx) => result,
                        Value::String(_) => result,
                        Value::Integer(_) | Value::Number(_) => {
                            result.into_string(ctx).map_or(Value::Nil, Value::String)
                        }
                        _ => return Err(error("'__tostring' must return a string")),
                    };
                    stack.replace(ctx, result);
                    Ok(SequencePoll::Return)
                }
            }

            let value = check_any(&stack, "tostring", 0)?;
            match meta_ops::tostring(ctx, value)? {
                MetaResult::Value(v) => {
                    stack.replace(ctx, v);
                    Ok(CallbackReturn::Return)
                }
                MetaResult::Call(call) => {
                    stack.replace(ctx, Variadic(call.args));
                    Ok(CallbackReturn::Call {
                        function: call.function,
                        then: Some(BoxSequence::new(&ctx, ToStringResult)),
                    })
                }
            }
        }),
    )?;

    ctx.set_global(
        "error",
        Callback::from_fn(&ctx, |ctx, exec, stack| {
            let message = stack.get(0);
            let level = opt_integer(ctx, &stack, "error", 1, 1)?;
            // A string message gets the position where the error was raised, at the given level
            // (Lua 5.1 also converts a number to a string with a position).
            let message = match message {
                Value::Integer(_) | Value::Number(_) if is_lua51(ctx) => {
                    message.into_string(ctx).map_or(message, Value::String)
                }
                message => message,
            };
            if let (Value::String(s), Ok(level @ 1..)) = (message, usize::try_from(level)) {
                if let Some(frame) = exec.lua_frame(level) {
                    return Err(Value::String(frame.locate(ctx, s.as_bytes())?).into());
                }
            }
            Err(message.into())
        }),
    )?;

    ctx.set_global(
        "xpcall",
        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            let handler = stack.get(1);
            // Lua 5.4 checks the handler, Lua 5.1 only that there is one (it fails when calling
            // it).
            if is_lua51(ctx) {
                check_any(&stack, "xpcall", 1)?;
            } else if !matches!(handler, Value::Function(_)) {
                return Err(type_error(&stack, "xpcall", 1, "function"));
            }
            let function = match meta_ops::call(ctx, stack.get(0)) {
                Ok(function) => function,
                // Calling a value that cannot be called is an error for the handler.
                Err(err) => {
                    let Ok(handler) = meta_ops::call(ctx, handler) else {
                        stack.replace(ctx, (false, "error in error handling"));
                        return Ok(CallbackReturn::Return);
                    };
                    stack.replace(ctx, Error::from(err).to_value(ctx));
                    return Ok(CallbackReturn::Call {
                        function: handler,
                        then: Some(BoxSequence::new(
                            &ctx,
                            XPCall {
                                handler: handler.into(),
                                handling: true,
                            },
                        )),
                    });
                }
            };
            if is_lua51(ctx) {
                // Lua 5.1 does not pass extra arguments to the function.
                stack.clear();
            } else {
                stack.drain(..2);
            }
            Ok(CallbackReturn::Call {
                function,
                then: Some(BoxSequence::new(
                    &ctx,
                    XPCall {
                        handler,
                        handling: false,
                    },
                )),
            })
        }),
    )?;

    ctx.set_global(
        "assert",
        Callback::from_fn(&ctx, |ctx, _, stack| {
            if check_any(&stack, "assert", 0)?.to_bool() {
                return Ok(CallbackReturn::Return);
            }
            // String messages get the position of the caller, Lua 5.4 raises other values as
            // they are and Lua 5.1 only takes strings (and numbers).
            let message = match stack.get(1) {
                // Lua 5.4 raises a message given as nil; 5.1 takes the default.
                Value::Nil if stack.len() >= 2 && !is_lua51(ctx) => return Err(Value::Nil.into()),
                Value::Nil => None,
                Value::String(s) => Some(s),
                _ if is_lua51(ctx) => Some(check_string(ctx, &stack, "assert", 1)?),
                v => return Err(v.into()),
            };
            let message = message.map_or(&b"assertion failed!"[..], |s| s.as_bytes());
            // Lua 5.1 formats the message with `luaL_error(L, "%s", msg)`, which ends it at its
            // first zero byte.
            let message = if is_lua51(ctx) {
                c_str(message)
            } else {
                message
            };
            Err(error_bytes(message.to_vec()))
        }),
    )?;

    ctx.set_global(
        "pcall",
        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            let function = match meta_ops::call(ctx, check_any(&stack, "pcall", 0)?) {
                Ok(function) => function,
                // Calling a value that cannot be called is an error for `pcall` to return.
                Err(err) => {
                    stack.replace(ctx, (false, Error::from(err)));
                    return Ok(CallbackReturn::Return);
                }
            };
            stack.pop_front();
            Ok(CallbackReturn::Call {
                function,
                then: Some(BoxSequence::new(&ctx, PCall)),
            })
        }),
    )?;

    ctx.set_global(
        "type",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let value = check_any(&stack, "type", 0)?;
            stack.replace(ctx, value.type_name());
            Ok(CallbackReturn::Return)
        }),
    )?;

    ctx.set_global(
        "select",
        Callback::from_fn(&ctx, |ctx, mut exec, mut stack| {
            // Dropping the leading values moves the others.
            exec.fuel().consume(fuel_for(stack.len(), cost::VALUES));
            let n = stack.len() as i64;
            if matches!(stack.get(0), Value::String(s) if s.as_bytes().first() == Some(&b'#')) {
                stack.replace(ctx, n - 1);
                return Ok(CallbackReturn::Return);
            }
            let i = check_int(ctx, &stack, "select", 0)?;
            let i = if i < 0 { n.saturating_add(i) } else { i.min(n) };
            if i < 1 {
                return Err(arg_error("select", 0, "index out of range"));
            }
            stack.drain(..i as usize);
            Ok(CallbackReturn::Return)
        }),
    )?;

    ctx.set_global(
        "rawget",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            check_any(&stack, "rawget", 1)?;
            let (table, key): (Table, Value) = stack.consume(ctx)?;
            stack.replace(ctx, table.get_value(ctx, key));
            Ok(CallbackReturn::Return)
        }),
    )?;

    ctx.set_global(
        "rawequal",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let equal = meta_ops::raw_equal(
                check_any(&stack, "rawequal", 0)?,
                check_any(&stack, "rawequal", 1)?,
            );
            stack.replace(ctx, equal);
            Ok(CallbackReturn::Return)
        }),
    )?;

    ctx.set_global(
        "rawlen",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let len = match stack.get(0) {
                Value::Table(table) => table.length_charged(ctx),
                Value::String(s) => s.len(),
                _ => return Err(type_error(&stack, "rawlen", 0, "table or string")),
            };
            stack.replace(ctx, len);
            Ok(CallbackReturn::Return)
        }),
    )?;

    ctx.set_global(
        "rawset",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            check_any(&stack, "rawset", 2)?;
            let (table, key, value): (Table, Value, Value) = stack.consume(ctx)?;
            table.set(ctx, key, value)?;
            stack.replace(ctx, table);
            Ok(CallbackReturn::Return)
        }),
    )?;

    ctx.set_global(
        "getmetatable",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let metatable = match check_any(&stack, "getmetatable", 0)? {
                Value::Table(t) => t.metatable(),
                Value::UserData(u) => u.metatable(),
                Value::String(_) => ctx.string_metatable(),
                _ => None,
            };
            // A `__metatable` field protects the metatable.
            let result = match metatable {
                Some(mt) => match mt.get_value(ctx, "__metatable") {
                    Value::Nil => Value::Table(mt),
                    protected => protected,
                },
                None => Value::Nil,
            };
            stack.replace(ctx, result);
            Ok(CallbackReturn::Return)
        }),
    )?;

    ctx.set_global(
        "setmetatable",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let Value::Table(t) = stack.get(0) else {
                return Err(type_error(&stack, "setmetatable", 0, "table"));
            };
            let mt = match stack.get(1) {
                Value::Nil if stack.len() >= 2 => None,
                Value::Table(mt) => Some(mt),
                _ if is_lua51(ctx) => {
                    return Err(arg_error("setmetatable", 1, "nil or table expected"));
                }
                _ => return Err(type_error(&stack, "setmetatable", 1, "nil or table")),
            };
            if t.metatable()
                .is_some_and(|mt| !mt.get_value(ctx, "__metatable").is_nil())
            {
                return Err(error("cannot change a protected metatable"));
            }
            t.set_metatable(&ctx, mt);
            stack.replace(ctx, t);
            Ok(CallbackReturn::Return)
        }),
    )?;

    fn next<'gc>(
        ctx: Context<'gc>,
        table: Table<'gc>,
        index: Value<'gc>,
    ) -> Result<(Value<'gc>, Value<'gc>), Value<'gc>> {
        match table.next_charged(ctx, index) {
            NextValue::Found { key, value } => Ok((key, value)),
            NextValue::Last => Ok((Value::Nil, Value::Nil)),
            NextValue::NotFound => Err("invalid key to 'next'".into_value(ctx)),
        }
    }

    let next = Callback::from_fn(&ctx, |ctx, _, mut stack| {
        let (table, index): (Table, Value) = stack.consume(ctx)?;
        match next(ctx, table, index)? {
            // One nil at the end, as PUC-Rio Lua returns.
            (Value::Nil, _) => stack.replace(ctx, Value::Nil),
            entry => stack.replace(ctx, entry),
        }
        Ok(CallbackReturn::Return)
    });

    ctx.set_global("next", next)?;

    ctx.set_global(
        "pairs",
        Callback::from_fn_with(&ctx, next, move |next, ctx, _, mut stack| {
            check_iterated(ctx, &stack, "pairs")?;
            let table = stack.get(0);
            // Lua 5.1 has no `__pairs`.
            if let Some(mt) = match table {
                _ if is_lua51(ctx) => None,
                Value::Table(t) => t.metatable(),
                Value::UserData(u) => u.metatable(),
                _ => None,
            } {
                /// `__pairs` returns exactly three values, as PUC-Rio Lua calls it with three results.
                #[derive(Collect)]
                #[collect(require_static)]
                struct PairsReturn;

                impl<'gc> Sequence<'gc> for PairsReturn {
                    fn poll(
                        self: Pin<&mut Self>,
                        _ctx: Context<'gc>,
                        _exec: Execution<'gc, '_>,
                        mut stack: Stack<'gc, '_>,
                    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                        stack.resize(3);
                        Ok(SequencePoll::Return)
                    }
                }

                let pairs = mt.get_value(ctx, MetaMethod::Pairs);
                if !pairs.is_nil() {
                    let function = meta_ops::call(ctx, pairs)?;
                    stack.replace(ctx, (table, Value::Nil));
                    return Ok(CallbackReturn::Call {
                        function,
                        then: Some(BoxSequence::new(&ctx, PairsReturn)),
                    });
                }
            }

            stack.replace(ctx, (*next, table, Value::Nil));
            Ok(CallbackReturn::Return)
        }),
    )?;

    let inext = Callback::from_fn(&ctx, |ctx, _, mut stack| {
        let (table, index): (Value, Option<i64>) = stack.consume(ctx)?;
        let next_index = index.unwrap_or(0).wrapping_add(1);
        // Lua 5.1's `ipairs` reads the table raw.
        if let (true, Value::Table(t)) = (is_lua51(ctx), table) {
            let v = t.get_value(ctx, next_index);
            if !v.is_nil() {
                stack.extend([next_index.into(), v]);
            }
            return Ok(CallbackReturn::Return);
        }
        Ok(match meta_ops::index(ctx, table, next_index.into())? {
            MetaResult::Value(v) => {
                if !v.is_nil() {
                    stack.extend([next_index.into(), v]);
                }
                CallbackReturn::Return
            }
            MetaResult::Call(call) => {
                #[derive(Collect)]
                #[collect(require_static)]
                struct INext(i64);

                impl<'gc> Sequence<'gc> for INext {
                    fn poll(
                        self: Pin<&mut Self>,
                        _ctx: Context<'gc>,
                        _exec: Execution<'gc, '_>,
                        mut stack: Stack<'gc, '_>,
                    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                        if !stack.get(0).is_nil() {
                            stack.push_front(self.0.into());
                        }
                        Ok(SequencePoll::Return)
                    }
                }

                stack.extend(call.args);
                CallbackReturn::Call {
                    function: call.function,
                    then: Some(BoxSequence::new(&ctx, INext(next_index))),
                }
            }
        })
    });

    ctx.set_global(
        "ipairs",
        Callback::from_fn_with(&ctx, inext, move |inext, ctx, _, mut stack| {
            check_iterated(ctx, &stack, "ipairs")?;
            // `inext, t, 0`, as PUC-Rio Lua returns.
            stack.resize(1);
            stack.into_front(ctx, *inext);
            stack.push_back(Value::Integer(0));
            Ok(CallbackReturn::Return)
        }),
    )?;

    // DELIBERATE(puc): in Lua51 mode `load` takes strings too (5.1's takes functions only); the host
    // adds `loadstring` (see COMPATIBILITY.md).
    ctx.set_global("load", Callback::from_fn(&ctx, load))?;

    ctx.set_global(
        "collectgarbage",
        Callback::from_fn(&ctx, move |ctx, _, mut stack| {
            match stack.consume::<Option<String>>(ctx)? {
                Some(arg) if arg == "count" => {
                    stack.into_back(ctx, ctx.total_memory() as f64 / 1024.0);
                }
                Some(arg) => {
                    return Err(arg_error(
                        "collectgarbage",
                        0,
                        format!("invalid option '{}'", arg.display_lossy()),
                    ));
                }
                None => {}
            }
            Ok(CallbackReturn::Return)
        }),
    )?;

    ctx.set_global(
        "_VERSION",
        if is_lua51(ctx) { "Lua 5.1" } else { "Lua 5.4" },
    )?;
    Ok(())
}

/// The most work `load` does to compile a chunk (see [`CompileBudget::work`]), so that a single
/// `load` call fits in an `Executor` step: at most about 2–3 ms natively on an Apple M1 Max, and
/// 3–4 ms in wasm. Larger chunks (more than about 20,000 tokens, or 320 KiB of source) are a
/// "chunk is too complex to compile" error, which `load` returns as its second result.
pub const LOAD_MAX_WORK: u64 = 20_000;

// `load(chunk [, chunkname [, mode [, env]]])`, where `chunk` is a string or a function returning
// the pieces of the chunk.
fn load<'gc>(
    ctx: Context<'gc>,
    _: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> Result<CallbackReturn<'gc>, Error<'gc>> {
    let has_env = stack.len() >= 4;
    let (chunk, chunk_name, mode, env): (Value, Option<String>, Option<String>, Value) =
        stack.consume(ctx)?;
    let options = LoadOptions {
        chunk_name,
        mode,
        env: has_env.then_some(env),
    };

    match chunk {
        Value::String(source) => {
            options.compile(ctx, &mut stack, source.as_bytes())?;
            Ok(CallbackReturn::Return)
        }
        Value::Function(reader) => {
            // Call the reader until it returns nil or an empty string, accumulating the pieces.
            #[derive(Collect)]
            #[collect(no_drop)]
            struct ReadChunk<'gc> {
                reader: Function<'gc>,
                options: LoadOptions<'gc>,
                #[collect(require_static)]
                source: ByteBuffer,
            }

            impl<'gc> Sequence<'gc> for ReadChunk<'gc> {
                fn poll(
                    mut self: Pin<&mut Self>,
                    ctx: Context<'gc>,
                    mut exec: Execution<'gc, '_>,
                    mut stack: Stack<'gc, '_>,
                ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                    let this = &mut *self;
                    match stack.get(0) {
                        Value::String(piece) if !piece.as_bytes().is_empty() => {
                            exec.fuel()
                                .consume(fuel_for(piece.as_bytes().len(), cost::BYTES_PROCESSED));
                            this.source.extend(ctx, piece.as_bytes())?;
                            stack.clear();
                            Ok(SequencePoll::Call {
                                function: this.reader,
                                bottom: 0,
                            })
                        }
                        Value::Nil | Value::String(_) => {
                            stack.clear();
                            this.options
                                .compile(ctx, &mut stack, this.source.as_slice())?;
                            Ok(SequencePoll::Return)
                        }
                        v => {
                            stack.replace(
                                ctx,
                                (
                                    Value::Nil,
                                    format!(
                                        "reader function must return a string, got {}",
                                        v.type_name()
                                    ),
                                ),
                            );
                            Ok(SequencePoll::Return)
                        }
                    }
                }
            }

            Ok(CallbackReturn::Call {
                function: reader,
                then: Some(BoxSequence::new(
                    &ctx,
                    ReadChunk {
                        reader,
                        options,
                        source: ByteBuffer::new(),
                    },
                )),
            })
        }
        v => Err(TypeError {
            expected: "string or function",
            found: v.type_name(),
        }
        .into()),
    }
}

// The argument of `pairs` and `ipairs`: any value in Lua 5.4 (iterating it may fail later), a table
// in Lua 5.1.
fn check_iterated<'gc>(
    ctx: Context<'gc>,
    stack: &Stack<'gc, '_>,
    name: &str,
) -> Result<(), Error<'gc>> {
    if is_lua51(ctx) {
        check_table(stack, name, 0)?;
    } else {
        check_any(stack, name, 0)?;
    }
    Ok(())
}

/// The name of a chunk as PUC-Rio Lua's `luaO_chunkid` shows it in run-time positions: a chunk
/// name starting with '=' or '@' is shown without it, other names (usually the source of the
/// chunk) are shown as `[string "<first line>"]`, truncated to fit in 60 bytes (5.1 keeps 52
/// bytes of a file name, 5.4 keeps 56). The name is made once per chunk, so syntax errors show it
/// too, where Lua 5.1 would keep up to 80 bytes.
pub fn chunk_id(compat: Compat, name: &[u8]) -> std::string::String {
    const ID_SIZE: usize = 60;
    let lossy = |b: &[u8]| std::string::String::from_utf8_lossy(b).into_owned();
    match name.split_first() {
        Some((b'=', rest)) => lossy(rest.get(..ID_SIZE - 1).unwrap_or(rest)),
        Some((b'@', rest)) => {
            let keep = if compat == Compat::Lua51 {
                ID_SIZE - 8
            } else {
                ID_SIZE - 4
            };
            if rest.len() < keep || (compat == Compat::Lua51 && rest.len() == keep) {
                lossy(rest)
            } else {
                // Keep the end of long file names.
                format!(
                    "...{}",
                    lossy(rest.get(rest.len() - keep..).unwrap_or(rest))
                )
            }
        }
        _ => {
            let (line_end, max) = if compat == Compat::Lua51 {
                (
                    name.iter().position(|&c| c == b'\n' || c == b'\r'),
                    ID_SIZE - 17,
                )
            } else {
                (name.iter().position(|&c| c == b'\n'), ID_SIZE - 15)
            };
            let line = name.get(..line_end.unwrap_or(name.len())).unwrap_or(name);
            // Lua 5.1 keeps names of exactly `max` bytes.
            let fits = name.len() < max || (compat == Compat::Lua51 && name.len() == max);
            if line_end.is_none() && fits {
                format!("[string \"{}\"]", lossy(name))
            } else {
                format!("[string \"{}...\"]", lossy(line.get(..max).unwrap_or(line)))
            }
        }
    }
}

#[derive(Collect)]
#[collect(no_drop)]
struct LoadOptions<'gc> {
    chunk_name: Option<String<'gc>>,
    mode: Option<String<'gc>>,
    env: Option<Value<'gc>>,
}

impl<'gc> LoadOptions<'gc> {
    // Compiles a chunk, placing the function, or nil and an error message, on the stack.
    fn compile(
        &self,
        ctx: Context<'gc>,
        stack: &mut Stack<'gc, '_>,
        source: &[u8],
    ) -> Result<(), Error<'gc>> {
        if self
            .mode
            .is_some_and(|mode| !mode.as_bytes().contains(&b't'))
        {
            stack.replace(ctx, (Value::Nil, "attempt to load a text chunk"));
            return Ok(());
        }

        let budget = CompileBudget::new(CompileLimits::for_context(ctx, Some(LOAD_MAX_WORK)));
        // A chunk loaded from a string is named after its source by default.
        let chunk_name = chunk_id(
            ctx.compat(),
            self.chunk_name.map_or(source, |name| name.as_bytes()),
        );
        let result = FunctionPrototype::compile_with_budget(ctx, &chunk_name, source, &budget);
        ctx.charge_fuel(
            i32::try_from(budget.work() / cost::COMPILE_WORK as u64)
                .unwrap_or(i32::MAX)
                .saturating_add(fuel_for(source.len(), cost::SOURCE_BYTES)),
        );

        match result {
            Ok(proto) => {
                let proto = Gc::new(&ctx, proto);
                let mut upvalues = TrackedVec::new();
                if !proto.upvalues.is_empty() {
                    // A chunk's only upvalue is `_ENV`.
                    let env = self.env.unwrap_or(Value::Table(ctx.globals()));
                    upvalues.push(UpValue::new(&ctx, UpValueState::Closed(env)));
                }
                stack.replace(ctx, Closure::from_parts(&ctx, proto, upvalues));
            }
            Err(CompilerError::Limit(err)) => return Err(err.into()),
            Err(CompilerError::Parsing(err)) => stack.replace(ctx, (Value::Nil, err.to_string())),
            Err(CompilerError::Compilation(err)) => {
                stack.replace(ctx, (Value::Nil, err.to_string()))
            }
        }
        Ok(())
    }
}

/// The continuation of `xpcall`: returns `true` and the results of the function, or `false` and
/// the result of the message handler, called with the error.
#[derive(Collect)]
#[collect(no_drop)]
struct XPCall<'gc> {
    handler: Value<'gc>,
    // Whether the handler is being called.
    handling: bool,
}

impl<'gc> Sequence<'gc> for XPCall<'gc> {
    fn poll(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        if self.handling {
            // The handler returned: return `false` and its first result.
            let result = stack.get(0);
            stack.replace(ctx, (false, result));
        } else {
            stack.into_front(ctx, true);
        }
        Ok(SequencePoll::Return)
    }

    fn error(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        error: Error<'gc>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        if self.handling {
            // An error in the handler.
            stack.replace(ctx, (false, "error in error handling"));
            return Ok(SequencePoll::Return);
        }
        let Ok(handler) = meta_ops::call(ctx, self.handler) else {
            stack.replace(ctx, (false, "error in error handling"));
            return Ok(SequencePoll::Return);
        };
        self.handling = true;
        stack.replace(ctx, error.to_value(ctx));
        Ok(SequencePoll::Call {
            function: handler,
            bottom: 0,
        })
    }
}

#[derive(Collect)]
#[collect(require_static)]
pub struct PCall;

impl<'gc> Sequence<'gc> for PCall {
    fn poll(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        stack.into_front(ctx, true);
        Ok(SequencePoll::Return)
    }

    fn error(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        _exec: Execution<'gc, '_>,
        error: Error<'gc>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        stack.replace(ctx, (false, error));
        Ok(SequencePoll::Return)
    }
}
