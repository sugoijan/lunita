use std::{
    cell::RefMut,
    hash::{Hash, Hasher},
    mem,
};

use allocator_api2::vec;
use gc_arena::{
    allocator_api::MetricsAlloc, lock::RefLock, Collect, Finalization, Gc, GcWeak, Mutation,
};
use thiserror::Error;

use crate::{
    closure::{UpValue, UpValueState},
    compat::Compat,
    fuel::{cost, count_fuel, fuel_for},
    limits::Written,
    meta_ops, stack,
    table::{Growth, TableError},
    types::{RegisterIndex, VarCount},
    BoxSequence, Callback, Closure, Context, Error, FromMultiValue, Fuel, Function, IntoMultiValue,
    String, Table, UserData, Value,
};

use super::VMError;

/// The current state of a [`Thread`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadMode {
    /// No frames are on the thread and there are no available results, the thread can be started.
    Stopped,
    /// The thread has an error or has returned (or yielded) values that must be taken to move the
    /// thread back to the `Stopped` (or `Suspended`) state.
    Result,
    /// Thread has an active Lua, Callback, or Sequence frame.
    Normal,
    /// Thread has yielded and is waiting on being resumed.
    Suspended,
    /// The thread is waiting on another thread to finish.
    Waiting,
    /// A callback or sequence that this thread owns is currently being run.
    Running,
}

#[derive(Debug, Copy, Clone, Error)]
#[error("bad thread mode: {found:?}{}", if let Some(expected) = *.expected {
        format!(", expected {:?}", expected)
    } else {
        format!("")
    })]
pub struct BadThreadMode {
    pub found: ThreadMode,
    pub expected: Option<ThreadMode>,
}

pub type ThreadInner<'gc> = RefLock<ThreadState<'gc>>;

/// The maximum number of values on the stack of a thread, past which a call is a "stack overflow"
/// error (as `LUAI_MAXSTACK` in PUC-Rio Lua).
pub const MAX_STACK_SIZE: usize = 1_000_000;

/// The most frames a thread may have at once, past which a call is a "stack overflow" error that
/// a script can catch: about what PUC-Rio Lua 5.4 allows a function using a few registers (its
/// stack holds a million values, so a call that keeps no values would otherwise only be bounded
/// by the memory limit).
pub const MAX_FRAMES: usize = 200_000;

/// [`MAX_FRAMES`] with [`Compat::Lua51`]: Lua 5.1's `LUAI_MAXCALLS`.
pub const MAX_FRAMES_51: usize = 20_000;

/// A Lua coroutine.
///
/// All running Lua or callback code is run as part of a larger `Thread`. `Thread`s may create other
/// `Thread`s, suspend them, resume them, and may yield to calling `Thread`s.
#[derive(Debug, Clone, Copy, Collect)]
#[collect(no_drop)]
pub struct Thread<'gc>(Gc<'gc, RefLock<ThreadState<'gc>>>);

impl<'gc> PartialEq for Thread<'gc> {
    fn eq(&self, other: &Thread<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl<'gc> Eq for Thread<'gc> {}

impl<'gc> Hash for Thread<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Gc::as_ptr(self.0).hash(state)
    }
}

impl<'gc> Thread<'gc> {
    pub fn new(ctx: Context<'gc>) -> Thread<'gc> {
        let p = Gc::new(
            &ctx,
            RefLock::new(ThreadState {
                frames: vec::Vec::new_in(MetricsAlloc::new(&ctx)),
                stack: vec::Vec::new_in(MetricsAlloc::new(&ctx)),
                open_upvalues: vec::Vec::new_in(MetricsAlloc::new(&ctx)),
                queued: Written::new(),
            }),
        );
        ctx.finalizers().register_thread(&ctx, p);
        Thread(p)
    }

    pub fn from_inner(inner: Gc<'gc, ThreadInner<'gc>>) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> Gc<'gc, ThreadInner<'gc>> {
        self.0
    }

    pub fn mode(self) -> ThreadMode {
        match self.0.try_borrow() {
            Ok(state) => state.mode(),
            Err(_) => ThreadMode::Running,
        }
    }

    /// If this thread is `Stopped`, start a new function with the given arguments.
    pub fn start(
        self,
        ctx: Context<'gc>,
        function: Function<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) -> Result<(), BadThreadMode> {
        let mut state = self.check_mode(&ctx, ThreadMode::Stopped)?;
        debug_assert!(state.stack.is_empty());
        if let Err(err) = state.push_values(ctx, args.into_multi_value(ctx)) {
            state.stack.clear();
            state.frames.push(Frame::Error(err.into()));
        } else {
            state.push_call_or_error(ctx, 0, function);
        }
        Ok(())
    }

    /// If this thread is `Stopped`, start a new suspended function.
    pub fn start_suspended(
        self,
        mc: &Mutation<'gc>,
        function: Function<'gc>,
    ) -> Result<(), BadThreadMode> {
        let mut state = self.check_mode(mc, ThreadMode::Stopped)?;
        state.frames.push(Frame::Start(function));
        Ok(())
    }

    /// If the thread is in the `Result` mode, take the returned (or yielded) values. Moves the
    /// thread back to the `Stopped` (or `Suspended`) mode.
    pub fn take_result<T: FromMultiValue<'gc>>(
        self,
        ctx: Context<'gc>,
    ) -> Result<Result<T, Error<'gc>>, BadThreadMode> {
        let mut state = self.check_mode(&ctx, ThreadMode::Result)?;
        Ok(state
            .take_result()
            .and_then(|vals| Ok(T::from_multi_value(ctx, vals)?)))
    }

    /// If the thread is in `Suspended` mode, resume it.
    pub fn resume(
        self,
        ctx: Context<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) -> Result<(), BadThreadMode> {
        let mut state = self.check_mode(&ctx, ThreadMode::Suspended)?;

        let bottom = state.stack.len();
        let pushed = state.push_values(ctx, args.into_multi_value(ctx));

        // A suspended thread always has a `Start` or `Yielded` top frame.
        match state.frames.pop() {
            Some(Frame::Start(_) | Frame::Yielded) if pushed.is_err() => {
                // Unwind as if the error was raised where the thread was suspended.
                state.stack.truncate(bottom);
                if let Err(err) = pushed {
                    state.frames.push(Frame::Error(err.into()));
                }
            }
            Some(Frame::Start(function)) => {
                debug_assert!(
                    bottom == 0 && state.open_upvalues.is_empty() && state.frames.is_empty()
                );
                state.push_call_or_error(ctx, 0, function);
            }
            Some(Frame::Yielded) => {
                if let Err(err) = state.return_to(ctx, bottom) {
                    state.frames.push(Frame::Error(err.into()));
                }
            }
            frame => {
                if let Some(frame) = frame {
                    state.frames.push(frame);
                }
                return Err(BadThreadMode {
                    found: state.mode(),
                    expected: Some(ThreadMode::Suspended),
                });
            }
        }
        Ok(())
    }

    /// If the thread is in `Suspended` mode, cause an error wherever the thread was suspended.
    pub fn resume_err(self, mc: &Mutation<'gc>, error: Error<'gc>) -> Result<(), BadThreadMode> {
        let mut state = self.check_mode(mc, ThreadMode::Suspended)?;
        let frame = state.frames.pop();
        debug_assert!(matches!(frame, Some(Frame::Start(_) | Frame::Yielded)));
        state.frames.push(Frame::Error(error));
        Ok(())
    }

    /// If this thread is in any other mode than `Running`, reset the thread completely and restore
    /// it to the `Stopped` state.
    pub fn reset(self, mc: &Mutation<'gc>) -> Result<(), BadThreadMode> {
        match self.0.try_borrow_mut(mc) {
            Ok(mut state) => {
                state.reset(mc);
                Ok(())
            }
            Err(_) => Err(BadThreadMode {
                found: ThreadMode::Running,
                expected: None,
            }),
        }
    }

    /// The number of frames on this thread (zero if it is running).
    pub(crate) fn frame_count(self) -> usize {
        self.0.try_borrow().map_or(0, |state| state.frames.len())
    }

    /// Reset this thread and leave it in the `Result` mode with the given error.
    pub(crate) fn set_error(self, mc: &Mutation<'gc>, error: Error<'gc>) {
        if let Ok(mut state) = self.0.try_borrow_mut(mc) {
            state.reset(mc);
            state.frames.push(Frame::Error(error));
        }
    }

    /// For each open upvalue pointing to this thread, if the upvalue itself is live, then resurrect
    /// the actual value that it is pointing to.
    ///
    /// Because open upvalues keep a *weak* pointer to their parent thread, their target values will
    /// not be properly marked as live until until they are manually marked with this method.
    pub(crate) fn resurrect_live_upvalues(
        self,
        fc: &Finalization<'gc>,
    ) -> Result<(), BadThreadMode> {
        // If this thread is not dead, then none of the held stack values can be dead, so we don't
        // need to resurrect them.
        if Gc::is_dead(fc, self.0) {
            let state = self.0.try_borrow().map_err(|_| BadThreadMode {
                found: ThreadMode::Running,
                expected: None,
            })?;
            state.resurrect_live_upvalues(fc);
        }
        Ok(())
    }

    fn check_mode(
        &self,
        mc: &Mutation<'gc>,
        expected: ThreadMode,
    ) -> Result<RefMut<'_, ThreadState<'gc>>, BadThreadMode> {
        debug_assert!(expected != ThreadMode::Running);
        if let Ok(state) = self.0.try_borrow_mut(mc) {
            let found = state.mode();
            if found == expected {
                Ok(state)
            } else {
                Err(BadThreadMode {
                    found,
                    expected: Some(expected),
                })
            }
        } else {
            Err(BadThreadMode {
                found: ThreadMode::Running,
                expected: Some(expected),
            })
        }
    }
}

#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct OpenUpValue<'gc> {
    thread: GcWeak<'gc, RefLock<ThreadState<'gc>>>,
    stack_index: usize,
}

impl<'gc> OpenUpValue<'gc> {
    const UPGRADE_ERR: &'static str = "thread not finalized: upvalues not closed";

    // Open upvalues are always closed before their thread is finalized, and the library does not
    // access them through this path while their thread is running, so neither failure below can
    // happen from Lua. A host reading an open upvalue of the running thread (`Closure::upvalues`)
    // gets nil, and its write is dropped, with a debug assertion.

    pub fn get(self, mc: &Mutation<'gc>) -> Value<'gc> {
        let Some(thread) = self.thread.upgrade(mc) else {
            debug_assert!(false, "{}", Self::UPGRADE_ERR);
            return Value::Nil;
        };
        let Ok(state) = thread.try_borrow() else {
            debug_assert!(false, "open upvalue of a running thread");
            return Value::Nil;
        };
        state
            .stack
            .get(self.stack_index)
            .copied()
            .unwrap_or_default()
    }

    pub fn set(self, mc: &Mutation<'gc>, v: Value<'gc>) {
        let Some(thread) = self.thread.upgrade(mc) else {
            debug_assert!(false, "{}", Self::UPGRADE_ERR);
            return;
        };
        let Ok(mut state) = thread.try_borrow_mut(mc) else {
            debug_assert!(false, "open upvalue of a running thread");
            return;
        };
        if let Some(slot) = state.stack.get_mut(self.stack_index) {
            *slot = v;
        }
    }
}

#[derive(Debug, Copy, Clone, Collect)]
#[collect(require_static)]
pub(super) enum MetaReturn {
    /// No return value is expected.
    None,
    /// Place a single return value at an index relative to the returned to function's stack bottom.
    Register(RegisterIndex),
    /// Increment the PC by one if the returned value converted to a boolean is equal to this.
    SkipIf(bool),
}

#[derive(Debug, Copy, Clone, Collect)]
#[collect(require_static)]
pub(super) enum LuaReturn {
    /// Normal function call, place return values at the bottom of the returning function's stack,
    /// as normal.
    Normal(VarCount),
    /// Synthetic metamethod call, do the operation specified in MetaReturn.
    Meta(MetaReturn),
    /// Tail call of a callback, return the values from this frame too.
    Tail,
}

#[derive(Debug, Collect)]
#[collect(no_drop)]
pub(super) enum Frame<'gc> {
    /// A running Lua frame.
    Lua {
        bottom: usize,
        closure: Closure<'gc>,
        base: usize,
        is_variable: bool,
        pc: usize,
        stack_size: usize,
        expected_return: Option<LuaReturn>,
    },
    /// A frame for a running sequence. When it is the top frame, either the `poll` or `error`
    /// method will be called the next time this thread is stepped, depending on whether there is a
    /// pending error.
    Sequence {
        bottom: usize,
        sequence: BoxSequence<'gc>,
        // Will be set when unwinding has stopped at this frame. If set, this must be the top frame
        // of the stack.
        pending_error: Option<Error<'gc>>,
    },
    /// A suspended function call that has not yet been run. Must be the only frame in the stack.
    Start(Function<'gc>),
    /// A callback that has been queued but not called yet. Must be the top frame of the stack.
    Callback {
        bottom: usize,
        callback: Callback<'gc>,
    },
    /// Thread has yielded and is waiting resume. Must be the top frame of the stack or immediately
    /// below a Result frame.
    Yielded,
    /// We are waiting on an upper thread to finish. Must be the top frame of the stack.
    WaitThread,
    /// Results are waiting to be taken. Must be the top frame of the stack.
    Result { bottom: usize },
    /// An error is currently unwinding. Must be the top frame of the stack.
    Error(Error<'gc>),
}

#[derive(Debug, Collect)]
#[collect(no_drop)]
pub struct ThreadState<'gc> {
    pub(super) frames: vec::Vec<Frame<'gc>, MetricsAlloc<'gc>>,
    pub(super) stack: vec::Vec<Value<'gc>, MetricsAlloc<'gc>>,
    pub(super) open_upvalues: vec::Vec<UpValue<'gc>, MetricsAlloc<'gc>>,
    queued: Written,
}

impl<'gc> ThreadState<'gc> {
    pub(super) fn mode(&self) -> ThreadMode {
        match self.frames.last() {
            None => {
                debug_assert!(self.stack.is_empty() && self.open_upvalues.is_empty());
                ThreadMode::Stopped
            }
            Some(frame) => match frame {
                Frame::Lua { .. } | Frame::Callback { .. } | Frame::Sequence { .. } => {
                    ThreadMode::Normal
                }
                Frame::Start(_) | Frame::Yielded => ThreadMode::Suspended,
                Frame::WaitThread => ThreadMode::Waiting,
                Frame::Result { .. } => ThreadMode::Result,
                Frame::Error(_) => {
                    if self.frames.len() == 1 {
                        ThreadMode::Result
                    } else {
                        ThreadMode::Normal
                    }
                }
            },
        }
    }

    /// Makes room for the stack to grow to `len` values, checking the stack size and memory limits
    /// first.
    pub(super) fn reserve_stack(&mut self, ctx: Context<'gc>, len: usize) -> Result<(), VMError> {
        stack::reserve_values(ctx, &mut self.stack, len)
    }

    /// Makes room for one more frame, checking the memory limit first.
    ///
    /// This always leaves room for one frame more, so that an error frame can be pushed after a
    /// frame is refused without growing the frame stack.
    fn reserve_frame(&mut self, ctx: Context<'gc>) -> Result<(), VMError> {
        let max_frames = match ctx.compat() {
            Compat::Lua51 => MAX_FRAMES_51,
            Compat::Default => MAX_FRAMES,
        };
        if self.frames.len() >= max_frames {
            return Err(VMError::StackOverflow);
        }
        let capacity = self.frames.capacity();
        let required = self.frames.len().saturating_add(2);
        if required > capacity {
            let new_capacity = required.max(capacity.saturating_mul(2)).max(4);
            ctx.check_growth(Growth {
                bytes: new_capacity.saturating_mul(mem::size_of::<Frame<'gc>>()),
                entries: 0,
            })?;
            self.frames
                .reserve_exact(new_capacity.saturating_sub(self.frames.len()));
        }
        Ok(())
    }

    /// Pushes a new function call frame.
    ///
    /// Arguments are taken from the top of the stack starting at `bottom`, which will become the
    /// bottom of the newly pushed frame.
    pub(super) fn push_call(
        &mut self,
        ctx: Context<'gc>,
        bottom: usize,
        function: Function<'gc>,
    ) -> Result<(), VMError> {
        self.reserve_frame(ctx)?;
        match function {
            Function::Closure(closure) => {
                let proto = closure.prototype();
                let fixed_params = proto.fixed_params as usize;
                let stack_size = proto.stack_size as usize;
                let Some(args) = self.stack.get_mut(bottom..) else {
                    return Err(VMError::BadStack);
                };
                let given_params = args.len();

                let var_params = given_params.saturating_sub(fixed_params);
                args.rotate_right(var_params);
                let base = bottom + var_params;

                self.reserve_stack(ctx, base + stack_size)?;
                self.stack.resize(base + stack_size, Value::Nil);

                self.frames.push(Frame::Lua {
                    bottom,
                    closure,
                    base,
                    is_variable: false,
                    pc: 0,
                    stack_size,
                    expected_return: None,
                });
            }
            Function::Callback(callback) => {
                self.frames.push(Frame::Callback { bottom, callback });
            }
        }
        Ok(())
    }

    /// Pushes a new function call frame, or an error frame if that fails.
    pub(super) fn push_call_or_error(
        &mut self,
        ctx: Context<'gc>,
        bottom: usize,
        function: Function<'gc>,
    ) {
        if let Err(err) = self.push_call(ctx, bottom, function) {
            self.stack.truncate(bottom);
            self.frames.push(Frame::Error(err.into()));
        }
    }

    /// Pushes values to the stack, checking the stack size and memory limits as it grows.
    ///
    /// On an error, some of the values may have been pushed.
    pub(super) fn push_values(
        &mut self,
        ctx: Context<'gc>,
        values: impl IntoIterator<Item = Value<'gc>>,
    ) -> Result<(), VMError> {
        let values = values.into_iter();
        self.reserve_stack(ctx, self.stack.len().saturating_add(values.size_hint().0))?;
        for value in values {
            if self.stack.len() == self.stack.capacity() {
                self.reserve_stack(ctx, self.stack.len().saturating_add(1))?;
            }
            self.stack.push(value);
        }
        Ok(())
    }

    /// Return to the current top frame from a popped frame.
    ///
    /// The current top frame (the frame we are returning to) must be a Lua frame, Sequence, or
    /// there must be no frames at all (in which case this will push a new `Result` frame.)
    ///
    /// `bottom` must be the bottom of the popped, returning frame, and the return values are taken
    /// from the top of the stack starting at `bottom`.
    pub(super) fn return_to(
        &mut self,
        ctx: Context<'gc>,
        mut bottom: usize,
    ) -> Result<(), VMError> {
        // Returning many values moves them (a chain of returns passing them on is quadratic);
        // a few are part of the call's cost.
        let returned = self.stack.len().saturating_sub(bottom);
        if returned >= cost::VALUES {
            ctx.charge_fuel(fuel_for(returned, cost::VALUES));
        }
        // Lua frames that tail called a callback return its values in turn: they are moved once,
        // to the lowest of those frames (a chain of them can be as long as the frame limit).
        let mut lowest = None;
        while let Some(&Frame::Lua {
            bottom: lua_bottom,
            expected_return: Some(LuaReturn::Tail),
            ..
        }) = self.frames.last()
        {
            self.frames.pop();
            lowest = Some(lua_bottom);
        }
        if let Some(lua_bottom) = lowest {
            let len = self.stack.len().saturating_sub(bottom);
            self.stack.copy_within(bottom.., lua_bottom);
            self.stack.truncate(lua_bottom + len);
            bottom = lua_bottom;
        }
        match self.frames.last_mut() {
            Some(Frame::Sequence { .. }) => {}
            Some(Frame::Lua {
                expected_return,
                is_variable,
                base,
                stack_size,
                pc,
                ..
            }) => {
                let return_len = self.stack.len().saturating_sub(bottom);
                match expected_return.take() {
                    Some(LuaReturn::Normal(ret_count)) => {
                        let return_len = ret_count
                            .to_constant()
                            .map(|c| c as usize)
                            .unwrap_or(return_len);

                        self.stack.truncate(bottom + return_len);

                        *is_variable = ret_count.is_variable();
                        if !ret_count.is_variable() {
                            self.stack.resize(*base + *stack_size, Value::Nil);
                        }
                    }
                    Some(LuaReturn::Meta(meta_ret)) => {
                        let meta_val = self.stack.get(bottom).copied().unwrap_or_default();
                        self.stack.truncate(bottom);
                        self.stack.resize(*base + *stack_size, Value::Nil);
                        *is_variable = false;
                        match meta_ret {
                            MetaReturn::None => {}
                            MetaReturn::Register(reg) => {
                                let Some(slot) = self.stack.get_mut(*base + reg.0 as usize) else {
                                    return Err(VMError::BadStack);
                                };
                                *slot = meta_val;
                            }
                            MetaReturn::SkipIf(skip_if) => {
                                if meta_val.to_bool() == skip_if {
                                    *pc += 1;
                                }
                            }
                        }
                    }
                    Some(LuaReturn::Tail) | None => return Err(VMError::BadReturn),
                }
            }
            None => {
                self.frames.push(Frame::Result { bottom });
            }
            _ => return Err(VMError::BadReturn),
        }
        Ok(())
    }

    pub(super) fn take_result(
        &mut self,
    ) -> Result<impl ExactSizeIterator<Item = Value<'gc>> + '_, Error<'gc>> {
        match self.frames.pop() {
            Some(Frame::Result { bottom }) => {
                let bottom = bottom.min(self.stack.len());
                Ok(self.stack.drain(bottom..))
            }
            Some(Frame::Error(err)) => {
                debug_assert!(self.stack.is_empty());
                debug_assert!(self.frames.is_empty());
                debug_assert!(self.open_upvalues.is_empty());
                Err(err)
            }
            frame => {
                if let Some(frame) = frame {
                    self.frames.push(frame);
                }
                Err(BadThreadMode {
                    found: self.mode(),
                    expected: Some(ThreadMode::Result),
                }
                .into())
            }
        }
    }

    pub(super) fn close_upvalues(&mut self, mc: &Mutation<'gc>, bottom: usize) {
        let start = match self
            .open_upvalues
            .binary_search_by(|&u| open_upvalue_ind(u).cmp(&bottom))
        {
            Ok(i) => i,
            Err(i) => i,
        };

        let this_ptr = self as *mut _;
        for &upval in self.open_upvalues.get(start..).unwrap_or_default() {
            match upval.get() {
                UpValueState::Open(open_upvalue) => {
                    debug_assert!(
                        open_upvalue.thread.upgrade(mc).map(|t| t.as_ptr()) == Some(this_ptr)
                    );
                    upval.set(
                        mc,
                        UpValueState::Closed(
                            self.stack
                                .get(open_upvalue.stack_index)
                                .copied()
                                .unwrap_or_default(),
                        ),
                    );
                }
                UpValueState::Closed(_) => debug_assert!(false, "upvalue is not open"),
            }
        }

        self.open_upvalues.truncate(start);
    }

    pub(super) fn reset(&mut self, mc: &Mutation<'gc>) {
        self.close_upvalues(mc, 0);
        debug_assert!(self.open_upvalues.is_empty());
        self.open_upvalues.clear();
        self.stack.clear();
        self.frames.clear();
    }

    fn resurrect_live_upvalues(&self, fc: &Finalization<'gc>) {
        for &upval in &self.open_upvalues {
            if !Gc::is_dead(fc, UpValue::into_inner(upval)) {
                match upval.get() {
                    UpValueState::Open(open_upvalue) => {
                        match self.stack.get(open_upvalue.stack_index).copied() {
                            Some(Value::String(s)) => Gc::resurrect(fc, String::into_inner(s)),
                            Some(Value::Table(t)) => Gc::resurrect(fc, Table::into_inner(t)),
                            Some(Value::Function(Function::Closure(c))) => {
                                Gc::resurrect(fc, Closure::into_inner(c))
                            }
                            Some(Value::Function(Function::Callback(c))) => {
                                Gc::resurrect(fc, Callback::into_inner(c))
                            }
                            Some(Value::Thread(t)) => Gc::resurrect(fc, Thread::into_inner(t)),
                            Some(Value::UserData(u)) => Gc::resurrect(fc, UserData::into_inner(u)),
                            _ => {}
                        }
                    }
                    UpValueState::Closed(_) => debug_assert!(false, "upvalue is not open"),
                }
            }
        }
    }
}

pub(super) struct LuaFrame<'gc, 'a> {
    pub(super) thread: Thread<'gc>,
    pub(super) state: &'a mut ThreadState<'gc>,
    pub(super) fuel: &'a mut Fuel,
}

impl<'gc, 'a> LuaFrame<'gc, 'a> {
    const FUEL_PER_CALL: i32 = 4;
    const FUEL_PER_ITEM: i32 = 1;

    // Returns the active closure for this Lua frame
    pub(super) fn closure(&self) -> Result<Closure<'gc>, VMError> {
        match self.state.frames.last() {
            Some(Frame::Lua { closure, .. }) => Ok(*closure),
            _ => Err(VMError::NotLuaFrame),
        }
    }

    /// returns a view of the Lua frame's registers
    pub(super) fn registers<'b>(&'b mut self) -> Result<LuaRegisters<'gc, 'b>, VMError> {
        match self.state.frames.last_mut() {
            Some(Frame::Lua {
                bottom, base, pc, ..
            }) => {
                let Some((upper_stack, stack_frame)) = self.state.stack.split_at_mut_checked(*base)
                else {
                    return Err(VMError::BadStack);
                };
                Ok(LuaRegisters {
                    pc,
                    stack_frame,
                    upper_stack,
                    bottom: *bottom,
                    base: *base,
                    open_upvalues: &mut self.state.open_upvalues,
                    thread: self.thread,
                })
            }
            _ => Err(VMError::NotLuaFrame),
        }
    }

    /// Place the current frame's varargs at the given register, expecting the given count
    pub(super) fn varargs(
        &mut self,
        ctx: Context<'gc>,
        dest: RegisterIndex,
        count: VarCount,
    ) -> Result<(), VMError> {
        let Some(Frame::Lua {
            bottom,
            base,
            is_variable,
            ..
        }) = self.state.frames.last_mut()
        else {
            return Err(VMError::NotLuaFrame);
        };

        if *is_variable {
            return Err(VMError::ExpectedVariableStack(false));
        }

        self.fuel.consume(Self::FUEL_PER_CALL);

        let varargs_start = *bottom;
        let varargs_len = base.saturating_sub(varargs_start);

        let dest = *base + dest.0 as usize;
        if let Some(count) = count.to_constant() {
            let count = count as usize;
            self.fuel.consume(count_fuel(Self::FUEL_PER_ITEM, count));

            // The destination registers are within the frame, and the varargs are below it.
            if dest + count > self.state.stack.len() {
                return Err(VMError::BadStack);
            }
            let copied = count.min(varargs_len);
            self.state
                .stack
                .copy_within(varargs_start..varargs_start + copied, dest);
            if let Some(rest) = self.state.stack.get_mut(dest + copied..dest + count) {
                rest.fill(Value::Nil);
            }
        } else {
            self.fuel
                .consume(count_fuel(Self::FUEL_PER_ITEM, varargs_len));

            if varargs_start + varargs_len > dest {
                return Err(VMError::BadStack);
            }
            *is_variable = true;
            self.state.stack.truncate(dest);
            self.state.reserve_stack(ctx, dest + varargs_len)?;
            self.state
                .stack
                .extend_from_within(varargs_start..varargs_start + varargs_len);
        }

        Ok(())
    }

    /// Set elements of a table as a group according to the `SetList` opcode protocol.
    ///
    /// Expects a table at register `table_base`, the current table index at `table_base + 1`, and
    /// `count` elements following this.
    pub(super) fn set_table_list(
        &mut self,
        ctx: Context<'gc>,
        table_base: RegisterIndex,
        count: VarCount,
    ) -> Result<(), VMError> {
        let Some(&mut Frame::Lua {
            base,
            ref mut is_variable,
            stack_size,
            ..
        }) = self.state.frames.last_mut()
        else {
            return Err(VMError::NotLuaFrame);
        };

        if count.is_variable() != *is_variable {
            return Err(VMError::ExpectedVariableStack(count.is_variable()));
        }

        self.fuel.consume(Self::FUEL_PER_CALL);

        let table_ind = base + table_base.0 as usize;
        let start_ind = table_ind + 1;

        let (Some(&table), Some(&start)) = (
            self.state.stack.get(table_ind),
            self.state.stack.get(start_ind),
        ) else {
            return Err(VMError::BadStack);
        };

        let (Value::Table(table), Value::Integer(mut start)) = (table, start) else {
            return Err(VMError::BadSetList(table.type_name(), start.type_name()));
        };

        let values = self.state.stack.get(table_ind + 2..).unwrap_or_default();
        let set_count = count
            .to_constant()
            .map(|c| c as usize)
            .unwrap_or(values.len());
        let Some(values) = values.get(..set_count) else {
            return Err(VMError::BadStack);
        };

        self.fuel
            .consume(count_fuel(Self::FUEL_PER_ITEM, set_count));
        for &value in values {
            if let Some(inc) = start.checked_add(1) {
                start = inc;
                table.set(ctx, inc, value).map_err(|err| match err {
                    TableError::Limit(err) => VMError::Limit(err),
                    // An integer key is always a valid key.
                    TableError::InvalidKey(_) => VMError::BadSetList("table", "integer"),
                })?;
            } else {
                break;
            }
        }

        if let Some(slot) = self.state.stack.get_mut(start_ind) {
            *slot = Value::Integer(start);
        }

        if count.is_variable() {
            self.state.stack.resize(base + stack_size, Value::Nil);
            *is_variable = false;
        }

        Ok(())
    }

    /// Call the function at the given register with the given arguments. On return, results will be
    /// placed starting at the function register.
    pub(super) fn call_function(
        self,
        ctx: Context<'gc>,
        func: RegisterIndex,
        args: VarCount,
        returns: VarCount,
    ) -> Result<(), VMError> {
        let Some(Frame::Lua {
            expected_return,
            is_variable,
            base,
            ..
        }) = self.state.frames.last_mut()
        else {
            return Err(VMError::NotLuaFrame);
        };

        if *is_variable != args.is_variable() {
            return Err(VMError::ExpectedVariableStack(args.is_variable()));
        }

        self.fuel.consume(Self::FUEL_PER_CALL);

        let function_index = *base + func.0 as usize;
        let Some(&function) = self.state.stack.get(function_index) else {
            return Err(VMError::BadStack);
        };
        let available_args = self.state.stack.len() - function_index - 1;
        let arg_count = args
            .to_constant()
            .map(|c| c as usize)
            .unwrap_or(available_args);
        if arg_count > available_args {
            return Err(VMError::BadStack);
        }

        let call = meta_ops::call(ctx, function)?;
        *expected_return = Some(LuaReturn::Normal(returns));

        self.fuel
            .consume(count_fuel(Self::FUEL_PER_ITEM, arg_count));

        self.state.stack.remove(function_index);
        self.state.stack.truncate(function_index + arg_count);

        self.state.push_call(ctx, function_index, call)
    }

    /// Calls the function at the given index with a constant number of arguments without
    /// invalidating the function or its arguments. Returns are placed *after* the function and its
    /// arguments, and all registers past this are invalidated as normal.
    pub(super) fn call_function_keep(
        self,
        ctx: Context<'gc>,
        func: RegisterIndex,
        arg_count: u8,
        returns: VarCount,
    ) -> Result<(), VMError> {
        let Some(Frame::Lua {
            expected_return,
            is_variable,
            base,
            ..
        }) = self.state.frames.last_mut()
        else {
            return Err(VMError::NotLuaFrame);
        };

        if *is_variable {
            return Err(VMError::ExpectedVariableStack(false));
        }

        self.fuel.consume(Self::FUEL_PER_CALL);

        let arg_count = arg_count as usize;

        let function_index = *base + func.0 as usize;
        let top = function_index + 1 + arg_count;
        if top > self.state.stack.len() {
            return Err(VMError::BadStack);
        }
        let Some(&function) = self.state.stack.get(function_index) else {
            return Err(VMError::BadStack);
        };

        let call = meta_ops::call(ctx, function)?;
        *expected_return = Some(LuaReturn::Normal(returns));

        self.fuel
            .consume(count_fuel(Self::FUEL_PER_ITEM, arg_count));

        self.state.stack.truncate(top);
        self.state.reserve_stack(ctx, top + arg_count)?;
        self.state
            .stack
            .extend_from_within(function_index + 1..function_index + 1 + arg_count);

        self.state.push_call(ctx, top, call)
    }

    /// Calls an externally defined function in a completely non-destructive way in a new frame, and
    /// places an optional single result of this function call at the given register.
    ///
    /// Nothing at all in the frame is invalidated, other than optionally placing the return value.
    pub(super) fn call_meta_function(
        self,
        ctx: Context<'gc>,
        func: Function<'gc>,
        args: &[Value<'gc>],
        meta_ret: MetaReturn,
    ) -> Result<(), VMError> {
        let Some(Frame::Lua {
            expected_return,
            is_variable,
            base,
            stack_size,
            ..
        }) = self.state.frames.last_mut()
        else {
            return Err(VMError::NotLuaFrame);
        };

        if *is_variable {
            return Err(VMError::ExpectedVariableStack(false));
        }

        self.fuel.consume(Self::FUEL_PER_CALL);

        let top = self.state.stack.len();
        debug_assert_eq!(top, *base + *stack_size);

        *expected_return = Some(LuaReturn::Meta(meta_ret));

        self.fuel
            .consume(count_fuel(Self::FUEL_PER_ITEM, args.len()));

        self.state.reserve_stack(ctx, top + args.len())?;
        self.state.stack.extend_from_slice(args);

        self.state.push_call(ctx, top, func)
    }

    /// Tail-call the function at the given register with the given arguments. For a closure, pops
    /// the current Lua frame and pushes a frame for it; for a callback, keeps the Lua frame to pass
    /// the callback's returns through (see below).
    pub(super) fn tail_call_function(
        self,
        ctx: Context<'gc>,
        func: RegisterIndex,
        args: VarCount,
    ) -> Result<(), VMError> {
        let Some(&mut Frame::Lua {
            bottom,
            base,
            is_variable,
            ..
        }) = self.state.frames.last_mut()
        else {
            return Err(VMError::NotLuaFrame);
        };

        if is_variable != args.is_variable() {
            return Err(VMError::ExpectedVariableStack(args.is_variable()));
        }

        self.fuel.consume(Self::FUEL_PER_CALL);

        let function_index = base + func.0 as usize;
        let Some(&function) = self.state.stack.get(function_index) else {
            return Err(VMError::BadStack);
        };
        let available_args = self.state.stack.len() - function_index - 1;
        let arg_count = args
            .to_constant()
            .map(|c| c as usize)
            .unwrap_or(available_args);
        if arg_count > available_args {
            return Err(VMError::BadStack);
        }

        let call = meta_ops::call(ctx, function)?;

        self.state.close_upvalues(&ctx, bottom);
        self.fuel
            .consume(count_fuel(Self::FUEL_PER_ITEM, arg_count));

        if let Function::Callback(_) = call {
            // As PUC-Rio Lua does with C functions, the callback is called from this frame, which
            // returns its values: the frame stays below it to tell where it was called from (for
            // the position of its errors, see `Execution::lua_frame`).
            if let Some(Frame::Lua {
                expected_return, ..
            }) = self.state.frames.last_mut()
            {
                *expected_return = Some(LuaReturn::Tail);
            }
            self.state.stack.truncate(function_index + 1 + arg_count);
            self.state.stack.remove(function_index);
            return self.state.push_call(ctx, function_index, call);
        }

        self.state.frames.pop();

        self.state
            .stack
            .copy_within(function_index + 1..function_index + 1 + arg_count, bottom);
        self.state.stack.truncate(bottom + arg_count);

        self.state.push_call(ctx, bottom, call)
    }

    /// Return to the upper frame with results starting at the given register index.
    pub(super) fn return_upper(
        self,
        ctx: Context<'gc>,
        start: RegisterIndex,
        count: VarCount,
    ) -> Result<(), VMError> {
        let Some(&Frame::Lua {
            bottom,
            base,
            is_variable,
            ..
        }) = self.state.frames.last()
        else {
            return Err(VMError::NotLuaFrame);
        };

        if is_variable != count.is_variable() {
            return Err(VMError::ExpectedVariableStack(count.is_variable()));
        }

        let start = base + start.0 as usize;
        let available = self.state.stack.len().saturating_sub(start);
        let count = count.to_constant().map(|c| c as usize).unwrap_or(available);
        if count > available {
            return Err(VMError::BadStack);
        }

        self.state.frames.pop();
        self.fuel.consume(Self::FUEL_PER_CALL);

        self.state.close_upvalues(&ctx, bottom);

        self.fuel.consume(count_fuel(Self::FUEL_PER_ITEM, count));

        self.state.stack.copy_within(start..start + count, bottom);
        self.state.stack.truncate(bottom + count);
        self.state.return_to(ctx, bottom)
    }
}

pub(super) struct LuaRegisters<'gc, 'a> {
    pub pc: &'a mut usize,
    pub stack_frame: &'a mut [Value<'gc>],
    upper_stack: &'a mut [Value<'gc>],
    bottom: usize,
    base: usize,
    open_upvalues: &'a mut vec::Vec<UpValue<'gc>, MetricsAlloc<'gc>>,
    thread: Thread<'gc>,
}

impl<'gc, 'a> LuaRegisters<'gc, 'a> {
    pub(super) fn open_upvalue(&mut self, mc: &Mutation<'gc>, reg: RegisterIndex) -> UpValue<'gc> {
        let ind = self.base + reg.0 as usize;
        let i = match self
            .open_upvalues
            .binary_search_by(|&u| open_upvalue_ind(u).cmp(&ind))
        {
            Ok(i) => {
                if let Some(&uv) = self.open_upvalues.get(i) {
                    return uv;
                }
                i
            }
            Err(i) => i,
        };
        let uv = UpValue::new(
            mc,
            UpValueState::Open(OpenUpValue {
                thread: Gc::downgrade(self.thread.0),
                stack_index: ind,
            }),
        );
        self.open_upvalues.insert(i, uv);
        uv
    }

    /// Get the value of a register in this frame.
    #[inline]
    pub(super) fn get(&self, reg: usize) -> Result<Value<'gc>, VMError> {
        self.stack_frame.get(reg).copied().ok_or(VMError::BadStack)
    }

    /// Set the value of a register in this frame.
    #[inline]
    pub(super) fn set(&mut self, reg: usize, value: Value<'gc>) -> Result<(), VMError> {
        *self.stack_frame.get_mut(reg).ok_or(VMError::BadStack)? = value;
        Ok(())
    }

    pub(super) fn get_upvalue(&self, mc: &Mutation<'gc>, upvalue: UpValue<'gc>) -> Value<'gc> {
        match upvalue.get() {
            UpValueState::Open(open_upvalue) => {
                if open_upvalue.thread.as_ptr() == Gc::as_ptr(self.thread.0) {
                    debug_assert!(
                        open_upvalue.stack_index < self.bottom,
                        "upvalues must be above the current Lua frame"
                    );
                    self.upper_stack
                        .get(open_upvalue.stack_index)
                        .copied()
                        .unwrap_or_default()
                } else {
                    open_upvalue.get(mc)
                }
            }
            UpValueState::Closed(v) => v,
        }
    }

    pub(super) fn set_upvalue(
        &mut self,
        mc: &Mutation<'gc>,
        upvalue: UpValue<'gc>,
        value: Value<'gc>,
    ) {
        match upvalue.get() {
            UpValueState::Open(open_upvalue) => {
                if open_upvalue.thread.as_ptr() == Gc::as_ptr(self.thread.0) {
                    debug_assert!(
                        open_upvalue.stack_index < self.bottom,
                        "upvalues must be above the current Lua frame"
                    );
                    if let Some(slot) = self.upper_stack.get_mut(open_upvalue.stack_index) {
                        *slot = value;
                    }
                } else {
                    open_upvalue.set(mc, value);
                }
            }
            UpValueState::Closed(_) => {
                upvalue.set(mc, UpValueState::Closed(value));
            }
        }
    }

    pub(super) fn close_upvalues(&mut self, mc: &Mutation<'gc>, bottom_register: RegisterIndex) {
        let bottom = self.base + bottom_register.0 as usize;
        let start = match self
            .open_upvalues
            .binary_search_by(|&u| open_upvalue_ind(u).cmp(&bottom))
        {
            Ok(i) => i,
            Err(i) => i,
        };

        for &upval in self.open_upvalues.get(start..).unwrap_or_default() {
            match upval.get() {
                UpValueState::Open(open_upvalue) => {
                    debug_assert!(open_upvalue.thread.as_ptr() == Gc::as_ptr(self.thread.0));
                    let value = if open_upvalue.stack_index < self.base {
                        self.upper_stack.get(open_upvalue.stack_index)
                    } else {
                        self.stack_frame.get(open_upvalue.stack_index - self.base)
                    };
                    upval.set(mc, UpValueState::Closed(value.copied().unwrap_or_default()));
                }
                UpValueState::Closed(_) => debug_assert!(false, "upvalue is not open"),
            }
        }

        self.open_upvalues.truncate(start);
    }
}

// Open upvalue lists only contain open upvalues.
fn open_upvalue_ind<'gc>(u: UpValue<'gc>) -> usize {
    match u.get() {
        UpValueState::Open(open_upvalue) => open_upvalue.stack_index,
        UpValueState::Closed(_) => {
            debug_assert!(false, "upvalue is not open");
            usize::MAX
        }
    }
}
