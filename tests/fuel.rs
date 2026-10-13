use lunita::{
    Callback, CallbackReturn, Closure, Compat, Executor, ExecutorMode, ExternError, Fuel, Lua,
};

#[test]
fn test_interrupt() -> Result<(), ExternError> {
    let mut lua = Lua::core();

    lua.try_enter(|ctx| {
        let callback = Callback::from_fn(&ctx, |_, mut exec, _| {
            exec.fuel().interrupt();
            Ok(CallbackReturn::Return)
        });
        ctx.set_global("callback", callback).unwrap();
        Ok(())
    })?;

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, &b"callback(); abort()"[..])?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    lua.enter(|ctx| {
        let executor = ctx.fetch(&executor);
        let mut fuel = Fuel::with(i32::MAX);
        assert!(!executor.step(ctx, &mut fuel).unwrap());
        assert!(fuel.is_interrupted());
        assert!(executor.mode() == ExecutorMode::Normal)
    });

    Ok(())
}

// Runs a chunk to completion in steps of 1,000 fuel, returning the total fuel used.
fn fuel_used(source: &str) -> i64 {
    fuel_used_with(Compat::Default, source)
}

fn fuel_used_with(compat: Compat, source: &str) -> i64 {
    let mut lua = Lua::core();
    lua.set_compat(compat);
    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(ctx, None, source.as_bytes())?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    let mut used = 0;
    loop {
        let mut fuel = Fuel::with(1_000);
        let finished = lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel).unwrap());
        used += i64::from(1_000 - fuel.remaining());
        if finished {
            break;
        }
    }
    lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<()>(ctx)?)
        .unwrap();
    used
}

#[test]
fn test_fuel_in_proportion_to_work() {
    // Doubling a string 21 times builds 4 MiB of strings in 21 instructions.
    let concat = fuel_used("local s = 'x' for i = 1, 21 do s = s .. s end");
    assert!(concat > (4 << 20) / 256, "{concat}");

    // Growing a table to a million entries.
    let table = fuel_used("local t = {} for i = 1, 1e6 do t[i] = i end");
    let loop_only = fuel_used("local t = {} for i = 1, 1e6 do end");
    assert!(table - loop_only > 1_000_000 / 32, "{table} {loop_only}");

    // Skipping removed entries in `next`.
    let next = fuel_used(
        "local t = {} for i = 1, 1e4 do t['k' .. i] = i end \
         for i = 1, 1e4 - 1 do t['k' .. i] = nil end \
         for i = 1, 100 do assert(next(t) == 'k10000') end",
    );
    assert!(next > 100 * 10_000 / 32, "{next}");

    // `load` charges for the work of compiling.
    let load = fuel_used(
        "local src = {} for i = 1, 1000 do src[i] = 'x = 1 ' end src = table.concat(src) \
         for i = 1, 10 do assert(load(src)) end",
    );
    assert!(load > 10 * 3_000 / 2, "{load}");
}

#[test]
fn lua51_pattern_end_found_once() {
    // In Lua 5.1 mode a pattern ends at its first zero byte, which is looked for once per
    // `gmatch`, `gsub`, `find` or `match` call, not once per match or resume.
    let long = "local p = 'x' .. ('\\0'):rep(100000)";
    let short = "local p = 'x'";
    for (body, slack) in [
        ("for w in ('x'):rep(2000):gmatch(p) do end", 2000),
        (
            "local s = ('x'):rep(2000) for i = 1, 20 do s:gsub(p, 'y') end",
            20,
        ),
        (
            "local s = ('x'):rep(2000) for i = 1, 20 do s:find(p) s:match(p) end",
            40,
        ),
    ] {
        let with_long = fuel_used_with(Compat::Lua51, &format!("{long} {body}"));
        let with_short = fuel_used_with(Compat::Lua51, &format!("{short} {body}"));
        // Each operation scans the long pattern once (100,000 bytes, 391 fuel).
        assert!(
            with_long <= with_short + slack * 400,
            "{body}: {with_long} with a long pattern, {with_short} with a short one"
        );
    }
}

// Runs a chunk in steps of 1,000 fuel, returning how many steps it took.
fn steps_used(source: &str) -> usize {
    let mut lua = Lua::core();
    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(ctx, None, source.as_bytes())?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    let mut steps = 0;
    loop {
        let mut fuel = Fuel::with(1_000);
        steps += 1;
        if lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel).unwrap()) {
            break;
        }
    }
    lua.try_enter(|ctx| ctx.fetch(&executor).take_result::<()>(ctx)?)
        .unwrap();
    steps
}

#[test]
fn string_to_number_conversions_are_charged() {
    // A string operand is scanned to convert it: a megabyte of spaces is about 4,000 fuel.
    let big = "local big = (' '):rep(1 << 20) .. '2'";
    for body in [
        "for j = 1, 10 do for i = 1, big do end end",
        "for j = 1, 10 do math.floor(big) end",
        "for j = 1, 10 do string.sub('abc', big) end",
        "local t = setmetatable({}, {__mod = function() return 0 end}) for j = 1, 10 do local _ = big % 1 end",
    ] {
        let used = fuel_used(&format!("{big} {body}"));
        assert!(used >= 10 * 4_000, "{body}: {used}");
    }
}

#[test]
fn gsub_charges_its_template_per_match() {
    // An empty expansion of a 256 KiB template still scans it: 1,000 fuel a match.
    let used =
        fuel_used("local rep = ('%1'):rep(1 << 17) local s = ('a'):rep(100) s:gsub('()', rep)");
    assert!(used >= 100 * 1_000, "{used}");
}

#[test]
fn table_constructors_charge_their_room() {
    let fields = (1..=255)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let used = fuel_used(&format!("for i = 1, 1000 do local t = {{{fields}}} end"));
    // 255 values of 16 bytes and 255 slots: about 12 fuel a constructor, over the 255 moves.
    assert!(used >= 1000 * (255 + 12), "{used}");
}

#[test]
fn concatenation_through_a_metamethod_suspends_on_fuel() {
    // 50 strings of 100 KiB joined through `__concat`: 125 MiB copied, about 500,000 fuel of it.
    let source = "local t = setmetatable({}, {__concat = function(a, b) return b end})
                  local s = ('x'):rep(100 * 1024)
                  local r = t .. s .. s .. s .. s .. s .. s .. s .. s .. s .. s
                    .. s .. s .. s .. s .. s .. s .. s .. s .. s .. s
                    .. s .. s .. s .. s .. s .. s .. s .. s .. s .. s
                    .. s .. s .. s .. s .. s .. s .. s .. s .. s .. s
                    .. s .. s .. s .. s .. s .. s .. s .. s .. s .. s";
    let steps = steps_used(source);
    assert!(steps >= 40, "{steps} steps");
}
