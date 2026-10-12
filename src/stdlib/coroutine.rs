use crate::{
    BoxSequence, Callback, CallbackReturn, Context, Function, LimitError, Table, Thread,
    ThreadMode, Value,
};

use super::{
    args::{arg_error, is_lua51, type_error},
    base::PCall,
};

pub fn load_coroutine<'gc>(ctx: Context<'gc>) -> Result<(), LimitError> {
    let coroutine = Table::new(&ctx);

    coroutine.set_field(
        ctx,
        "create",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            // A function (in Lua 5.1, a Lua function), as PUC-Rio Lua requires.
            let function = match stack.get(0) {
                Value::Function(Function::Closure(f)) => Function::Closure(f),
                Value::Function(f) if !is_lua51(ctx) => f,
                _ if is_lua51(ctx) => return Err(arg_error("create", 0, "Lua function expected")),
                _ => return Err(type_error(&stack, "create", 0, "function")),
            };
            let thread = Thread::new(ctx);
            thread.start_suspended(&ctx, function)?;
            stack.replace(ctx, thread);
            Ok(CallbackReturn::Return)
        }),
    )?;

    coroutine.set_field(
        ctx,
        "resume",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread: Thread = stack.from_front(ctx)?;
            Ok(CallbackReturn::Resume {
                thread,
                then: Some(BoxSequence::new(&ctx, PCall)),
            })
        }),
    )?;

    coroutine.set_field(
        ctx,
        "continue",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread: Thread = stack.from_front(ctx)?;
            Ok(CallbackReturn::Resume { thread, then: None })
        }),
    )?;

    coroutine.set_field(
        ctx,
        "status",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread: Thread = stack.consume(ctx)?;
            stack.replace(
                ctx,
                match thread.mode() {
                    ThreadMode::Stopped => "dead",
                    ThreadMode::Running => "running",
                    // Waiting on the thread it resumed, as PUC-Rio Lua's "normal" coroutine is.
                    ThreadMode::Waiting | ThreadMode::Normal => "normal",
                    ThreadMode::Result | ThreadMode::Suspended => "suspended",
                },
            );
            Ok(CallbackReturn::Return)
        }),
    )?;

    coroutine.set_field(
        ctx,
        "yield",
        Callback::from_fn(&ctx, |_, _, _| {
            Ok(CallbackReturn::Yield {
                to_thread: None,
                then: None,
            })
        }),
    )?;

    coroutine.set_field(
        ctx,
        "yieldto",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let thread: Thread = stack.from_front(ctx)?;
            Ok(CallbackReturn::Yield {
                to_thread: Some(thread),
                then: None,
            })
        }),
    )?;

    coroutine.set_field(
        ctx,
        "running",
        Callback::from_fn(&ctx, |ctx, exec, mut stack| {
            let current_thread = exec.current_thread();
            if is_lua51(ctx) {
                // Lua 5.1 returns the running coroutine alone, nil for the main thread.
                if current_thread.is_main {
                    stack.replace(ctx, Value::Nil);
                } else {
                    stack.replace(ctx, current_thread.thread);
                }
            } else {
                stack.replace(ctx, (current_thread.thread, current_thread.is_main));
            }
            Ok(CallbackReturn::Return)
        }),
    )?;

    ctx.set_global("coroutine", coroutine)?;
    Ok(())
}
