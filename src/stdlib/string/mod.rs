//! The string library: `string.byte`, `char`, `find`, `format`, `gmatch`, `gsub`, `len`, `lower`,
//! `match`, `rep`, `reverse`, `sub` and `upper`, and the string metatable (`__index = string`).
//!
//! Every function checks the string length and memory limits before building a string, and charges
//! fuel for the bytes it reads and writes. Pattern matching charges fuel per step of the matcher,
//! fails with "pattern too complex" past [`MAX_MATCH_STEPS`] steps in one match attempt or
//! [`pattern::MAX_DEPTH`] nested calls, and suspends between match attempts (`find`, `match`,
//! `gmatch`) or replacements (`gsub`) when the step's fuel runs out.
//!
//! Where Lua versions differ, [`Compat::Lua51`](crate::Compat::Lua51) selects Lua 5.1's behaviour.

mod format;
mod pattern;

use std::pin::Pin;

use gc_arena::{Collect, Gc, lock::RefLock};

use crate::{
    BoxSequence, Callback, CallbackReturn, Context, Error, Execution, Fuel, Function, LimitError,
    Sequence, SequencePoll, Stack, String, Table, Value, compat,
    compiler::string_utils::c_str,
    fuel::{cost, fuel_for},
    limits::ByteBuffer,
    memory::Written,
    meta_ops::{self, MetaResult},
};

use self::{
    format::{FormatError, Spec},
    pattern::{Capture, Matcher, Plain},
};
use super::{
    args::{
        arg_error, check_int, check_integer, check_number, check_string, error, is_lua51, opt_int,
        opt_integer, type_error,
    },
    continue_sequence,
};

// DELIBERATE(puc): PUC has no such limit; a pattern needing more fails where PUC returns nil
// (see COMPATIBILITY.md).
/// The most steps a single match attempt may take, past which it is a "pattern too complex" error.
///
/// An attempt runs within one `Executor::step`. The slowest steps (backtracking over `.-` and `*`)
/// take about 7 ns each natively on an Apple M1 Max and 11 ns in wasm under Node, so an attempt
/// takes at most about 0.7 ms natively and 1.1 ms in wasm (at `opt-level = "z"` too; docs/hardening-notes.md
/// keeps the measured bounds).
pub const MAX_MATCH_STEPS: u64 = 100_000;

type Result<'gc, T> = std::result::Result<T, Error<'gc>>;
type CallbackResult<'gc> = Result<'gc, CallbackReturn<'gc>>;

pub fn load_string<'gc>(ctx: Context<'gc>) -> std::result::Result<(), LimitError> {
    let string = Table::new(&ctx);

    macro_rules! functions {
        ($($name:literal => $f:ident,)*) => {
            $(string.set_field(ctx, $name, Callback::from_fn(&ctx, $f))?;)*
        };
    }
    functions! {
        "byte" => str_byte,
        "char" => str_char,
        "find" => str_find,
        "format" => str_format,
        "gmatch" => str_gmatch,
        "gsub" => str_gsub,
        "len" => str_len,
        "lower" => str_lower,
        "match" => str_match,
        "rep" => str_rep,
        "reverse" => str_reverse,
        "sub" => str_sub,
        "upper" => str_upper,
    }

    let metatable = Table::new(&ctx);
    metatable.set_field(ctx, "__index", string)?;
    ctx.set_string_metatable(Some(metatable));

    ctx.set_global("string", string)?;
    Ok(())
}

// Converts a relative initial position (negative counts from the end) to a 1-based position,
// `posrelatI` in Lua 5.4.
fn start_position(pos: i64, len: usize) -> usize {
    let len = len as i128;
    let pos = i128::from(pos);
    let pos = if pos > 0 {
        pos
    } else if pos == 0 || pos < -len {
        1
    } else {
        len + pos + 1
    };
    usize::try_from(pos).unwrap_or(usize::MAX)
}

// Converts a relative end position to a 1-based position in `0..=len`, `getendpos` in Lua 5.4.
fn end_position(pos: i64, len: usize) -> usize {
    let len_i = len as i128;
    let pos = i128::from(pos);
    let pos = if pos > len_i {
        len_i
    } else if pos >= 0 {
        pos
    } else if pos < -len_i {
        0
    } else {
        len_i + pos + 1
    };
    usize::try_from(pos).unwrap_or(0)
}

// Creates a string (which charges for it), failing past the string length or memory limits.
fn new_string<'gc>(ctx: Context<'gc>, bytes: Vec<u8>) -> Result<'gc, String<'gc>> {
    ctx.check_string(bytes.len())?;
    Ok(ctx.new_string(bytes))
}

// Like `new_string`, copying the bytes (once, where a `Vec` would be copied again when interned).
fn copy_string<'gc>(ctx: Context<'gc>, bytes: &[u8]) -> Result<'gc, String<'gc>> {
    ctx.check_string(bytes.len())?;
    Ok(ctx.intern(bytes))
}

// Checks that a string of `len` bytes may be built, and charges fuel for building it.
fn reserve_string<'gc>(ctx: Context<'gc>, fuel: &mut Fuel, len: usize) -> Result<'gc, ()> {
    ctx.check_string(len)?;
    fuel.consume(fuel_for(len, cost::BYTES_PROCESSED));
    Ok(())
}

// Simple functions.

fn str_len<'gc>(
    ctx: Context<'gc>,
    _: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    let s = check_string(ctx, &stack, "len", 0)?;
    stack.replace(ctx, s.len());
    Ok(CallbackReturn::Return)
}

fn str_sub<'gc>(
    ctx: Context<'gc>,
    _: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    let s = check_string(ctx, &stack, "sub", 0)?;
    let bytes = s.as_bytes();
    let start = start_position(check_integer(ctx, &stack, "sub", 1)?, bytes.len());
    let end = end_position(opt_integer(ctx, &stack, "sub", 2, -1)?, bytes.len());
    let sub = if start <= end {
        bytes.get(start - 1..end).unwrap_or_default()
    } else {
        &[]
    };
    let result = if sub.len() == bytes.len() {
        s
    } else {
        copy_string(ctx, sub)?
    };
    stack.replace(ctx, result);
    Ok(CallbackReturn::Return)
}

fn map_bytes<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
    name: &str,
    f: impl FnOnce(&[u8]) -> Vec<u8>,
) -> CallbackResult<'gc> {
    let s = check_string(ctx, &stack, name, 0)?;
    reserve_string(ctx, exec.fuel(), s.as_bytes().len())?;
    let result = new_string(ctx, f(s.as_bytes()))?;
    stack.replace(ctx, result);
    Ok(CallbackReturn::Return)
}

fn str_lower<'gc>(
    ctx: Context<'gc>,
    exec: Execution<'gc, '_>,
    stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    map_bytes(ctx, exec, stack, "lower", |s| s.to_ascii_lowercase())
}

fn str_upper<'gc>(
    ctx: Context<'gc>,
    exec: Execution<'gc, '_>,
    stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    map_bytes(ctx, exec, stack, "upper", |s| s.to_ascii_uppercase())
}

fn str_reverse<'gc>(
    ctx: Context<'gc>,
    exec: Execution<'gc, '_>,
    stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    map_bytes(ctx, exec, stack, "reverse", |s| {
        s.iter().rev().copied().collect()
    })
}

fn str_rep<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    let s = check_string(ctx, &stack, "rep", 0)?;
    let n = check_int(ctx, &stack, "rep", 1)?;
    // Lua 5.1 has no separator.
    let sep = if is_lua51(ctx) || stack.get(2).is_nil() {
        None
    } else {
        Some(check_string(ctx, &stack, "rep", 2)?)
    };
    let sep = sep.map_or(&[][..], |sep| sep.as_bytes());
    let s = s.as_bytes();

    // (An empty string repeated is empty however many times: no loop over `n`.)
    let result = if n <= 0 || (s.is_empty() && sep.is_empty()) {
        Vec::new()
    } else {
        let n = usize::try_from(n).unwrap_or(usize::MAX);
        // As PUC-Rio Lua, a length past what can be allocated is an error, also without limits
        // (where allocating it would panic).
        let len = s
            .len()
            .checked_mul(n)
            .and_then(|l| l.checked_add(sep.len().checked_mul(n - 1)?))
            .filter(|&len| len <= isize::MAX as usize)
            .ok_or_else(|| error("resulting string too large"))?;
        reserve_string(ctx, exec.fuel(), len)?;
        let mut result = Vec::new();
        // Without a memory limit, the allocation itself may fail.
        result
            .try_reserve_exact(len)
            .map_err(|_| error("not enough memory"))?;
        // `s` and `sep` `n - 1` times, doubling what is already there (`log n` copies instead of
        // `n`), then `s`.
        let repeated = len - s.len();
        if repeated > 0 {
            result.extend_from_slice(s);
            result.extend_from_slice(sep);
            while result.len() < repeated {
                let copy = result.len().min(repeated - result.len());
                result.extend_from_within(..copy);
            }
        }
        result.extend_from_slice(s);
        result
    };
    let result = new_string(ctx, result)?;
    stack.replace(ctx, result);
    Ok(CallbackReturn::Return)
}

fn str_byte<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    let s = check_string(ctx, &stack, "byte", 0)?;
    let bytes = s.as_bytes();
    let i = opt_integer(ctx, &stack, "byte", 1, 1)?;
    let (start, end) = if is_lua51(ctx) {
        // In Lua 5.1, the default end is the start after converting it.
        let start = start_position(i, bytes.len());
        let j = if stack.get(2).is_nil() {
            if i < 0 {
                (i + bytes.len() as i64 + 1).max(0)
            } else {
                i
            }
        } else {
            check_integer(ctx, &stack, "byte", 2)?
        };
        (start, end_position(j, bytes.len()))
    } else {
        let j = opt_integer(ctx, &stack, "byte", 2, i)?;
        (start_position(i, bytes.len()), end_position(j, bytes.len()))
    };
    stack.clear();
    if start > end {
        return Ok(CallbackReturn::Return);
    }
    let slice = bytes.get(start - 1..end).unwrap_or_default();
    exec.fuel().consume(fuel_for(slice.len(), cost::VALUES));
    stack
        .try_reserve(ctx, slice.len())
        .map_err(|_| error("string slice too long"))?;
    stack.extend(slice.iter().map(|&b| Value::Integer(i64::from(b))));
    Ok(CallbackReturn::Return)
}

fn str_char<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    let mut bytes = Vec::with_capacity(stack.len());
    ctx.check_string(stack.len())?;
    exec.fuel().consume(fuel_for(stack.len(), cost::VALUES));
    for i in 0..stack.len() {
        let c = check_int(ctx, &stack, "char", i)?;
        let c = u8::try_from(c).map_err(|_| {
            arg_error(
                "char",
                i,
                if is_lua51(ctx) {
                    "invalid value"
                } else {
                    "value out of range"
                },
            )
        })?;
        bytes.push(c);
    }
    let result = new_string(ctx, bytes)?;
    stack.replace(ctx, result);
    Ok(CallbackReturn::Return)
}

// Pattern matching.

// Strips a leading '^' anchor, returning whether the pattern was anchored.
fn strip_anchor(pattern: &[u8]) -> (bool, &[u8]) {
    match pattern.split_first() {
        Some((b'^', rest)) => (true, rest),
        _ => (false, pattern),
    }
}

// Where a pattern ends: Lua 5.1 patterns are C strings, which end at their first zero byte.
// Looking for it scans the pattern, which is charged here, once per operation (the callers keep
// the end and slice with `pattern_bytes`).
fn pattern_end<'gc>(ctx: Context<'gc>, fuel: &mut Fuel, pattern: String<'gc>) -> usize {
    let bytes = pattern.as_bytes();
    if is_lua51(ctx) {
        let end = c_str(bytes).len();
        fuel.consume(fuel_for(end, cost::BYTES_PROCESSED));
        end
    } else {
        bytes.len()
    }
}

// The pattern up to its end found by `pattern_end`.
fn pattern_bytes<'gc>(pattern: String<'gc>, end: usize) -> &'gc [u8] {
    pattern.as_bytes().get(..end).unwrap_or_default()
}

// Pushes the captures of a match from `start` to `end` (or only the captures, if `whole` is false
// and there are none), returning how many were pushed.
fn push_captures<'gc>(
    ctx: Context<'gc>,
    stack: &mut Stack<'gc, '_>,
    matcher: &Matcher<'_>,
    subject: &[u8],
    start: usize,
    end: usize,
    whole: bool,
) -> Result<'gc, usize> {
    let count = matcher.capture_count(whole);
    stack
        .try_reserve(ctx, count)
        .map_err(|_| error("too many captures"))?;
    for i in 0..count {
        let value = capture_value(ctx, matcher, subject, i, start, end)?;
        stack.push_back(value);
    }
    Ok(count)
}

fn capture_value<'gc>(
    ctx: Context<'gc>,
    matcher: &Matcher<'_>,
    subject: &[u8],
    i: usize,
    start: usize,
    end: usize,
) -> Result<'gc, Value<'gc>> {
    Ok(match matcher.capture(i, start, end).map_err(error)? {
        Capture::Bytes { start, len } => {
            let bytes = subject.get(start..start + len).unwrap_or_default();
            Value::String(copy_string(ctx, bytes)?)
        }
        Capture::Position(p) => Value::Integer(p as i64),
    })
}

/// The state of a `string.find` or `string.match` search.
#[derive(Collect)]
#[collect(no_drop)]
struct Search<'gc> {
    subject: String<'gc>,
    pattern: String<'gc>,
    #[collect(require_static)]
    pattern_end: usize,
    // Where to try the next match.
    #[collect(require_static)]
    position: usize,
    #[collect(require_static)]
    find: bool,
    // A `find` without pattern matching.
    #[collect(require_static)]
    plain: bool,
}

// Tries a match at `start`, charging its steps.
fn try_match<'gc>(
    fuel: &mut Fuel,
    matcher: &mut Matcher<'_>,
    start: usize,
) -> Result<'gc, Option<usize>> {
    let steps = matcher.steps();
    let result = matcher.find_at(start);
    fuel.consume(fuel_for(
        (matcher.steps() - steps) as usize,
        cost::MATCH_STEPS,
    ));
    result.map_err(error)
}

impl<'gc> Search<'gc> {
    // Tries matches until one is found (pushing its results), the subject ends (pushing nil), or
    // the fuel runs out (after at least one attempt).
    fn run(
        &mut self,
        ctx: Context<'gc>,
        fuel: &mut Fuel,
        stack: &mut Stack<'gc, '_>,
    ) -> Result<'gc, SequencePoll<'gc>> {
        let subject = self.subject.as_bytes();
        if self.plain {
            let needle = self.pattern.as_bytes();
            let limit = usize::try_from(fuel.remaining())
                .unwrap_or(0)
                .saturating_mul(cost::BYTES_PROCESSED);
            let (result, compared) = pattern::find_plain(subject, needle, self.position, limit);
            fuel.consume(fuel_for(compared, cost::BYTES_PROCESSED));
            match result {
                Plain::Found(start) => {
                    stack.push_back(Value::Integer(start as i64 + 1));
                    stack.push_back(Value::Integer((start + needle.len()) as i64));
                }
                Plain::Missing => stack.push_back(Value::Nil),
                Plain::Paused(position) => {
                    self.position = position;
                    return Ok(SequencePoll::Pending);
                }
            }
            return Ok(SequencePoll::Return);
        }
        let (anchor, pattern) = strip_anchor(pattern_bytes(self.pattern, self.pattern_end));
        let mut matcher = Matcher::new(subject, pattern, is_lua51(ctx), MAX_MATCH_STEPS);
        let mut attempts = 0;
        while self.position <= subject.len() {
            if attempts > 0 && fuel.remaining() <= 0 {
                return Ok(SequencePoll::Pending);
            }
            attempts += 1;
            if let Some(end) = try_match(fuel, &mut matcher, self.position)? {
                let start = self.position;
                if self.find {
                    stack.push_back(Value::Integer(start as i64 + 1));
                    stack.push_back(Value::Integer(end as i64));
                    push_captures(ctx, stack, &matcher, subject, start, end, false)?;
                } else {
                    push_captures(ctx, stack, &matcher, subject, start, end, true)?;
                }
                return Ok(SequencePoll::Return);
            }
            if anchor {
                break;
            }
            self.position += 1;
        }
        stack.push_back(Value::Nil);
        Ok(SequencePoll::Return)
    }
}

impl<'gc> Sequence<'gc> for Search<'gc> {
    fn poll(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        mut exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<'gc, SequencePoll<'gc>> {
        self.run(ctx, exec.fuel(), &mut stack)
    }
}

fn find_aux<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
    find: bool,
) -> CallbackResult<'gc> {
    let name = if find { "find" } else { "match" };
    let subject = check_string(ctx, &stack, name, 0)?;
    let pattern = check_string(ctx, &stack, name, 1)?;
    let len = subject.as_bytes().len();
    let init = opt_integer(ctx, &stack, name, 2, 1)?;
    let init = if is_lua51(ctx) {
        // Lua 5.1 clamps the initial position to the subject.
        let init = if init < 0 {
            (init + len as i64 + 1).max(0)
        } else {
            init
        };
        usize::try_from(init - 1).unwrap_or(0).min(len)
    } else {
        let init = start_position(init, len) - 1;
        if init > len {
            stack.replace(ctx, Value::Nil);
            return Ok(CallbackReturn::Return);
        }
        init
    };
    let plain = stack.get(3).to_bool();
    stack.clear();

    let pattern_end = pattern_end(ctx, exec.fuel(), pattern);
    // Looking for special characters scans the pattern.
    let plain = find
        && (plain || {
            exec.fuel()
                .consume(fuel_for(pattern_end, cost::BYTES_PROCESSED));
            pattern::no_specials(pattern_bytes(pattern, pattern_end))
        });
    let mut search = Search {
        subject,
        pattern,
        pattern_end,
        position: init,
        find,
        // In Lua 5.1, the check for special characters stops at the first zero byte, but a plain
        // search uses the whole pattern.
        plain,
    };
    let poll = search.run(ctx, exec.fuel(), &mut stack)?;
    continue_sequence(ctx, poll, search)
}

fn str_find<'gc>(
    ctx: Context<'gc>,
    exec: Execution<'gc, '_>,
    stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    find_aux(ctx, exec, stack, true)
}

fn str_match<'gc>(
    ctx: Context<'gc>,
    exec: Execution<'gc, '_>,
    stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    find_aux(ctx, exec, stack, false)
}

/// The state of a `string.gmatch` iterator.
#[derive(Collect)]
#[collect(no_drop)]
struct GmatchState<'gc> {
    subject: String<'gc>,
    pattern: String<'gc>,
    #[collect(require_static)]
    pattern_end: usize,
    #[collect(require_static)]
    position: usize,
    // The end of the last match (Lua 5.4 does not match an empty string there).
    #[collect(require_static)]
    last_match: Option<usize>,
    queued: Written<RefLock<GmatchState<'static>>>,
}

type GmatchRef<'gc> = Gc<'gc, RefLock<GmatchState<'gc>>>;

// Finds the next match of a `gmatch` iterator, pushing its captures (or nothing at the end).
fn gmatch_next<'gc>(
    ctx: Context<'gc>,
    fuel: &mut Fuel,
    stack: &mut Stack<'gc, '_>,
    state: GmatchRef<'gc>,
) -> Result<'gc, SequencePoll<'gc>> {
    let mut state = state.borrow_mut(&ctx);
    let subject = state.subject.as_bytes();
    // `gmatch` does not treat '^' as an anchor.
    let pattern = pattern_bytes(state.pattern, state.pattern_end);
    let lua51 = is_lua51(ctx);
    let mut matcher = Matcher::new(subject, pattern, lua51, MAX_MATCH_STEPS);
    let mut attempts = 0;
    while state.position <= subject.len() {
        if attempts > 0 && fuel.remaining() <= 0 {
            return Ok(SequencePoll::Pending);
        }
        attempts += 1;
        let start = state.position;
        match try_match(fuel, &mut matcher, start)? {
            Some(end) if lua51 => {
                // An empty match moves on by at least one position.
                state.position = if end == start { end + 1 } else { end };
                push_captures(ctx, stack, &matcher, subject, start, end, true)?;
                return Ok(SequencePoll::Return);
            }
            Some(end) if Some(end) != state.last_match => {
                state.position = end;
                state.last_match = Some(end);
                push_captures(ctx, stack, &matcher, subject, start, end, true)?;
                return Ok(SequencePoll::Return);
            }
            _ => state.position += 1,
        }
    }
    Ok(SequencePoll::Return)
}

#[derive(Collect)]
#[collect(no_drop)]
struct GmatchSequence<'gc>(GmatchRef<'gc>);

impl<'gc> Sequence<'gc> for GmatchSequence<'gc> {
    fn poll(
        self: Pin<&mut Self>,
        ctx: Context<'gc>,
        mut exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<'gc, SequencePoll<'gc>> {
        gmatch_next(ctx, exec.fuel(), &mut stack, self.0)
    }
}

fn str_gmatch<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    let subject = check_string(ctx, &stack, "gmatch", 0)?;
    let pattern = check_string(ctx, &stack, "gmatch", 1)?;
    let pattern_end = pattern_end(ctx, exec.fuel(), pattern);
    let len = subject.as_bytes().len();
    // Lua 5.1 has no initial position.
    let position = if is_lua51(ctx) {
        0
    } else {
        (start_position(opt_integer(ctx, &stack, "gmatch", 2, 1)?, len) - 1).min(len + 1)
    };
    let state = Gc::new(
        &ctx,
        RefLock::new(GmatchState {
            subject,
            pattern,
            pattern_end,
            position,
            last_match: None,
            queued: Written::new(),
        }),
    );
    let iterator = Callback::from_fn_with(&ctx, state, |&state, ctx, mut exec, mut stack| {
        stack.clear();
        let poll = gmatch_next(ctx, exec.fuel(), &mut stack, state)?;
        continue_sequence(ctx, poll, GmatchSequence(state))
    });
    stack.replace(ctx, iterator);
    Ok(CallbackReturn::Return)
}

/// The state of a `string.gsub` call, which replaces matches until it runs out of fuel or has to
/// call a function (or an `__index` metamethod) for a replacement.
#[derive(Collect)]
#[collect(no_drop)]
struct Gsub<'gc> {
    subject: String<'gc>,
    pattern: String<'gc>,
    #[collect(require_static)]
    pattern_end: usize,
    replacement: Value<'gc>,
    #[collect(require_static)]
    max_replacements: i64,
    #[collect(require_static)]
    replacements: i64,
    #[collect(require_static)]
    position: usize,
    // The subject is copied to the output up to here.
    #[collect(require_static)]
    copied: usize,
    #[collect(require_static)]
    last_match: Option<usize>,
    #[collect(require_static)]
    output: ByteBuffer,
    // The match whose replacement value is being computed by a call.
    #[collect(require_static)]
    pending: Option<(usize, usize)>,
    #[collect(require_static)]
    done: bool,
}

impl<'gc> Gsub<'gc> {
    fn append(&mut self, ctx: Context<'gc>, fuel: &mut Fuel, bytes: &[u8]) -> Result<'gc, ()> {
        // The bytes are charged even if the result is too long (`extend` refuses them without
        // copying); the matcher already processed them, and making the result charges them again.
        fuel.consume(fuel_for(bytes.len(), cost::BYTES_COPIED));
        self.output.extend(ctx, bytes)?;
        Ok(())
    }

    // Copies the subject up to `end` to the output.
    fn copy_to(&mut self, ctx: Context<'gc>, fuel: &mut Fuel, end: usize) -> Result<'gc, ()> {
        let subject = self.subject;
        let bytes = subject.as_bytes().get(self.copied..end).unwrap_or_default();
        self.append(ctx, fuel, bytes)?;
        self.copied = end;
        Ok(())
    }

    // Adds the value computed for the match from `start` to `end` (by a function or a table).
    fn add_value(
        &mut self,
        ctx: Context<'gc>,
        fuel: &mut Fuel,
        value: Value<'gc>,
        start: usize,
        end: usize,
    ) -> Result<'gc, ()> {
        let subject = self.subject.as_bytes();
        match value {
            Value::Nil | Value::Boolean(false) => {
                self.append(ctx, fuel, subject.get(start..end).unwrap_or_default())
            }
            v @ (Value::String(_) | Value::Integer(_) | Value::Number(_)) => {
                let s = v.into_string(ctx).map_or(&[][..], |s| s.as_bytes());
                self.append(ctx, fuel, s)
            }
            v => Err(error(format!(
                "invalid replacement value (a {})",
                v.type_name()
            ))),
        }
    }

    // Adds a replacement string, with its `%` escapes, for the match from `start` to `end`.
    fn add_string(
        &mut self,
        ctx: Context<'gc>,
        fuel: &mut Fuel,
        matcher: &Matcher<'_>,
        replacement: &[u8],
        start: usize,
        end: usize,
    ) -> Result<'gc, ()> {
        // The template is scanned for every match, whatever it expands to.
        fuel.consume(fuel_for(replacement.len(), cost::BYTES_PROCESSED));
        if !replacement.contains(&b'%') {
            return self.append(ctx, fuel, replacement);
        }
        let subject = self.subject.as_bytes();
        let lua51 = is_lua51(ctx);
        let mut bytes = replacement.iter().copied();
        let mut out = Vec::new();
        while let Some(c) = bytes.next() {
            if c != b'%' {
                out.push(c);
                continue;
            }
            match bytes.next() {
                Some(b'%') => out.push(b'%'),
                Some(b'0') => out.extend_from_slice(subject.get(start..end).unwrap_or_default()),
                Some(d @ b'1'..=b'9') => {
                    match matcher
                        .capture(usize::from(d - b'1'), start, end)
                        .map_err(error)?
                    {
                        Capture::Bytes { start, len } => out
                            .extend_from_slice(subject.get(start..start + len).unwrap_or_default()),
                        Capture::Position(p) => {
                            compat::write_integer(&mut out, ctx.compat(), p as i64)
                        }
                    }
                }
                // Lua 5.1 copies any other escaped character, after a final '%' the terminator.
                Some(c) if lua51 => out.push(c),
                None if lua51 => out.push(0),
                _ => {
                    return Err(error("invalid use of '%' in replacement string"));
                }
            }
            if out.len() >= 4096 {
                self.append(ctx, fuel, &out)?;
                out.clear();
            }
        }
        self.append(ctx, fuel, &out)
    }

    // Moves past the match from `start` to `end`, once its replacement was added.
    fn advance(&mut self, ctx: Context<'gc>, start: usize, end: usize) {
        self.copied = end;
        if is_lua51(ctx) {
            if end > start {
                self.position = end;
            } else if start < self.subject.as_bytes().len() {
                // After an empty match, Lua 5.1 keeps the next character.
                self.position = start + 1;
            } else {
                self.done = true;
            }
        } else {
            self.position = end;
            self.last_match = Some(end);
        }
    }

    // Runs until the subject is done, the fuel runs out or a call is needed.
    fn run(
        &mut self,
        ctx: Context<'gc>,
        fuel: &mut Fuel,
        stack: &mut Stack<'gc, '_>,
    ) -> Result<'gc, SequencePoll<'gc>> {
        let subject = self.subject.as_bytes();
        let (anchor, pattern) = strip_anchor(pattern_bytes(self.pattern, self.pattern_end));

        if let Some((start, end)) = self.pending.take() {
            let value = stack.get(0);
            stack.clear();
            self.add_value(ctx, fuel, value, start, end)?;
            self.advance(ctx, start, end);
            if anchor {
                self.done = true;
            }
        }

        let lua51 = is_lua51(ctx);
        let mut matcher = Matcher::new(subject, pattern, lua51, MAX_MATCH_STEPS);
        while !self.done && self.replacements < self.max_replacements {
            if fuel.remaining() <= 0 {
                return Ok(SequencePoll::Pending);
            }
            let start = self.position;
            let matched = match try_match(fuel, &mut matcher, start)? {
                Some(end) if lua51 || Some(end) != self.last_match => Some(end),
                _ => None,
            };

            if let Some(end) = matched {
                self.replacements += 1;
                self.copy_to(ctx, fuel, start)?;
                match self.replacement {
                    Value::Table(table) => {
                        let key = capture_value(ctx, &matcher, subject, 0, start, end)?;
                        match meta_ops::index(ctx, Value::Table(table), key)? {
                            MetaResult::Value(v) => self.add_value(ctx, fuel, v, start, end)?,
                            MetaResult::Call(call) => {
                                self.pending = Some((start, end));
                                stack.clear();
                                stack.extend(call.args);
                                return Ok(SequencePoll::Call {
                                    function: call.function,
                                    bottom: 0,
                                });
                            }
                        }
                    }
                    Value::Function(function) => {
                        stack.clear();
                        push_captures(ctx, stack, &matcher, subject, start, end, true)?;
                        self.pending = Some((start, end));
                        return Ok(SequencePoll::Call {
                            function,
                            bottom: 0,
                        });
                    }
                    replacement => {
                        let replacement = replacement.into_string(ctx);
                        let replacement = replacement.map_or(&[][..], |s| s.as_bytes());
                        self.add_string(ctx, fuel, &matcher, replacement, start, end)?;
                    }
                }
                self.advance(ctx, start, end);
            } else if start < subject.len() {
                // Otherwise, keep one character.
                self.position = start + 1;
            } else {
                self.done = true;
            }
            if anchor {
                self.done = true;
            }
        }

        let result = if self.replacements == 0 {
            // Nothing was copied to the output: the subject is the result.
            self.subject
        } else {
            // Add the rest of the subject.
            self.copy_to(ctx, fuel, subject.len())?;
            ctx.new_string(self.output.take())
        };
        stack.replace(ctx, (result, self.replacements));
        Ok(SequencePoll::Return)
    }
}

impl<'gc> Sequence<'gc> for Gsub<'gc> {
    fn poll(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        mut exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<'gc, SequencePoll<'gc>> {
        self.run(ctx, exec.fuel(), &mut stack)
    }
}

fn str_gsub<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    let subject = check_string(ctx, &stack, "gsub", 0)?;
    let pattern = check_string(ctx, &stack, "gsub", 1)?;
    let replacement = stack.get(2);
    if !matches!(
        replacement,
        Value::String(_)
            | Value::Integer(_)
            | Value::Number(_)
            | Value::Table(_)
            | Value::Function(_)
    ) {
        return Err(type_error(&stack, "gsub", 2, "string/function/table"));
    }
    let len = subject.as_bytes().len();
    let max_replacements = opt_int(ctx, &stack, "gsub", 3)?.unwrap_or(len as i64 + 1);
    stack.clear();

    let mut gsub = Gsub {
        subject,
        pattern,
        pattern_end: pattern_end(ctx, exec.fuel(), pattern),
        replacement,
        max_replacements,
        replacements: 0,
        position: 0,
        copied: 0,
        last_match: None,
        output: ByteBuffer::new(),
        pending: None,
        done: false,
    };
    let poll = gsub.run(ctx, exec.fuel(), &mut stack)?;
    continue_sequence(ctx, poll, gsub)
}

// Formatting.

fn str_format<'gc>(
    ctx: Context<'gc>,
    exec: Execution<'gc, '_>,
    stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    let format = check_string(ctx, &stack, "format", 0)?;
    if is_lua51(ctx) {
        return format_all(ctx, exec, stack);
    }

    // Lua 5.4 converts the arguments of `%s` with `tostring`, which may call `__tostring`: convert
    // those first, in order.
    #[derive(Collect)]
    #[collect(no_drop)]
    struct ToStringArgs<'gc> {
        format: String<'gc>,
        // Where to continue looking for `%s` in the format string.
        #[collect(require_static)]
        position: usize,
        // The argument of the last directive seen.
        #[collect(require_static)]
        arg: usize,
        // The argument whose `tostring` is being called.
        #[collect(require_static)]
        pending: Option<usize>,
        #[collect(require_static)]
        top: usize,
    }

    impl<'gc> ToStringArgs<'gc> {
        // The next argument of a `%s` directive that needs `tostring`.
        fn next_arg(&mut self, stack: &Stack<'gc, '_>) -> Option<usize> {
            let mut bytes = self
                .format
                .as_bytes()
                .get(self.position..)
                .unwrap_or_default();
            let mut found = None;
            while let Some(i) = bytes.iter().position(|&c| c == b'%') {
                bytes = bytes.get(i + 1..).unwrap_or_default();
                if bytes.first() == Some(&b'%') {
                    bytes = bytes.get(1..).unwrap_or_default();
                    continue;
                }
                self.arg += 1;
                let Ok((spec, len)) = format::parse_spec_54(bytes) else {
                    // `format_all` reports the error.
                    bytes = &[];
                    break;
                };
                bytes = bytes.get(len..).unwrap_or_default();
                if spec.conversion == b's'
                    && matches!(stack.get(self.arg), Value::Table(_) | Value::UserData(_))
                {
                    found = Some(self.arg);
                    break;
                }
            }
            self.position = self.format.as_bytes().len() - bytes.len();
            found
        }
    }

    impl<'gc> Sequence<'gc> for ToStringArgs<'gc> {
        fn poll(
            mut self: Pin<&mut Self>,
            ctx: Context<'gc>,
            mut exec: Execution<'gc, '_>,
            mut stack: Stack<'gc, '_>,
        ) -> Result<'gc, SequencePoll<'gc>> {
            let top = self.top;
            if let Some(arg) = self.pending.take() {
                let value = stack.get(top);
                stack.resize(top);
                if let Some(slot) = stack.as_mut_slice().get_mut(arg) {
                    *slot = value;
                }
            }
            loop {
                let start = self.position;
                let next = self.next_arg(&stack);
                exec.fuel().consume(fuel_for(
                    self.position.saturating_sub(start),
                    cost::BYTES_PROCESSED,
                ));
                let Some(arg) = next else {
                    break;
                };
                match meta_ops::tostring(ctx, stack.get(arg))? {
                    MetaResult::Value(v) => {
                        if let Some(slot) = stack.as_mut_slice().get_mut(arg) {
                            *slot = v;
                        }
                    }
                    MetaResult::Call(call) => {
                        self.pending = Some(arg);
                        stack.resize(top);
                        stack.extend(call.args);
                        return Ok(SequencePoll::Call {
                            function: call.function,
                            bottom: top,
                        });
                    }
                }
            }
            format_all(ctx, exec, stack)?;
            Ok(SequencePoll::Return)
        }
    }

    // Without a table or userdata argument, no `%s` needs `tostring`.
    if !stack
        .as_slice()
        .iter()
        .skip(1)
        .any(|v| matches!(v, Value::Table(_) | Value::UserData(_)))
    {
        return format_all(ctx, exec, stack);
    }
    let mut seq = ToStringArgs {
        format,
        position: 0,
        arg: 0,
        pending: None,
        top: stack.len(),
    };
    // Without any argument that needs `tostring`, format right away.
    if seq.next_arg(&stack).is_none() {
        return format_all(ctx, exec, stack);
    }
    seq.position = 0;
    seq.arg = 0;
    Ok(CallbackReturn::Sequence(BoxSequence::new(&ctx, seq)))
}

// Formats the arguments on the stack (the format string first), once any `tostring` conversion
// they need is done.
fn format_all<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> CallbackResult<'gc> {
    let lua51 = is_lua51(ctx);
    let format = check_string(ctx, &stack, "format", 0)?;
    let mut bytes = format.as_bytes();
    let mut out = Vec::new();
    let mut arg = 0;
    let fuel = exec.fuel();

    // Checks the length of the output before `added` bytes are written to it, charging for them.
    let check_len = |ctx: Context<'gc>, fuel: &mut Fuel, out: &Vec<u8>, added: usize| {
        ctx.check_string(out.len().saturating_add(added))?;
        fuel.consume(fuel_for(added, cost::BYTES_PROCESSED));
        Ok::<(), Error<'gc>>(())
    };

    while let Some(i) = bytes.iter().position(|&c| c == b'%') {
        let (literal, rest) = bytes.split_at(i);
        check_len(ctx, fuel, &out, literal.len())?;
        out.extend_from_slice(literal);
        bytes = rest.get(1..).unwrap_or_default();
        if bytes.first() == Some(&b'%') {
            out.push(b'%');
            bytes = bytes.get(1..).unwrap_or_default();
            continue;
        }

        arg += 1;
        if arg >= stack.len() {
            return Err(arg_error("format", arg, "no value"));
        }
        let (spec, len) = if lua51 {
            format::parse_spec_51(bytes)
        } else {
            format::parse_spec_54(bytes)
        }
        .map_err(error)?;
        bytes = bytes.get(len..).unwrap_or_default();

        // `%q` of a string writes up to four bytes for each of its bytes: find the length it
        // writes (scanning the string) and check it before writing.
        let mut checked = false;
        if let (b'q', Value::String(s)) = (spec.conversion, stack.get(arg)) {
            fuel.consume(fuel_for(s.as_bytes().len(), cost::BYTES_PROCESSED));
            let quoted = if lua51 {
                format::quoted_len_51(s.as_bytes())
            } else {
                format::quoted_len_54(s.as_bytes())
            };
            check_len(ctx, fuel, &out, quoted)?;
            checked = true;
        }

        // Any other item is at most a few hundred bytes, or an existing string.
        let start = out.len();
        format_one(ctx, &stack, &mut out, &spec, arg, lua51)?;
        if !checked {
            ctx.check_string(out.len())?;
            fuel.consume(fuel_for(out.len() - start, cost::BYTES_PROCESSED));
        }
    }
    check_len(ctx, fuel, &out, bytes.len())?;
    out.extend_from_slice(bytes);

    let result = ctx.new_string(out);
    stack.replace(ctx, result);
    Ok(CallbackReturn::Return)
}

fn format_one<'gc>(
    ctx: Context<'gc>,
    stack: &Stack<'gc, '_>,
    out: &mut Vec<u8>,
    spec: &Spec,
    arg: usize,
    lua51: bool,
) -> Result<'gc, ()> {
    match spec.conversion {
        b'c' => {
            let c = if lua51 {
                check_number(ctx, stack, "format", arg)? as i32
            } else {
                check_integer(ctx, stack, "format", arg)? as i32
            };
            let start = out.len();
            format::format_char(out, spec, c as u8);
            if lua51 {
                // Lua 5.1 measures `sprintf`'s output with `strlen`.
                if let Some(end) = out
                    .get(start..)
                    .and_then(|s| s.iter().position(|&b| b == 0))
                {
                    out.truncate(start + end);
                }
            }
        }
        b'd' | b'i' => {
            let n = if lua51 {
                check_number(ctx, stack, "format", arg)? as i64
            } else {
                check_integer(ctx, stack, "format", arg)?
            };
            format::format_signed(out, spec, n);
        }
        b'o' | b'u' | b'x' | b'X' => {
            let n = if lua51 {
                check_number(ctx, stack, "format", arg)? as u64
            } else {
                check_integer(ctx, stack, "format", arg)? as u64
            };
            format::format_unsigned(out, spec, n);
        }
        b'e' | b'E' | b'f' | b'g' | b'G' => {
            let n = check_number(ctx, stack, "format", arg)?;
            format::format_double(out, spec, n);
        }
        b'a' | b'A' if !lua51 => {
            let n = check_number(ctx, stack, "format", arg)?;
            format::format_hex_double(out, spec, n);
        }
        b'p' if !lua51 => {
            let s = match stack.get(arg) {
                Value::String(s) => format!("{:p}", s.as_bytes().as_ptr()),
                Value::Table(t) => format!("{:p}", Gc::as_ptr(t.into_inner())),
                Value::Function(Function::Closure(c)) => {
                    format!("{:p}", Gc::as_ptr(c.into_inner()))
                }
                Value::Function(Function::Callback(c)) => {
                    format!("{:p}", Gc::as_ptr(c.into_inner()))
                }
                Value::Thread(t) => format!("{:p}", Gc::as_ptr(t.into_inner())),
                Value::UserData(u) => format!("{:p}", Gc::as_ptr(u.into_inner())),
                _ => "(null)".to_owned(),
            };
            format::format_bytes(out, spec, s.as_bytes());
        }
        b'q' => {
            if lua51 {
                let s = check_string(ctx, stack, "format", arg)?;
                format::quote_51(out, s.as_bytes());
            } else {
                match stack.get(arg) {
                    Value::String(s) => format::quote_54(out, s.as_bytes()),
                    Value::Integer(i) => {
                        if i == i64::MIN {
                            out.extend_from_slice(b"0x8000000000000000");
                        } else {
                            out.extend_from_slice(i.to_string().as_bytes());
                        }
                    }
                    Value::Number(n) => {
                        if n == f64::INFINITY {
                            out.extend_from_slice(b"1e9999");
                        } else if n == f64::NEG_INFINITY {
                            out.extend_from_slice(b"-1e9999");
                        } else if n.is_nan() {
                            out.extend_from_slice(b"(0/0)");
                        } else {
                            format::format_hex_double(
                                out,
                                &Spec {
                                    conversion: b'a',
                                    ..Spec::default()
                                },
                                n,
                            );
                        }
                    }
                    v @ (Value::Nil | Value::Boolean(_)) => {
                        out.extend_from_slice(v.display().to_string().as_bytes())
                    }
                    _ => return Err(arg_error("format", arg, "value has no literal form")),
                }
            }
        }
        b's' => {
            let s = if lua51 {
                check_string(ctx, stack, "format", arg)?
            } else {
                match stack.get(arg) {
                    v @ (Value::String(_) | Value::Integer(_) | Value::Number(_)) => {
                        v.into_string(ctx).unwrap_or_else(|| ctx.intern(b""))
                    }
                    // Tables and userdata were converted with `tostring` already.
                    // DELIBERATE(puc): any `__tostring` result is taken (5.4 requires a string;
                    // see COMPATIBILITY.md).
                    v => ctx.intern(v.display().to_string().as_bytes()),
                }
            };
            let bytes = s.as_bytes();
            // DELIBERATE(cost): this scan and `c_str` below are uncharged (see `fuel::cost`, known and
            // accepted: about 92 fuel per ms on a 1 MiB string, bounded by
            // the host's deadline).
            if !lua51 && spec.modifiers && bytes.contains(&0) {
                return Err(arg_error("format", arg, "string contains zeros"));
            }
            // Lua 5.4 keeps a string formatted without modifiers; 5.1 only a long one.
            if (!lua51 && !spec.modifiers) || (spec.precision.is_none() && bytes.len() >= 100) {
                // Kept as is.
                out.extend_from_slice(bytes);
            } else {
                // Formatted as a C string, which ends at its first zero byte.
                let bytes = c_str(bytes);
                format::format_bytes(out, spec, bytes);
            }
        }
        c => {
            return Err(error(if lua51 {
                FormatError::InvalidOption(char::from(c))
            } else {
                FormatError::InvalidConversion(char::from(c).to_string())
            }));
        }
    }
    Ok(())
}
