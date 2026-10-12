use std::hash::{Hash, Hasher};

use allocator_api2::vec;
use gc_arena::{allocator_api::MetricsAlloc, lock::RefLock, Collect, Gc, Mutation};
use thiserror::Error;

use crate::{
    compiler::{FunctionRef, LineNumber},
    error::LibraryError,
    fuel::{cost, fuel_for},
    limits::{LimitError, Written},
    thread::BadThreadMode,
    CallbackReturn, Context, Error, FromMultiValue, Fuel, Function, IntoMultiValue, SequencePoll,
    Stack, String, Thread, ThreadMode, Value, Variadic,
};

use super::{
    thread::{Frame, LuaFrame, ThreadState},
    vm::run_vm,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorMode {
    /// There are no threads being run and the `Executor` must be restarted to do any work.
    Stopped,
    /// Lua has errored or returned (or yielded) values that must be taken to move the `Executor` to
    /// the `Stopped` (or `Suspended`) state.
    Result,
    /// There is an active thread in the `ThreadMode::Normal` state and it is can be run with
    /// `Executor::step`.
    Normal,
    /// The main thread has yielded and is waiting on being resumed.
    Suspended,
    /// The `Executor` is currently inside its own `Executor::step` function.
    Running,
}

#[derive(Debug, Copy, Clone, Error)]
#[error("bad executor mode: {found:?}, expected {expected:?}")]
pub struct BadExecutorMode {
    pub found: ExecutorMode,
    pub expected: ExecutorMode,
}

#[derive(Debug, Collect)]
#[collect(no_drop)]
pub struct ExecutorState<'gc> {
    thread_stack: vec::Vec<Thread<'gc>, MetricsAlloc<'gc>>,
    queued: Written,
}

pub type ExecutorInner<'gc> = RefLock<ExecutorState<'gc>>;

/// The entry-point for the Lua VM.
///
/// An `Executor` runs networks of [`Thread`]s that may depend on each other and may yield
/// control back and forth. All Lua code that is run is done so directly or indirectly by calling
/// [`Executor::step`].
///
/// # Re-entrancy
///
/// `Executor` is not meant to be used reentrantly: calling its methods from within a callback
/// which it itself is running (other than `Executor::mode`) fails, with a [`BadThreadMode`] or
/// [`BadExecutorMode`] error from the methods that return one and no effect from `stop` and
/// `restart`. Even with an independent `Executor`, cross-thread upvalues of a `Thread` that an
/// outer `Executor` has mutably borrowed read as nil and drop writes (a debug assertion in debug
/// builds).
///
/// `Executor`s are not meant to be used from callbacks at all, and `Executor`s should not be
/// nested. Instead, use the normal mechanisms for callbacks to call Lua code so that everything is
/// run by the same `Executor` which called the callback.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Executor<'gc>(Gc<'gc, ExecutorInner<'gc>>);

impl<'gc> PartialEq for Executor<'gc> {
    fn eq(&self, other: &Executor<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl<'gc> Eq for Executor<'gc> {}

impl<'gc> Hash for Executor<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Gc::as_ptr(self.0).hash(state)
    }
}

impl<'gc> Executor<'gc> {
    const VM_GRANULARITY: u32 = 64;

    const FUEL_PER_CALLBACK: i32 = 8;
    const FUEL_PER_SEQ_STEP: i32 = 4;
    const FUEL_PER_STEP: i32 = 4;

    /// Creates a new `Executor` with a stopped main thread.
    pub fn new(ctx: Context<'gc>) -> Self {
        Self::with_thread(&ctx, Thread::new(ctx))
    }

    // Creates an executor running the given thread, which must not be `Waiting` or `Running`.
    fn with_thread(mc: &Mutation<'gc>, thread: Thread<'gc>) -> Self {
        let mut thread_stack = vec::Vec::new_in(MetricsAlloc::new(mc));
        thread_stack.push(thread);
        Executor(Gc::new(
            mc,
            RefLock::new(ExecutorState {
                thread_stack,
                queued: Written::new(),
            }),
        ))
    }

    /// Creates a new `Executor` that begins running the given [`Thread`].
    ///
    /// If the provided thread is in [`ThreadMode::Waiting`] or [`ThreadMode::Running`], then this
    /// will return `Err(BadThreadMode)`.
    pub fn run(mc: &Mutation<'gc>, thread: Thread<'gc>) -> Result<Self, BadThreadMode> {
        check_runnable(thread)?;
        Ok(Self::with_thread(mc, thread))
    }

    pub fn from_inner(inner: Gc<'gc, ExecutorInner<'gc>>) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> Gc<'gc, ExecutorInner<'gc>> {
        self.0
    }

    /// Creates a new `Executor` with a new [`Thread`] running the given function.
    pub fn start(
        ctx: Context<'gc>,
        function: Function<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) -> Self {
        let thread = Thread::new(ctx);
        // A new thread is always stopped, so it can always be started.
        let _ = thread.start(ctx, function, args);
        Self::with_thread(&ctx, thread)
    }

    pub fn mode(self) -> ExecutorMode {
        if let Ok(state) = self.0.try_borrow() {
            if state.thread_stack.len() > 1 {
                ExecutorMode::Normal
            } else if let Some(main_thread) = state.thread_stack.first() {
                match main_thread.mode() {
                    ThreadMode::Stopped => ExecutorMode::Stopped,
                    ThreadMode::Result => ExecutorMode::Result,
                    ThreadMode::Normal => ExecutorMode::Normal,
                    ThreadMode::Suspended => ExecutorMode::Suspended,
                    ThreadMode::Waiting => {
                        // This should never happen from correct `Executor` / `Thread` use. In
                        // order for the main thread to be in the `Waiting` state with no thread
                        // being waited on, that thread must have been used by two `Executor`s at
                        // one time, and the *other* `Executor` must have moved it to the `Waiting`
                        // state.
                        //
                        // We call this `ExecutorMode::Normal` since the main thread is still not in
                        // some completed state, but calling `Executor::step` will never exit this
                        // mode (only forever return a `BadThreadMode` error).
                        ExecutorMode::Normal
                    }
                    ThreadMode::Running => ExecutorMode::Running,
                }
            } else {
                ExecutorMode::Stopped
            }
        } else {
            ExecutorMode::Running
        }
    }

    /// Runs the VM for a period of time controlled by the `fuel` parameter.
    ///
    /// The VM and callbacks will consume fuel as they run, and `Executor::step` will return as soon
    /// as `Fuel::should_continue()` returns false. A step that starts without fuel (also after the
    /// collection fuel charged at its start) does nothing and returns `false`; otherwise some
    /// minimal positive progress is made.
    ///
    /// Returns `false` if the method has exhausted its fuel, but there is more work to
    /// do, and returns `true` if no more progress can be made. If `true` is returned, then
    /// `Executor::mode()` will no longer be `ExecutorMode::Normal`.
    ///
    /// # Errors
    ///
    /// If a `Thread` being run by this `Executor` in an unexpected state, then this method will
    /// return a `BadThreadMode` error.
    ///
    /// If a `Thread` is currently in the stack of threads being run by an `Executor`, then that
    /// `Executor` expects to be the sole instance driving those threads to completion and expects
    /// that the state of these threads will not be externally changed. This rule cannot be violated
    /// from Lua or by normal `Rust` callbacks, only by purposefully misusing an `Executor` from
    /// Rust by, for example, setting a single `Thread` as the main thread of two `Executor`s
    /// at once or by manually calling [`Thread::take_result`] or [`Thread::reset`] on a thread
    /// currently being run by an `Executor`.
    ///
    /// This is considered "outside" of a normal Lua or Rust callback error since it cannot be
    /// triggered solely by Lua and likely indicates a bug in some Rust code, so this error is
    /// delivered through a separate channel than normal results and cannot be caught by Lua.
    ///
    /// If a resource limit is exceeded (see [`crate::limits`]) while the `Executor` is running, it
    /// stops at the end of the current operation: every thread it is running is reset, and the main
    /// thread finishes with the [`LimitError`](crate::LimitError) as its error.
    pub fn step(self, ctx: Context<'gc>, fuel: &mut Fuel) -> Result<bool, BadThreadMode> {
        let Ok(mut state) = self.0.try_borrow_mut(&ctx) else {
            return Err(RUNNING);
        };
        // Limits exceeded and fuel charged outside of this executor's steps (by the host) do not
        // affect it.
        let _ = ctx.take_limit_exceeded();
        let _ = ctx.take_fuel_debt();
        // A collection the instance finished before this step, for the garbage of its scripts
        // near the memory limit, is charged to the step (see `Lua::enter`).
        fuel.consume(ctx.take_collection_fuel());
        if !fuel.should_continue() {
            return Ok(false);
        }
        Ok(loop {
            let finished = self.run_once(ctx, &mut state, fuel)?;

            // Charge the fuel for the work done by the operation where the `Fuel` was not available.
            fuel.consume(ctx.take_fuel_debt());

            if let Some(error) = ctx
                .take_limit_exceeded()
                .or_else(|| ctx.check_memory(0).err())
            {
                // Discard any other limit exceeded while checking.
                let _ = ctx.take_limit_exceeded();
                fuel.consume(abort(&ctx, &mut state.thread_stack, error));
                break true;
            }

            if let Some(finished) = finished {
                break finished;
            }

            fuel.consume(Self::FUEL_PER_STEP);

            if !fuel.should_continue() {
                break false;
            }
        })
    }

    // Runs the top frame of the top thread once: returns whether the executor finished, or `None`
    // if it may go on.
    fn run_once(
        self,
        ctx: Context<'gc>,
        state: &mut ExecutorState<'gc>,
        fuel: &mut Fuel,
    ) -> Result<Option<bool>, BadThreadMode> {
        let Some(mut top_thread) = state.thread_stack.last().copied() else {
            return Ok(Some(true));
        };
        let mut res_thread = None;
        match top_thread.mode() {
            ThreadMode::Normal => {}
            ThreadMode::Running => return Err(RUNNING),
            ThreadMode::Stopped | ThreadMode::Suspended | ThreadMode::Result
                if state.thread_stack.len() == 1 =>
            {
                return Ok(Some(true));
            }
            ThreadMode::Result => {
                state.thread_stack.pop();
                res_thread = Some(top_thread);
                let Some(thread) = state.thread_stack.last().copied() else {
                    return Ok(Some(true));
                };
                top_thread = thread;
            }
            mode => {
                return Err(BadThreadMode {
                    found: mode,
                    expected: None,
                })
            }
        }

        let Ok(mut top_state) = top_thread.into_inner().try_borrow_mut(&ctx) else {
            return Err(RUNNING);
        };
        let top_state = &mut *top_state;
        if let Some(res_thread) = res_thread {
            let mode = top_state.mode();
            if mode != ThreadMode::Waiting {
                // Shenanigans have happened and the top thread has had its state externally
                // changed.
                return Err(BadThreadMode {
                    found: mode,
                    expected: Some(ThreadMode::Waiting),
                });
            }

            // A `Waiting` thread always has a `WaitThread` top frame.
            let waited = top_state.frames.pop();
            debug_assert!(matches!(waited, Some(Frame::WaitThread)));
            debug_assert_eq!(res_thread.mode(), ThreadMode::Result);
            // Take the results from the res_thread and return them to our top
            // thread.
            let Ok(mut res_state) = res_thread.into_inner().try_borrow_mut(&ctx) else {
                return Err(RUNNING);
            };
            match res_state.take_result() {
                Ok(vals) => {
                    let bottom = top_state.stack.len();
                    let pushed = top_state.push_values(ctx, vals);
                    if let Err(err) = pushed.and_then(|()| top_state.return_to(ctx, bottom)) {
                        top_state.stack.truncate(bottom);
                        top_state.frames.push(Frame::Error(err.into()));
                    }
                }
                Err(err) => {
                    top_state.frames.push(Frame::Error(err));
                }
            }
            drop(res_state);
        }

        if top_state.mode() == ThreadMode::Normal {
            match top_state.frames.pop() {
                Some(Frame::Callback { bottom, callback }) => {
                    fuel.consume(Self::FUEL_PER_CALLBACK);
                    match callback.call(
                        ctx,
                        Execution {
                            executor: self,
                            fuel,
                            threads: &state.thread_stack,
                            upper_frames: &top_state.frames,
                        },
                        Stack::new(ctx, &mut top_state.stack, bottom),
                    ) {
                        Ok(CallbackReturn::Return) => {
                            return_to(ctx, top_state, bottom);
                        }
                        Ok(CallbackReturn::Sequence(sequence)) => {
                            top_state.frames.push(Frame::Sequence {
                                bottom,
                                sequence,
                                pending_error: None,
                            });
                        }
                        Ok(CallbackReturn::Call { function, then }) => {
                            if let Some(sequence) = then {
                                top_state.frames.push(Frame::Sequence {
                                    bottom,
                                    sequence,
                                    pending_error: None,
                                });
                            }
                            top_state.push_call_or_error(ctx, bottom, function);
                        }
                        Ok(CallbackReturn::Yield { to_thread, then }) => {
                            if let Some(sequence) = then {
                                top_state.frames.push(Frame::Sequence {
                                    bottom,
                                    sequence,
                                    pending_error: None,
                                });
                            }
                            do_yield(ctx, &mut state.thread_stack, top_state, to_thread, bottom);
                        }
                        Ok(CallbackReturn::Resume { thread, then }) => {
                            if let Some(sequence) = then {
                                top_state.frames.push(Frame::Sequence {
                                    bottom,
                                    sequence,
                                    pending_error: None,
                                });
                            }
                            do_resume(ctx, &mut state.thread_stack, top_state, thread, bottom);
                        }
                        Err(err) => {
                            let err = locate_library_error(ctx, &top_state.frames, err);
                            top_state.stack.truncate(bottom);
                            top_state.frames.push(Frame::Error(err))
                        }
                    }
                }
                Some(Frame::Sequence {
                    bottom,
                    mut sequence,
                    pending_error,
                }) => {
                    fuel.consume(Self::FUEL_PER_SEQ_STEP);

                    let exec = Execution {
                        executor: self,
                        fuel,
                        threads: &state.thread_stack,
                        upper_frames: &top_state.frames,
                    };
                    let poll = if let Some(err) = pending_error {
                        sequence.error(
                            ctx,
                            exec,
                            err,
                            Stack::new(ctx, &mut top_state.stack, bottom),
                        )
                    } else {
                        sequence.poll(ctx, exec, Stack::new(ctx, &mut top_state.stack, bottom))
                    };

                    match poll {
                        Ok(SequencePoll::Pending) => {
                            top_state.frames.push(Frame::Sequence {
                                bottom,
                                sequence,
                                pending_error: None,
                            });
                        }
                        Ok(SequencePoll::Return) => {
                            return_to(ctx, top_state, bottom);
                        }
                        Ok(SequencePoll::Call {
                            function,
                            bottom: rel_bottom,
                        }) => {
                            top_state.frames.push(Frame::Sequence {
                                bottom,
                                sequence,
                                pending_error: None,
                            });
                            top_state.push_call_or_error(ctx, bottom + rel_bottom, function);
                        }
                        Ok(SequencePoll::TailCall(function)) => {
                            top_state.push_call_or_error(ctx, bottom, function);
                        }
                        Ok(SequencePoll::Yield {
                            to_thread,
                            bottom: rel_bottom,
                        }) => {
                            top_state.frames.push(Frame::Sequence {
                                bottom,
                                sequence,
                                pending_error: None,
                            });
                            do_yield(
                                ctx,
                                &mut state.thread_stack,
                                top_state,
                                to_thread,
                                bottom + rel_bottom,
                            );
                        }
                        Ok(SequencePoll::TailYield(to_thread)) => {
                            do_yield(ctx, &mut state.thread_stack, top_state, to_thread, bottom);
                        }
                        Ok(SequencePoll::Resume {
                            thread,
                            bottom: rel_bottom,
                        }) => {
                            top_state.frames.push(Frame::Sequence {
                                bottom,
                                sequence,
                                pending_error: None,
                            });
                            do_resume(
                                ctx,
                                &mut state.thread_stack,
                                top_state,
                                thread,
                                bottom + rel_bottom,
                            );
                        }
                        Ok(SequencePoll::TailResume(thread)) => {
                            do_resume(ctx, &mut state.thread_stack, top_state, thread, bottom);
                        }
                        Err(error) => {
                            let error = locate_library_error(ctx, &top_state.frames, error);
                            top_state.stack.truncate(bottom);
                            top_state.frames.push(Frame::Error(error));
                        }
                    }
                }
                Some(frame @ Frame::Lua { .. }) => {
                    top_state.frames.push(frame);

                    let lua_frame = LuaFrame {
                        state: top_state,
                        thread: top_thread,
                        fuel,
                    };
                    match run_vm(ctx, lua_frame, Self::VM_GRANULARITY) {
                        Err(err) => {
                            top_state.frames.push(Frame::Error(err.into()));
                        }
                        Ok(instructions_run) => {
                            fuel.consume(i32::try_from(instructions_run).unwrap_or(i32::MAX));
                        }
                    }
                }
                Some(Frame::Error(err)) => {
                    // A limit error raised outside a step (a thread resumed by the host with
                    // values it had no room for) stops the executor as one raised inside does,
                    // instead of unwinding where `pcall` could catch it.
                    if let Some(limit) = err.limit_error() {
                        ctx.exceed_limit(limit);
                        top_state.frames.push(Frame::Error(err));
                        return Ok(None);
                    }
                    // A `Normal` thread with an error frame has a frame below it.
                    match top_state.frames.pop() {
                        Some(Frame::Lua { bottom, .. }) => {
                            top_state.close_upvalues(&ctx, bottom);
                            top_state.stack.truncate(bottom);
                            top_state.frames.push(Frame::Error(err));
                        }
                        Some(Frame::Sequence {
                            bottom,
                            sequence,
                            pending_error,
                        }) => {
                            debug_assert!(pending_error.is_none());
                            top_state.frames.push(Frame::Sequence {
                                bottom,
                                sequence,
                                pending_error: Some(err),
                            });
                        }
                        // No other kind of frame can be below an error, but if one is, discard
                        // it and keep unwinding.
                        _ => {
                            top_state.frames.push(Frame::Error(err));
                        }
                    }
                }
                frame => {
                    // A `Normal` thread always has a Lua, callback, sequence or error top frame.
                    if let Some(frame) = frame {
                        top_state.frames.push(frame);
                    }
                    return Err(BadThreadMode {
                        found: top_state.mode(),
                        expected: Some(ThreadMode::Normal),
                    });
                }
            }
        }

        Ok(None)
    }

    pub fn take_result<T: FromMultiValue<'gc>>(
        self,
        ctx: Context<'gc>,
    ) -> Result<Result<T, Error<'gc>>, BadExecutorMode> {
        let mode = self.mode();
        let state = self.0.try_borrow();
        if let (ExecutorMode::Result, Ok(Some(main_thread))) =
            (mode, state.as_ref().map(|s| s.thread_stack.first()))
        {
            main_thread.take_result(ctx).map_err(|_| BadExecutorMode {
                found: mode,
                expected: ExecutorMode::Result,
            })
        } else {
            Err(BadExecutorMode {
                found: mode,
                expected: ExecutorMode::Result,
            })
        }
    }

    pub fn resume(
        self,
        ctx: Context<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) -> Result<(), BadExecutorMode> {
        let mode = self.mode();
        let state = self.0.try_borrow();
        if let (ExecutorMode::Suspended, Ok(Some(main_thread))) =
            (mode, state.as_ref().map(|s| s.thread_stack.first()))
        {
            main_thread.resume(ctx, args).map_err(|_| BadExecutorMode {
                found: mode,
                expected: ExecutorMode::Suspended,
            })
        } else {
            Err(BadExecutorMode {
                found: mode,
                expected: ExecutorMode::Suspended,
            })
        }
    }

    pub fn resume_err(self, mc: &Mutation<'gc>, error: Error<'gc>) -> Result<(), BadExecutorMode> {
        let mode = self.mode();
        let state = self.0.try_borrow();
        if let (ExecutorMode::Suspended, Ok(Some(main_thread))) =
            (mode, state.as_ref().map(|s| s.thread_stack.first()))
        {
            main_thread
                .resume_err(mc, error)
                .map_err(|_| BadExecutorMode {
                    found: mode,
                    expected: ExecutorMode::Suspended,
                })
        } else {
            Err(BadExecutorMode {
                found: mode,
                expected: ExecutorMode::Suspended,
            })
        }
    }

    /// Reset this `Executor` entirely, leaving it with a stopped main thread. Equivalent to
    /// creating a new executor with `Executor::new`.
    ///
    /// Does nothing if called from within the executor (from a callback it is running).
    pub fn stop(self, mc: &Mutation<'gc>) {
        let Ok(mut state) = self.0.try_borrow_mut(mc) else {
            return;
        };
        reset_threads(mc, &mut state.thread_stack);
    }

    /// Reset this `Executor` entirely and begins running the given thread.
    ///
    /// This is equivalent to creating a new executor with `Executor::run`.
    pub fn reset(self, mc: &Mutation<'gc>, thread: Thread<'gc>) -> Result<(), BadThreadMode> {
        check_runnable(thread)?;
        let Ok(mut state) = self.0.try_borrow_mut(mc) else {
            return Err(RUNNING);
        };
        reset_threads(mc, &mut state.thread_stack);
        state.thread_stack.clear();
        state.thread_stack.push(thread);
        Ok(())
    }

    /// Reset this `Executor` entirely and begins running the given function, equivalent to
    /// creating a new executor with `Executor::start`.
    ///
    /// Does nothing if called from within the executor (from a callback it is running).
    pub fn restart(
        self,
        ctx: Context<'gc>,
        function: Function<'gc>,
        args: impl IntoMultiValue<'gc>,
    ) {
        let Ok(mut state) = self.0.try_borrow_mut(&ctx) else {
            return;
        };
        reset_threads(&ctx, &mut state.thread_stack);
        if let Some(main_thread) = state.thread_stack.first() {
            // Once reset, the main thread can be started.
            let _ = main_thread.start(ctx, function, args);
        }
    }
}

// Resets every thread of a stopped executor (none can be running) and keeps its main thread: a
// coroutine left waiting on another would stay unresumable otherwise.
fn reset_threads<'gc>(
    mc: &Mutation<'gc>,
    thread_stack: &mut vec::Vec<Thread<'gc>, MetricsAlloc<'gc>>,
) {
    for thread in thread_stack.iter().rev() {
        let _ = thread.reset(mc);
    }
    thread_stack.truncate(1);
}

// An executor may run any thread that is not waiting or running.
fn check_runnable(thread: Thread<'_>) -> Result<(), BadThreadMode> {
    let mode = thread.mode();
    if matches!(mode, ThreadMode::Waiting | ThreadMode::Running) {
        return Err(BadThreadMode {
            found: mode,
            expected: Some(ThreadMode::Normal),
        });
    }
    Ok(())
}

// The error of running an executor or a thread that is already running.
const RUNNING: BadThreadMode = BadThreadMode {
    found: ThreadMode::Running,
    expected: None,
};

fn do_yield<'gc>(
    ctx: Context<'gc>,
    thread_stack: &mut vec::Vec<Thread<'gc>, MetricsAlloc<'gc>>,
    top_state: &mut ThreadState<'gc>,
    to_thread: Option<Thread<'gc>>,
    bottom: usize,
) {
    let bottom = bottom.min(top_state.stack.len());
    if let Some(to_thread) = to_thread {
        if let Err(err) = to_thread.resume(ctx, Variadic(top_state.stack.drain(bottom..))) {
            top_state.frames.push(Frame::Error(err.into()));
        } else {
            top_state.frames.push(Frame::Yielded);
            thread_stack.pop();
            thread_stack.push(to_thread);
        }
    } else {
        top_state.frames.push(Frame::Yielded);
        top_state.frames.push(Frame::Result { bottom });
    }
}

fn do_resume<'gc>(
    ctx: Context<'gc>,
    thread_stack: &mut vec::Vec<Thread<'gc>, MetricsAlloc<'gc>>,
    top_state: &mut ThreadState<'gc>,
    thread: Thread<'gc>,
    bottom: usize,
) {
    let bottom = bottom.min(top_state.stack.len());
    if let Err(err) = thread.resume(ctx, Variadic(top_state.stack.drain(bottom..))) {
        top_state.frames.push(Frame::Error(err.into()));
    } else {
        // Tail call the thread resume if we can.
        if top_state.frames.is_empty() {
            thread_stack.pop();
        } else {
            top_state.frames.push(Frame::WaitThread);
        }
        thread_stack.push(thread);
    }
}

fn return_to<'gc>(ctx: Context<'gc>, top_state: &mut ThreadState<'gc>, bottom: usize) {
    if let Err(err) = top_state.return_to(ctx, bottom) {
        top_state.frames.push(Frame::Error(err.into()));
    }
}

// Stops every thread in the stack after a limit error, leaving the main thread with the error as its
// result. Returns the fuel for the work this took (dropping every frame).
fn abort<'gc>(
    mc: &Mutation<'gc>,
    thread_stack: &mut vec::Vec<Thread<'gc>, MetricsAlloc<'gc>>,
    error: crate::LimitError,
) -> i32 {
    let mut frames = thread_stack.len();
    for thread in thread_stack.iter().rev() {
        frames = frames.saturating_add(thread.frame_count());
        // No thread of a running executor is borrowed between operations.
        let _ = thread.reset(mc);
    }
    thread_stack.truncate(1);
    if let Some(main_thread) = thread_stack.first() {
        main_thread.set_error(mc, error.into());
    }
    // DELIBERATE(cost): the memory the dropped frames held is freed uncharged, bounded by the
    // memory limit (limit case M2b).
    fuel_for(frames, cost::FRAMES)
}

/// Execution state passed to callbacks when they are run by an `Executor`.
pub struct Execution<'gc, 'a> {
    executor: Executor<'gc>,
    fuel: &'a mut Fuel,
    threads: &'a [Thread<'gc>],
    upper_frames: &'a [Frame<'gc>],
}

impl<'gc, 'a> Execution<'gc, 'a> {
    pub fn reborrow(&mut self) -> Execution<'gc, '_> {
        Execution {
            executor: self.executor,
            fuel: self.fuel,
            threads: self.threads,
            upper_frames: self.upper_frames,
        }
    }

    /// The fuel parameter passed to `Executor::step`.
    pub fn fuel(&mut self) -> &mut Fuel {
        self.fuel
    }

    /// The curently executing Thread.
    #[expect(
        clippy::unwrap_used,
        reason = "callbacks are only run by `Executor::step`, which always has a current thread"
    )]
    pub fn current_thread(&self) -> CurrentThread<'gc> {
        CurrentThread {
            thread: *self.threads.last().unwrap(),
            is_main: self.threads.len() == 1,
        }
    }

    /// The curently running Executor.
    ///
    /// Do not call methods on this from callbacks! This is provided only for identification
    /// purposes, so that callbacks can identify which executor that is currently executing them, or
    /// to store the pointer somewhere.
    pub fn executor(&self) -> Executor<'gc> {
        self.executor
    }

    /// Information about the function `level` levels up the call stack, if it is a Lua function:
    /// level 1 is the function that called the running callback, level 2 the function that called
    /// that one, and so on (as `luaL_where` levels). Returns `None` for other functions, such as
    /// sequences.
    pub fn lua_frame(&self, level: usize) -> Option<UpperLuaFrame<'gc>> {
        lua_frame_at(self.upper_frames, level)
    }

    /// If the function we are returning to is Lua, returns information about the Lua frame we are
    /// returning to.
    pub fn upper_lua_frame(&self) -> Option<UpperLuaFrame<'gc>> {
        UpperLuaFrame::of(self.upper_frames.last()?)
    }
}

pub struct CurrentThread<'gc> {
    pub thread: Thread<'gc>,
    pub is_main: bool,
}

pub struct UpperLuaFrame<'gc> {
    pub chunk_name: String<'gc>,
    pub current_function: FunctionRef<String<'gc>>,
    pub current_line: LineNumber,
}

impl<'gc> UpperLuaFrame<'gc> {
    // The position in a Lua frame, if `frame` is one.
    fn of(frame: &Frame<'gc>) -> Option<Self> {
        let Frame::Lua { closure, pc, .. } = frame else {
            return None;
        };
        let proto = closure.prototype();
        // The previously executed instruction for a callback should be the Call opcode.
        Some(UpperLuaFrame {
            chunk_name: proto.chunk_name,
            current_function: proto.reference,
            current_line: line_number(&proto.opcode_line_numbers, pc.saturating_sub(1)),
        })
    }

    /// `message` prefixed with this position, as `"chunk:line: message"`.
    pub(crate) fn locate(
        &self,
        ctx: Context<'gc>,
        message: &[u8],
    ) -> Result<String<'gc>, LimitError> {
        let mut located = format!(
            "{}:{}: ",
            self.chunk_name.display_lossy(),
            self.current_line
        )
        .into_bytes();
        located.extend_from_slice(message);
        ctx.check_string(located.len())?;
        Ok(ctx.new_string(located))
    }
}

// The Lua function `level` levels up `frames` (see `Execution::lua_frame`).
fn lua_frame_at<'gc>(frames: &[Frame<'gc>], level: usize) -> Option<UpperLuaFrame<'gc>> {
    // A level past every frame (a script may ask for any) finds nothing without a walk.
    if level > frames.len() {
        return None;
    }
    let frame = frames
        .iter()
        .rev()
        .filter(|f| matches!(f, Frame::Lua { .. } | Frame::Sequence { .. }))
        .nth(level.checked_sub(1)?)?;
    UpperLuaFrame::of(frame)
}

/// Turns an error message made by a library function (see `stdlib::args::error`) into a Lua string
/// with the position of the calling Lua function, as `luaL_where(L, 1)`: `frames` are the frames
/// below the callback or sequence that returned the error.
fn locate_library_error<'gc>(
    ctx: Context<'gc>,
    frames: &[Frame<'gc>],
    error: Error<'gc>,
) -> Error<'gc> {
    let Error::Runtime(err) = &error else {
        return error;
    };
    let Some(LibraryError(message)) = err.downcast::<LibraryError>() else {
        return error;
    };
    let located = match lua_frame_at(frames, 1) {
        Some(caller) => caller.locate(ctx, message),
        None => ctx
            .check_string(message.len())
            .map(|()| ctx.intern(message)),
    };
    match located {
        Ok(located) => Value::String(located).into(),
        Err(err) => err.into(),
    }
}

/// Find the line number of the opcode at index `opcode` from a function's sorted list of `(opcode
/// index, line number)` pairs, where each line number applies until the next pair.
pub(crate) fn line_number(
    opcode_line_numbers: &[(usize, LineNumber)],
    opcode: usize,
) -> LineNumber {
    let i = match opcode_line_numbers.binary_search_by_key(&opcode, |(opi, _)| *opi) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    opcode_line_numbers
        .get(i)
        .map(|&(_, line)| line)
        .unwrap_or(LineNumber(0))
}
