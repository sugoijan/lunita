use std::mem;
use std::pin::Pin;

use gc_arena::Collect;

use super::{
    args::{arg_error, check_int, check_string, error, is_lua51, opt_int, type_error},
    continue_sequence,
};
use crate::{
    BoxSequence, Callback, CallbackReturn, Closure, Context, Error, Execution, Fuel, Function,
    IntoValue, LimitError, MetaMethod, Sequence, SequencePoll, Stack, Table, Value,
    fuel::{cost, count_fuel, fuel_for},
    limits::ByteBuffer,
    meta_ops::{self, MetaResult},
    table::{Growth, RawTable},
    thread::MAX_STACK_SIZE,
};

pub fn load_table<'gc>(ctx: Context<'gc>) -> Result<(), LimitError> {
    let table = Table::new(&ctx);

    table.set_field(
        ctx,
        "pack",
        Callback::from_fn(&ctx, |ctx, _, stack| {
            Ok(CallbackReturn::Sequence(BoxSequence::new(
                &ctx,
                Pack::SetLength {
                    table: Table::new(&ctx).into(),
                    length: stack.len(),
                },
            )))
        }),
    )?;

    let unpack: Function<'gc> = Callback::from_fn(&ctx, |ctx, _, mut stack| {
        let (table, start_arg, end_arg): (Value<'gc>, Option<i64>, Option<i64>) =
            stack.consume(ctx)?;

        let start = start_arg.unwrap_or(1);
        let seq = if let Some(end) = end_arg {
            if start > end {
                return Ok(CallbackReturn::Return);
            }

            let length = try_compute_length(start, end)
                .filter(|&length| length < MAX_STACK_SIZE)
                .ok_or_else(|| error("too many results to unpack"))?;
            Unpack::MainLoop {
                start,
                table,
                length,
                index: 0,
                batch_end: 0,
                callback_return: false,
            }
        } else {
            Unpack::FindLength { start, table }
        };

        Ok(CallbackReturn::Sequence(BoxSequence::new(&ctx, seq)))
    })
    .into();

    table.set_field(ctx, "unpack", unpack)?;

    table.set_field(ctx, "concat", Callback::from_fn(&ctx, table_concat))?;
    table.set_field(ctx, "remove", Callback::from_fn(&ctx, table_remove))?;
    table.set_field(ctx, "insert", Callback::from_fn(&ctx, table_insert))?;

    // These sources are part of the library and always compile, unless the instance is already
    // past its memory limit.
    let data = include_str!("table/sort.lua");
    if let Ok(func) = Closure::load(ctx, Some("table/sort.lua"), data.as_bytes()) {
        table.set_field(ctx, "sort", func)?;
    }

    let data = include_str!("table/move.lua");
    if let Ok(func) = Closure::load(ctx, Some("table/move.lua"), data.as_bytes()) {
        table.set_field(ctx, "move", func)?;
    }

    ctx.set_global("table", table)?;
    Ok(())
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Collect)]
#[collect(require_static)]
enum OpState {
    // Not started.
    Start,
    // Waiting for the result of `__len`.
    Length,
    // Waiting for the result of `__index` for the removed value.
    Removed,
    // Moving elements.
    Moving,
    // Waiting for the result of `__index` to move it.
    Got,
    // Waiting for `__newindex` to move an element.
    Set,
    // Setting the last element (the inserted value or nil).
    Last,
    // Waiting for `__newindex` to set the last element.
    LastSet,
    // Concatenating.
    Concat,
    // Waiting for the result of `__index` to concatenate it.
    ConcatGot,
}

/// `table.insert`, `table.remove` and `table.concat` with any number of elements: element by
/// element, with metamethods (unless `raw`), suspending when the fuel runs out or a metamethod has
/// to be called.
#[derive(Collect)]
#[collect(no_drop)]
struct TableOp<'gc> {
    table: Table<'gc>,
    #[collect(require_static)]
    kind: OpKind,
    #[collect(require_static)]
    raw: bool,
    #[collect(require_static)]
    state: OpState,
    // The position argument.
    #[collect(require_static)]
    position: Option<i64>,
    // The value to insert, or the removed value.
    value: Value<'gc>,
    // Moves `t[i] = t[i + step]` for `i` from `next` until `end`, then sets `t[end] = value`
    // (insert) or nil (remove).
    #[collect(require_static)]
    next: i64,
    #[collect(require_static)]
    end: i64,
    #[collect(require_static)]
    step: i64,
    // Where the arguments of a metamethod call start on the stack (and its results after it).
    #[collect(require_static)]
    base: usize,
    // For concat: the separator, and the result so far.
    separator: Option<crate::String<'gc>>,
    #[collect(require_static)]
    output: ByteBuffer,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum OpKind {
    Insert,
    Remove,
    Concat,
}

enum Access<'gc> {
    Value(Value<'gc>),
    Call(SequencePoll<'gc>),
}

impl<'gc> TableOp<'gc> {
    fn new(table: Table<'gc>, kind: OpKind, raw: bool) -> Self {
        TableOp {
            table,
            kind,
            raw,
            state: OpState::Start,
            position: None,
            value: Value::Nil,
            next: 0,
            end: 0,
            step: 1,
            base: 0,
            separator: None,
            output: ByteBuffer::new(),
        }
    }

    // Starts with the length of the table, computing it raw or with `__len`.
    fn start(
        &mut self,
        ctx: Context<'gc>,
        stack: &mut Stack<'gc, '_>,
        fuel: &mut Fuel,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        // `concat` up to a given position needs no length, but Lua 5.4 calls `__len` anyway.
        if self.kind == OpKind::Concat && self.position.is_some() && self.raw {
            return self.with_length(ctx, stack, fuel, 0);
        }
        if self.raw {
            let length = self.table.length_charged(ctx);
            return self.with_length(ctx, stack, fuel, length);
        }
        match meta_ops::len(ctx, Value::Table(self.table))? {
            MetaResult::Value(v) => {
                let length = v
                    .to_integer()
                    .ok_or_else(|| error("object length is not an integer"))?;
                self.with_length(ctx, stack, fuel, length)
            }
            MetaResult::Call(call) => {
                self.state = OpState::Length;
                stack.clear();
                stack.extend(call.args);
                Ok(SequencePoll::Call {
                    function: call.function,
                    bottom: 0,
                })
            }
        }
    }

    // Sets up the moves once the length of the table is known.
    fn with_length(
        &mut self,
        ctx: Context<'gc>,
        stack: &mut Stack<'gc, '_>,
        fuel: &mut Fuel,
        length: i64,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        let lua51 = is_lua51(ctx);
        match self.kind {
            OpKind::Insert => {
                // DELIBERATE(puc): PUC-Rio Lua wraps around here; lunita errors (see COMPATIBILITY.md).
                let Some(mut e) = length.checked_add(1) else {
                    return Err(error("table is too long to insert into"));
                };
                let position = match self.position {
                    None => e,
                    Some(position) => {
                        if lua51 {
                            // Lua 5.1 grows the table to the position.
                            if position > e {
                                e = position;
                            }
                        } else if (position as u64).wrapping_sub(1) >= e as u64 {
                            return Err(arg_error("insert", 1, "position out of bounds"));
                        }
                        position
                    }
                };
                // `t[i] = t[i - 1]` for `i` from `e` down to `position + 1`.
                self.next = e;
                self.end = position;
                self.step = -1;
            }
            OpKind::Remove => {
                let position = self.position.unwrap_or(length);
                if lua51 {
                    if !(1 <= position && position <= length) {
                        // Nothing to remove.
                        stack.clear();
                        return Ok(SequencePoll::Return);
                    }
                } else if position != length
                    && ((position as u64).wrapping_sub(1) > length as u64
                        // With a length of `maxinteger`, PUC-Rio Lua accepts `mininteger` and
                        // then moves 2^64 elements.
                        || position < 0)
                {
                    return Err(arg_error("remove", 1, "position out of bounds"));
                }
                // The removed value, then `t[i] = t[i + 1]` for `i` from `position` to the
                // length, then `t[i] = nil`.
                self.next = position;
                self.end = length.max(position);
                self.step = 1;
                match self.get(ctx, stack, position)? {
                    Access::Value(v) => self.value = v,
                    Access::Call(poll) => {
                        self.state = OpState::Removed;
                        return Ok(poll);
                    }
                }
            }
            OpKind::Concat => {
                let last = self.position.unwrap_or(length);
                self.end = last;
                self.state = OpState::Concat;
                return self.run(ctx, stack, fuel);
            }
        }
        self.state = OpState::Moving;
        self.run(ctx, stack, fuel)
    }

    fn get(
        &mut self,
        ctx: Context<'gc>,
        stack: &mut Stack<'gc, '_>,
        key: i64,
    ) -> Result<Access<'gc>, Error<'gc>> {
        if self.raw {
            return Ok(Access::Value(self.table.get_raw(Value::Integer(key))));
        }
        Ok(
            match meta_ops::index(ctx, Value::Table(self.table), Value::Integer(key))? {
                MetaResult::Value(v) => Access::Value(v),
                MetaResult::Call(call) => {
                    self.base = stack.len();
                    stack.extend(call.args);
                    Access::Call(SequencePoll::Call {
                        function: call.function,
                        bottom: self.base,
                    })
                }
            },
        )
    }

    fn set(
        &mut self,
        ctx: Context<'gc>,
        stack: &mut Stack<'gc, '_>,
        key: i64,
        value: Value<'gc>,
    ) -> Result<Option<SequencePoll<'gc>>, Error<'gc>> {
        if self.raw {
            self.table.set(ctx, key, value)?;
            return Ok(None);
        }
        Ok(
            match meta_ops::new_index(ctx, Value::Table(self.table), Value::Integer(key), value)? {
                None => None,
                Some(call) => {
                    self.base = stack.len();
                    stack.extend(call.args);
                    Some(SequencePoll::Call {
                        function: call.function,
                        bottom: self.base,
                    })
                }
            },
        )
    }

    // Continues until done, out of fuel, or calling a metamethod.
    fn run(
        &mut self,
        ctx: Context<'gc>,
        stack: &mut Stack<'gc, '_>,
        fuel: &mut Fuel,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        loop {
            match self.state {
                OpState::Start => return self.start(ctx, stack, fuel),
                OpState::Length => {
                    let length = stack.get(0);
                    stack.clear();
                    let length = length
                        .to_integer()
                        .ok_or_else(|| error("object length is not an integer"))?;
                    return self.with_length(ctx, stack, fuel, length);
                }
                OpState::Moving => {
                    if self.next == self.end {
                        self.state = OpState::Last;
                        continue;
                    }
                    if !fuel.should_continue() {
                        return Ok(SequencePoll::Pending);
                    }
                    fuel.consume(FUEL_PER_SHIFTED_ITEM);
                    let source = self.next.wrapping_add(self.step);
                    match self.get(ctx, stack, source)? {
                        Access::Value(v) => {
                            if let Some(poll) = self.set(ctx, stack, self.next, v)? {
                                self.state = OpState::Set;
                                return Ok(poll);
                            }
                            self.next = source;
                        }
                        Access::Call(poll) => {
                            self.state = OpState::Got;
                            return Ok(poll);
                        }
                    }
                }
                OpState::Removed => {
                    self.value = stack.get(self.base);
                    stack.resize(self.base);
                    self.state = OpState::Moving;
                }
                OpState::Got => {
                    let v = stack.get(self.base);
                    stack.resize(self.base);
                    self.state = OpState::Moving;
                    if let Some(poll) = self.set(ctx, stack, self.next, v)? {
                        self.state = OpState::Set;
                        return Ok(poll);
                    }
                    self.next = self.next.wrapping_add(self.step);
                }
                OpState::Set => {
                    stack.clear();
                    self.next = self.next.wrapping_add(self.step);
                    self.state = OpState::Moving;
                }
                OpState::Last => {
                    let value = if self.kind == OpKind::Insert {
                        self.value
                    } else {
                        Value::Nil
                    };
                    self.state = OpState::LastSet;
                    if let Some(poll) = self.set(ctx, stack, self.end, value)? {
                        return Ok(poll);
                    }
                }
                OpState::LastSet => {
                    stack.clear();
                    if self.kind == OpKind::Remove {
                        stack.push_back(self.value);
                    }
                    return Ok(SequencePoll::Return);
                }
                OpState::Concat | OpState::ConcatGot => {
                    if self.state == OpState::ConcatGot {
                        let v = stack.get(self.base);
                        stack.resize(self.base);
                        self.add(ctx, fuel, v)?;
                        self.state = OpState::Concat;
                    }
                    while self.next <= self.end {
                        if !fuel.should_continue() {
                            return Ok(SequencePoll::Pending);
                        }
                        match self.get(ctx, stack, self.next)? {
                            Access::Value(v) => self.add(ctx, fuel, v)?,
                            Access::Call(poll) => {
                                self.state = OpState::ConcatGot;
                                return Ok(poll);
                            }
                        }
                    }
                    let output = self.output.take();
                    stack.replace(ctx, ctx.new_string(output));
                    return Ok(SequencePoll::Return);
                }
            }
        }
    }

    // Adds the element at `self.next` to the result of `concat`.
    fn add(
        &mut self,
        ctx: Context<'gc>,
        fuel: &mut Fuel,
        value: Value<'gc>,
    ) -> Result<(), Error<'gc>> {
        let Some(s) = value.into_string(ctx) else {
            return Err(error(format!(
                "invalid value ({}) at index {} in table for 'concat'",
                value.type_name(),
                self.next
            )));
        };
        let separator = match self.separator {
            Some(separator) if self.next < self.end => separator.as_bytes(),
            _ => &[],
        };
        // The element is read (1 fuel) and copied; making the result processes the bytes.
        let added = s.as_bytes().len() + separator.len();
        fuel.consume(fuel_for(added, cost::BYTES_COPIED).saturating_add(1));
        self.output.extend(ctx, s.as_bytes())?;
        self.output.extend(ctx, separator)?;
        self.advance();
        Ok(())
    }

    // Moves `concat` on to the next element, ending after `i64::MAX`.
    fn advance(&mut self) {
        match self.next.checked_add(1) {
            Some(next) => self.next = next,
            None => self.end = i64::MAX - 1,
        }
    }
}

impl<'gc> Sequence<'gc> for TableOp<'gc> {
    fn poll(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        mut exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        self.run(ctx, &mut stack, exec.fuel())
    }
}

// Whether a table is accessed raw: always in Lua 5.1, and in Lua 5.4 if it has no metamethods for
// these operations.
fn is_raw<'gc>(ctx: Context<'gc>, table: Table<'gc>) -> bool {
    is_lua51(ctx)
        || table.metatable().is_none_or(|mt| {
            mt.get_value(ctx, MetaMethod::Len).is_nil()
                && mt.get_value(ctx, MetaMethod::Index).is_nil()
                && mt.get_value(ctx, MetaMethod::NewIndex).is_nil()
        })
}

// Runs a table operation in the callback, returning it as a sequence if it has to suspend.
fn run_op<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
    mut op: TableOp<'gc>,
) -> Result<CallbackReturn<'gc>, Error<'gc>> {
    let poll = op.run(ctx, &mut stack, exec.fuel())?;
    continue_sequence(ctx, poll, op)
}

fn check_table<'gc>(stack: &Stack<'gc, '_>, name: &str) -> Result<Table<'gc>, Error<'gc>> {
    match stack.get(0) {
        Value::Table(t) => Ok(t),
        _ => Err(type_error(stack, name, 0, "table")),
    }
}

fn table_insert<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> Result<CallbackReturn<'gc>, Error<'gc>> {
    let table = check_table(&stack, "insert")?;
    let (position, value) = match stack.len() {
        2 => (None, stack.get(1)),
        3 => (Some(check_int(ctx, &stack, "insert", 1)?), stack.get(2)),
        _ => return Err(error("wrong number of arguments to 'insert'")),
    };
    stack.clear();
    let raw = is_raw(ctx, table);

    if raw {
        // Try the fast path in the array part.
        let fast = array_insert_shift(
            &mut table.into_inner().borrow_mut(&ctx).raw_table,
            position,
            value,
            |growth| ctx.check_growth(growth),
        );
        exec.fuel().consume(fuel_for(fast.2, cost::TABLE_LOOKUPS));
        match fast {
            (RawArrayOpResult::Success(()), len, _) => {
                let shifted =
                    len.saturating_sub(position.map_or(len, |p| p.saturating_sub(1) as usize));
                exec.fuel()
                    .consume(count_fuel(FUEL_PER_SHIFTED_ITEM, shifted));
                return Ok(CallbackReturn::Return);
            }
            (RawArrayOpResult::Limit(err), _, _) => return Err(err.into()),
            _ => {}
        }
    }

    let mut op = TableOp::new(table, OpKind::Insert, raw);
    op.position = position;
    op.value = value;
    run_op(ctx, exec, stack, op)
}

fn table_remove<'gc>(
    ctx: Context<'gc>,
    mut exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> Result<CallbackReturn<'gc>, Error<'gc>> {
    let table = check_table(&stack, "remove")?;
    let position = opt_int(ctx, &stack, "remove", 1)?;
    stack.clear();
    let raw = is_raw(ctx, table);

    if raw {
        // Try the fast path in the array part.
        let fast = array_remove_shift(&mut table.into_inner().borrow_mut(&ctx).raw_table, position);
        exec.fuel().consume(fuel_for(fast.2, cost::TABLE_LOOKUPS));
        if let (RawArrayOpResult::Success(value), len, _) = fast {
            if is_lua51(ctx) && !(1..=len as i64).contains(&position.unwrap_or(len as i64)) {
                // Lua 5.1 removes nothing outside of `1..=#t`.
                return Ok(CallbackReturn::Return);
            }
            let shifted =
                len.saturating_sub(position.map_or(len, |p| p.saturating_sub(1) as usize));
            exec.fuel()
                .consume(count_fuel(FUEL_PER_SHIFTED_ITEM, shifted));
            stack.push_back(value);
            return Ok(CallbackReturn::Return);
        }
    }

    let mut op = TableOp::new(table, OpKind::Remove, raw);
    op.position = position;
    run_op(ctx, exec, stack, op)
}

fn table_concat<'gc>(
    ctx: Context<'gc>,
    exec: Execution<'gc, '_>,
    mut stack: Stack<'gc, '_>,
) -> Result<CallbackReturn<'gc>, Error<'gc>> {
    let table = check_table(&stack, "concat")?;
    let separator = match stack.get(1) {
        Value::Nil => None,
        _ => Some(check_string(ctx, &stack, "concat", 1)?),
    };
    let first = opt_int(ctx, &stack, "concat", 2)?.unwrap_or(1);
    let last = opt_int(ctx, &stack, "concat", 3)?;
    stack.clear();

    let mut op = TableOp::new(table, OpKind::Concat, is_raw(ctx, table));
    op.separator = separator;
    op.next = first;
    op.position = last;
    run_op(ctx, exec, stack, op)
}

const FUEL_PER_SHIFTED_ITEM: i32 = 1;

const PACK_MIN_BATCH_SIZE: usize = 4096;

#[derive(Collect)]
#[collect(no_drop)]
enum Pack<'gc> {
    SetLength {
        table: Value<'gc>,
        length: usize,
    },
    MainLoop {
        table: Value<'gc>,
        length: usize,
        index: usize,
        batch_end: usize,
    },
}

impl<'gc> Sequence<'gc> for Pack<'gc> {
    fn poll(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        mut exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        if let Pack::SetLength { table, length } = *self {
            *self = Pack::MainLoop {
                table,
                length,
                index: 0,
                batch_end: 0,
            };

            if let Some(call) =
                meta_ops::new_index(ctx, table, "n".into_value(ctx), (length as i64).into())?
            {
                stack.extend(call.args);
                return Ok(SequencePoll::Call {
                    function: call.function,
                    bottom: length,
                });
            }
        }

        let Pack::MainLoop {
            table,
            length,
            ref mut index,
            ref mut batch_end,
        } = *self
        else {
            return Err(anyhow::anyhow!("table.pack: invalid state").into());
        };

        if stack.len() < length {
            return Err(anyhow::anyhow!("table.pack: stack underflow").into());
        }
        // Clear out return values from any called meta_ops::new_index methods.
        stack.resize(length);

        let fuel = exec.fuel();
        while *index < length {
            if index == batch_end {
                let remaining_fuel = fuel.remaining().max(0) as usize;
                let available_elems = remaining_fuel.saturating_mul(cost::VALUES);

                let remaining_elems = length - *index;
                let batch_size = available_elems
                    .max(PACK_MIN_BATCH_SIZE)
                    .min(remaining_elems);
                stack.try_reserve(ctx, batch_size)?;
                *batch_end = *index + batch_size;

                fuel.consume((batch_size / cost::VALUES) as i32);
            }

            while *index < *batch_end {
                if let Some(call) =
                    meta_ops::new_index(ctx, table, (*index as i64 + 1).into(), stack.get(*index))?
                {
                    stack.extend(call.args);
                    return Ok(SequencePoll::Call {
                        function: call.function,
                        bottom: length,
                    });
                }
                *index += 1;
            }

            if !fuel.should_continue() {
                break;
            }
        }

        if *index < length {
            Ok(SequencePoll::Pending)
        } else {
            stack.replace(ctx, table);
            Ok(SequencePoll::Return)
        }
    }
}

// Try to compute the length of a range for unpack, accounting for potential overflow.
fn try_compute_length(start: i64, end: i64) -> Option<usize> {
    debug_assert!(start <= end);
    end.checked_sub(start)
        .and_then(|l| l.checked_add(1))
        .and_then(|l| usize::try_from(l).ok())
}

const UNPACK_MIN_BATCH_SIZE: usize = 4096;

#[derive(Collect)]
#[collect(no_drop)]
enum Unpack<'gc> {
    FindLength {
        start: i64,
        table: Value<'gc>,
    },
    LengthFound {
        start: i64,
        table: Value<'gc>,
    },
    MainLoop {
        start: i64,
        table: Value<'gc>,
        length: usize,
        index: usize,
        batch_end: usize,
        callback_return: bool,
    },
}

impl<'gc> Sequence<'gc> for Unpack<'gc> {
    fn poll(
        mut self: Pin<&mut Self>,
        ctx: Context<'gc>,
        mut exec: Execution<'gc, '_>,
        mut stack: Stack<'gc, '_>,
    ) -> Result<SequencePoll<'gc>, Error<'gc>> {
        if let Unpack::FindLength { start, table } = *self {
            *self = Unpack::LengthFound { start, table };
            // We match PUC-Rio Lua here by finding the length at the *start* of the loop only. If
            // the __index metamethod or some other triggered Lua code changes the length of the
            // table, this will not be considered.
            match meta_ops::len(ctx, table)? {
                MetaResult::Value(v) => stack.push_back(v),
                MetaResult::Call(call) => {
                    stack.extend(call.args);
                    return Ok(SequencePoll::Call {
                        function: call.function,
                        bottom: 0,
                    });
                }
            }
        }

        if let Unpack::LengthFound { start, table } = *self {
            let end: i64 = stack.consume(ctx)?;
            if start > end {
                return Ok(SequencePoll::Return);
            }
            let length = try_compute_length(start, end)
                .filter(|&length| length < MAX_STACK_SIZE)
                .ok_or_else(|| error("too many results to unpack"))?;
            *self = Unpack::MainLoop {
                start,
                table,
                length,
                index: 0,
                batch_end: 0,
                callback_return: false,
            };
        }

        let Unpack::MainLoop {
            start,
            table,
            length,
            ref mut index,
            ref mut batch_end,
            ref mut callback_return,
        } = *self
        else {
            return Err(anyhow::anyhow!("table.unpack: invalid state").into());
        };

        if *callback_return {
            *callback_return = false;
            // The return value for __index was pushed onto the top of the stack, precisely where
            // it's needed.
            *index += 1;
            // truncate stack to the current height
            stack.resize(*index);
        }
        debug_assert_eq!(stack.len(), *index, "index must match stack height");

        let fuel = exec.fuel();
        while *index < length {
            let batch_remaining = *batch_end - *index;
            if batch_remaining == 0 {
                let remaining_fuel = fuel.remaining().max(0) as usize;
                let available_elems = remaining_fuel.saturating_mul(cost::VALUES);

                let remaining_elems = length - *index;
                let batch_size = available_elems
                    .max(UNPACK_MIN_BATCH_SIZE)
                    .min(remaining_elems);
                stack.try_reserve(ctx, batch_size)?;
                *batch_end = *index + batch_size;

                fuel.consume((batch_size / cost::VALUES) as i32);
            }

            while *index < *batch_end {
                match meta_ops::index(ctx, table, (start + *index as i64).into())? {
                    MetaResult::Value(v) => {
                        stack.push_back(v);
                    }
                    MetaResult::Call(call) => {
                        *callback_return = true;
                        stack.extend(call.args);
                        return Ok(SequencePoll::Call {
                            function: call.function,
                            bottom: *index,
                        });
                    }
                }
                *index += 1;
            }

            if *index < length && !fuel.should_continue() {
                return Ok(SequencePoll::Pending);
            }
        }

        debug_assert_eq!(*index, length, "all elements must have been accessed");
        debug_assert_eq!(length, stack.len(), "all elements must be on the stack");
        // Return values are already in-place on the stack
        Ok(SequencePoll::Return)
    }
}

#[derive(PartialEq, Debug)]
enum RawArrayOpResult<T> {
    Success(T),
    Possible,
    Failed,
    Limit(LimitError),
}

// Try to efficiently remove a key from the array part of the table.  (`key` is one-indexed; if it
// is None, the length of the array is used instead.)
//
// If successful, returns the removed value; otherwise, indicates whether the operation is possible
// to implement with a fallback, or is impossible due to an out-of-range index.
//
// Additionally, always returns the computed length of the array from before the operation, and
// the hash lookups its border search took (see `RawTable::length_with_probes`).
fn array_remove_shift<'gc>(
    table: &mut RawTable<'gc>,
    key: Option<i64>,
) -> (RawArrayOpResult<Value<'gc>>, usize, usize) {
    fn inner<'gc>(
        table: &mut RawTable<'gc>,
        length: usize,
        key: Option<i64>,
    ) -> RawArrayOpResult<Value<'gc>> {
        let index = if let Some(k) = key {
            // `t[#t + 1]` is nil, `t[0]` (with `#t == 0`) is left to the slow path.
            if i64::try_from(length).ok().map(|l| l.wrapping_add(1)) == Some(k) {
                return RawArrayOpResult::Success(Value::Nil);
            }
            match usize::try_from(k) {
                Ok(k) if k >= 1 && k <= length => k - 1,
                _ => return RawArrayOpResult::Failed,
            }
        } else if length == 0 {
            // Lua 5.4 removes and returns `t[0]` here (5.1 nothing): left to the slow path.
            return if table.get(Value::Integer(0)).is_nil() {
                RawArrayOpResult::Success(Value::Nil)
            } else {
                RawArrayOpResult::Possible
            };
        } else {
            length - 1
        };

        // `index < length`, so this is only `None` if the sequence does not fit in the array part.
        let Some(shifted) = table.array_mut().get_mut(index..length) else {
            return RawArrayOpResult::Possible;
        };
        let value = shifted
            .first_mut()
            .map(|v| mem::replace(v, Value::Nil))
            .unwrap_or_default();
        // DELIBERATE(cost): the move is charged after the fact (1 fuel per element, by the caller),
        // bounded by the memory limit: at most 2 Mi values, about 1 ms (see `fuel::cost`).
        shifted.rotate_left(1);
        RawArrayOpResult::Success(value)
    }

    let (length, probes) = table.length_with_probes();
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    (inner(table, length, key), length, probes)
}

// Try to efficiently insert a key and value into the array part of the table.  (`key` is
// one-indexed; if it is `None`, the length of the array is used instead.)
//
// The returned [`RawArrayOpResult`] indicates whether the operation was successful, or if it
// failed, whether the operation is possible to implement with a fallback, or is impossible due to
// an out-of-range index.
//
// Additionally, always returns the computed length of the array from before the operation, and
// the hash lookups its border search took (see `RawTable::length_with_probes`).
fn array_insert_shift<'gc>(
    table: &mut RawTable<'gc>,
    key: Option<i64>,
    value: Value<'gc>,
    check_growth: impl FnOnce(Growth) -> Result<(), LimitError>,
) -> (RawArrayOpResult<()>, usize, usize) {
    fn inner<'gc>(
        table: &mut RawTable<'gc>,
        length: usize,
        key: Option<i64>,
        value: Value<'gc>,
        check_growth: impl FnOnce(Growth) -> Result<(), LimitError>,
    ) -> RawArrayOpResult<()> {
        let index = if let Some(k) = key {
            match usize::try_from(k) {
                Ok(k) if k >= 1 && k <= length.saturating_add(1) => k - 1,
                _ => return RawArrayOpResult::Failed,
            }
        } else {
            length
        };

        let array_len = table.array().len();
        if length > array_len {
            return RawArrayOpResult::Possible;
        }

        if length == array_len {
            // If the array is full, grow it.
            if let Err(err) = table.try_grow_array(1, check_growth) {
                return RawArrayOpResult::Limit(err);
            }
        }

        // We know here that `index <= length < array.len()`, so we shift each element to the
        // right by one. `array[length] == nil`, which gets rotated back to `array[index]`; we
        // replace it with the value to insert.
        let Some(shifted) = table.array_mut().get_mut(index..=length) else {
            return RawArrayOpResult::Possible;
        };
        // DELIBERATE(cost): the move is charged after the fact (1 fuel per element, by the caller),
        // bounded by the memory limit: at most 2 Mi values, about 1 ms (see `fuel::cost`).
        shifted.rotate_right(1);
        if let Some(first) = shifted.first_mut() {
            *first = value;
        }
        RawArrayOpResult::Success(())
    }

    let (length, probes) = table.length_with_probes();
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    (
        inner(table, length, key, value, check_growth),
        length,
        probes,
    )
}
