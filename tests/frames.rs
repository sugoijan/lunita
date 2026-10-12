//! Frames and stacks under a script's control: recursion is a catchable "stack overflow",
//! implicit stack growth is checked, and the executor's threads are reset together.

use piccolo::{
    Callback, CallbackReturn, Closure, Compat, Executor, ExternError, Fuel, Limits, Lua, Value,
    Variadic,
};

fn run<R: for<'gc> piccolo::FromMultiValue<'gc>>(
    lua: &mut Lua,
    source: &str,
) -> Result<R, ExternError> {
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<R>(&executor)
}

#[test]
fn recursion_without_values_is_a_stack_overflow() {
    // A call that keeps no values on the stack only grows the frames.
    for (compat, frames) in [(Compat::Default, 200_000), (Compat::Lua51, 20_000)] {
        let mut lua = Lua::core();
        lua.set_compat(compat);
        let (ok, message, depth): (bool, String, i64) = run(
            &mut lua,
            "local n = 0
             local function rec() n = n + 1 return 1 + rec() end
             local ok, message = pcall(rec)
             return ok, tostring(message), n",
        )
        .unwrap();
        assert!(!ok && message.ends_with("stack overflow"), "{message}");
        assert!(depth <= frames && depth > frames - 10, "{depth}");
    }
}

#[test]
fn tail_calls_through_call_metamethods_return_values_once() {
    let mut lua = Lua::core();
    let (count, first, last): (i64, i64, i64) = run(
        &mut lua,
        "local big = ('x'):rep(50000) .. 'y'
         local t = setmetatable({}, {__call = function(self, n)
             if n == 0 then return string.byte(big, 1, 50001) end
             return self(n - 1)
         end})
         local r = {t(10000)}
         return #r, r[1], r[50001]",
    )
    .unwrap();
    assert_eq!((count, first, last), (50001, 120, 121));
}

#[test]
fn implicit_stack_pushes_are_checked_against_the_memory_limit() {
    let mut lua = Lua::core();
    let memory = 2 << 20;
    lua.set_limits(Limits {
        memory: Some(memory),
        string_len: None,
    });
    lua.enter(|ctx| {
        // A callback pushing values one by one, without reserving them first.
        ctx.set_global(
            "flood",
            Callback::from_fn(&ctx, |_, _, mut stack| {
                for i in 0..1_000_000 {
                    stack.push_back(Value::Integer(i));
                }
                Ok(CallbackReturn::Return)
            }),
        )
        .unwrap();
    });
    let result = run::<()>(&mut lua, "local t = {flood()}");
    assert!(result.is_err(), "the flood was not stopped");
    assert!(lua.total_memory() <= memory, "{} bytes", lua.total_memory());
}

#[test]
fn extending_a_stack_by_reference_is_checked_against_the_memory_limit() {
    let mut lua = Lua::core();
    let memory = 2 << 20;
    lua.set_limits(Limits {
        memory: Some(memory),
        string_len: None,
    });
    lua.enter(|ctx| {
        ctx.set_global(
            "flood",
            Callback::from_fn(&ctx, |_, _, mut stack| {
                let values = vec![Value::Integer(1); 100_000];
                for _ in 0..10 {
                    // The by-reference `Extend` of a `&mut Stack`.
                    let mut by_ref = &mut stack;
                    Extend::<&Value>::extend(&mut by_ref, &values);
                }
                Ok(CallbackReturn::Return)
            }),
        )
        .unwrap();
    });
    let result = run::<()>(&mut lua, "local t = {flood()}");
    assert!(result.is_err(), "the flood was not stopped");
    assert!(lua.total_memory() <= memory, "{} bytes", lua.total_memory());
}

#[test]
fn a_limit_error_from_a_host_resume_stops_the_executor() {
    let mut lua = Lua::core();
    lua.set_limits(Limits {
        memory: Some(1 << 20),
        string_len: None,
    });
    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                b"local ok, message = pcall(coroutine.yield) return ok, tostring(message)",
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    // Run until the yield; taking the yielded values suspends the executor.
    let mut fuel = Fuel::with(1_000_000);
    lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel))
        .unwrap();
    lua.enter(|ctx| {
        let yielded = ctx
            .fetch(&executor)
            .take_result::<Variadic<Vec<Value>>>(ctx)
            .unwrap();
        assert!(yielded.is_ok());
    });
    // Resuming with more values than the memory limit allows fails outside any step.
    lua.enter(|ctx| {
        ctx.fetch(&executor)
            .resume(ctx, Variadic(vec![Value::Nil; 200_000]))
            .unwrap()
    });
    let result = lua.execute::<(bool, String)>(&executor);
    match result {
        Err(err) => assert!(err.to_string().contains("memory limit"), "{err}"),
        Ok((ok, _)) => panic!("pcall caught the limit error: ok = {ok}"),
    }
}

#[test]
fn restart_resets_waiting_coroutines() {
    let mut lua = Lua::core();
    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                b"co = coroutine.create(function()
                      coroutine.resume(coroutine.create(function() while true do coroutine.yield() end end))
                  end)
                  coroutine.resume(co)",
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    // Stop while `co` waits on the inner coroutine, which yields.
    let mut fuel = Fuel::with(1_000);
    lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel))
        .unwrap();
    lua.enter(|ctx| {
        let f = Closure::load(
            ctx,
            None,
            b"return coroutine.status(co), coroutine.resume(co)",
        )
        .unwrap();
        ctx.fetch(&executor).restart(ctx, f.into(), ());
    });
    let (status, resumed): (String, bool) = lua.execute(&executor).unwrap();
    assert_eq!(status, "dead");
    assert!(!resumed);
}
