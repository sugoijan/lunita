//! Behaviour near the memory limit: operations that need no new memory are not refused, and the
//! collections finished at `Lua::enter` stay bounded.

use lunita::{Callback, CallbackReturn, Closure, Executor, ExternError, Fuel, Limits, Lua};

fn run(lua: &mut Lua, source: &str) -> Result<(), ExternError> {
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<()>(&executor)
}

#[test]
fn gsub_without_a_match_needs_no_memory() {
    let mut lua = Lua::core();
    lua.set_limits(Limits {
        memory: Some(2 << 20),
        string_len: None,
    });
    run(&mut lua, "S = ('x'):rep(1 << 20)").unwrap();
    // A copy of the subject would not fit below the limit; the result is the subject itself.
    run(
        &mut lua,
        "local s, n = S:gsub('zzz', '') assert(s == S and n == 0)
         s, n = S:gsub('zzz', function() end) assert(s == S and n == 0)",
    )
    .unwrap();
    assert!(run(&mut lua, "local s = S:gsub('x', 'y', 1) assert(#s == #S)").is_err());
}

#[test]
fn idle_enters_near_the_limit_collect_once() {
    let mut lua = Lua::core();
    let memory = 4 << 20;
    lua.set_limits(Limits {
        memory: Some(memory),
        string_len: None,
    });
    // A step to take once the memory is full (compiling then would be refused).
    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(ctx, None, b"return 1")?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    // Live objects up to the limit (the small ones are not refused one by one, the growth of
    // `T` is): the script stops once the memory is past the point allocations are refused.
    assert!(run(&mut lua, "T = {} for i = 1, 1 << 20 do T[i] = {} end").is_err());
    let live = lua.total_memory();
    assert!(live > memory / 4 * 3, "{live}");

    // Entering the arena without allocating finishes a collection at most once.
    for _ in 0..200 {
        lua.enter(|_| {});
    }
    let mut fuel = Fuel::with(i32::MAX);
    lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel))
        .unwrap();
    // The step pays for the collections: one costs about the memory it went through at 1,024
    // bytes per fuel.
    let used = i64::from(i32::MAX - fuel.remaining());
    let one = (live / 1024) as i64;
    assert!(
        used < 3 * one + 10_000,
        "{used} fuel, one collection is {one}"
    );
}

#[test]
fn string_literals_are_budgeted_before_interning() {
    let mut lua = Lua::core();
    let memory = 1 << 20;
    lua.set_limits(Limits {
        memory: Some(memory),
        string_len: None,
    });
    run(&mut lua, "S = ('a'):rep(400 * 1024)").unwrap();
    // The literal would not fit: compiling fails before it is interned.
    let _ = run(
        &mut lua,
        "local f, err = load(\"return '\" .. S .. \"'\") assert(not f)",
    );
    assert!(lua.total_memory() <= memory, "{} bytes", lua.total_memory());
    // A parse error echoes at most 256 bytes of the token, whichever token was expected.
    let _ = run(
        &mut lua,
        "local f, err = load(\"'\" .. S:sub(1, 1000) .. \"'\") assert(not f and #err < 400, #err)",
    );
    let _ = run(
        &mut lua,
        "local f, err = load(\"for i = 1, 2 '\" .. S:sub(1, 1000) .. \"'\") assert(not f and #err < 400, #err)",
    );
}

#[test]
fn host_strings_and_tables_past_the_limit_stop_the_script() {
    let mut lua = Lua::core();
    let memory = 1 << 20;
    lua.set_limits(Limits {
        memory: Some(memory),
        string_len: None,
    });
    lua.enter(|ctx| {
        ctx.set_global(
            "big_string",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                stack.replace(ctx, ctx.intern(&vec![b'x'; 2 << 20]));
                Ok(CallbackReturn::Return)
            }),
        )
        .unwrap();
        ctx.set_global(
            "big_table",
            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                stack.replace(ctx, (0..1_000_000i64).collect::<Vec<_>>());
                Ok(CallbackReturn::Return)
            }),
        )
        .unwrap();
    });
    assert!(run(&mut lua, "local s = big_string() while true do end").is_err());
    assert!(run(&mut lua, "local t = big_table() while true do end").is_err());
    // The table stopped growing once refused.
    assert!(
        lua.total_memory() <= memory + (1 << 20),
        "{} bytes",
        lua.total_memory()
    );
}
