use lunita::{
    Closure, Executor, ExternError, FunctionPrototype, Lua, Table,
    compiler::{BudgetError, CompileBudget, LOAD_MAX_WORK, ParseError, ParseErrorKind},
    stdlib,
};

#[test]
fn host_load_accepts_source_at_work_limit() -> Result<(), ExternError> {
    let source = vec![
        b' ';
        LOAD_MAX_WORK as usize * CompileBudget::SOURCE_BYTES_PER_WORK
    ];
    Lua::core().try_enter(|ctx| {
        Closure::load(ctx, None, &source)?;
        Ok(())
    })
}

#[test]
fn host_load_rejects_source_over_work_limit() {
    let source = vec![
        b' ';
        (LOAD_MAX_WORK as usize + 1) * CompileBudget::SOURCE_BYTES_PER_WORK
    ];
    let error = Lua::core()
        .try_enter(|ctx| {
            Closure::load(ctx, None, &source)?;
            Ok(())
        })
        .unwrap_err();
    let parse_error = error
        .root_cause()
        .downcast_ref::<ParseError>()
        .expect("work limit must be reported as a parse error");
    assert!(matches!(
        &parse_error.kind,
        ParseErrorKind::Budget(BudgetError::Work)
    ));
}

// Runs `source` in a table holding `names` from the globals and a `load` bound to that table,
// with the global `privileged` set and `visible` set in the table only. Returns the script's
// result and whether the globals were left without a `leaked` variable.
fn run_restricted(names: &[&'static str], source: &str) -> Result<(bool, bool), ExternError> {
    let mut lua = Lua::core();

    // Helpers of the host, compiled with the globals as their environment: one calls its argument
    // from its own frame, the other with a tail call.
    let prelude = lua.try_enter(|ctx| {
        let closure = Closure::load(
            ctx,
            Some("prelude"),
            br#"
                function apply(f, ...)
                    local results = { f(...) }
                    return results[1], results[2]
                end
                function tail_apply(f, ...)
                    return f(...)
                end
            "#,
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<()>(&prelude)?;

    let executor = lua.try_enter(|ctx| {
        ctx.set_global("privileged", true)?;

        let restricted = Table::new(&ctx);
        restricted.set(ctx, "visible", "restricted")?;
        restricted.set(ctx, "load", stdlib::load_callback(ctx, restricted))?;
        for &name in names {
            restricted.set(ctx, name, ctx.get_global_value(name))?;
        }

        let closure =
            Closure::load_with_env(ctx, Some("restricted"), source.as_bytes(), restricted)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    let result = lua.execute::<bool>(&executor)?;
    let globals_clean = lua.enter(|ctx| ctx.get_global_value("leaked").is_nil());
    Ok((result, globals_clean))
}

#[test]
fn bound_load_uses_its_environment() -> Result<(), ExternError> {
    let (result, globals_clean) = run_restricted(
        &["assert", "pcall"],
        r#"
            assert(load("return privileged")() == nil, "direct call")
            assert(load("return visible")() == "restricted", "reads its environment")

            local loader = load
            local function nested()
                return loader("return privileged")()
            end
            assert(nested() == nil, "alias in a nested function")

            local ok, loaded = pcall(load, "return privileged")
            assert(ok and loaded() == nil, "through pcall")

            local pieces = { "return ", "privileged" }
            local index = 0
            local streamed = load(function()
                index = index + 1
                return pieces[index]
            end)
            assert(streamed() == nil, "reader function")

            -- A function of the chunk keeps the environment of the chunk.
            local inner = load("return function() return privileged, visible end")()
            local p, v = inner()
            assert(p == nil and v == "restricted", "function of a loaded chunk")

            load("leaked = 1")()
            assert(leaked == 1, "writes its environment")
            return true
        "#,
    )?;
    assert!(result);
    assert!(globals_clean);
    Ok(())
}

// The environment does not depend on the frames between the script and `load`: `load` as the body
// of a coroutine has no Lua frame below it in its thread.
#[test]
fn bound_load_as_coroutine_body() -> Result<(), ExternError> {
    let (result, globals_clean) = run_restricted(
        &["assert", "pcall", "coroutine"],
        r#"
            local ok, loaded = coroutine.resume(coroutine.create(load), "return privileged")
            assert(ok and loaded() == nil, "load as the coroutine body")

            local co = coroutine.create(pcall)
            local ok, called, loaded = coroutine.resume(co, load, "return privileged")
            assert(ok and called and loaded() == nil, "pcall as the coroutine body")

            local co = coroutine.create(function(source)
                return load(source)
            end)
            local ok, loaded = coroutine.resume(co, "leaked = 1")
            assert(ok, "load in a coroutine")
            loaded()
            assert(leaked == 1, "writes its environment")
            return true
        "#,
    )?;
    assert!(result);
    assert!(globals_clean);
    Ok(())
}

// Nor on whose frame is nearest: a function of the host with the globals as its environment,
// shared with the script, calls `load` for it.
#[test]
fn bound_load_through_host_function() -> Result<(), ExternError> {
    let (result, globals_clean) = run_restricted(
        &["assert", "pcall", "apply", "tail_apply"],
        r#"
            assert(apply(load, "return privileged")() == nil, "called by a host function")
            assert(tail_apply(load, "return privileged")() == nil, "tail called by a host function")
            assert(apply(pcall, load, "return privileged") == true, "host function and pcall")
            local _, loaded = apply(pcall, load, "return privileged")
            assert(loaded() == nil, "host function and pcall")

            apply(load, "leaked = 1")()
            assert(leaked == 1, "writes its environment")
            return true
        "#,
    )?;
    assert!(result);
    assert!(globals_clean);
    Ok(())
}

#[test]
fn load_honors_explicit_environment() -> Result<(), ExternError> {
    let (result, globals_clean) = run_restricted(
        &["assert", "pcall"],
        r#"
            local explicit = { allowed = true }
            assert(load("return allowed", nil, nil, explicit)() == true, "explicit environment")
            assert(load("return visible", nil, nil, explicit)() == nil, "explicit environment only")

            -- A `load` reached through an explicit environment keeps its own default.
            local nested = load("return load('return visible')()", nil, nil, { load = load })
            assert(nested() == "restricted", "nested load")

            -- An explicit nil is an environment too.
            assert(load("return 1", nil, nil, nil)() == 1, "nil environment, no globals used")
            assert(not pcall(load("return visible", nil, nil, nil)), "nil environment")

            load("leaked = 1", nil, nil, explicit)()
            assert(explicit.leaked == 1 and leaked == nil, "writes the explicit environment")
            return true
        "#,
    )?;
    assert!(result);
    assert!(globals_clean);
    Ok(())
}

#[test]
fn base_load_defaults_to_globals() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        ctx.set_global("visible", true)?;
        let closure = Closure::load(
            ctx,
            Some("globals"),
            br#"
                local explicit = { allowed = true }
                return load("return visible")() == true
                    and load("return visible", nil, nil, explicit)() == nil
                    and load("return allowed", nil, nil, explicit)() == true
            "#,
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert!(lua.execute::<bool>(&executor)?);
    Ok(())
}

// The caller of the base library's `load` needs no environment of its own: a chunk that uses no
// global has no `_ENV` upvalue, and `Closure::new` makes it without an environment.
#[test]
fn base_load_from_closure_without_environment() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let proto = FunctionPrototype::compile(
            ctx,
            "no-environment",
            br#"
                local load = ...
                local function nested()
                    return load("return tostring(1)")()
                end
                return load("return tostring(1)")() == "1" and nested() == "1"
            "#,
        )?;
        assert!(proto.upvalues.is_empty());
        let closure = Closure::new(&ctx, proto, None)?;
        let load = ctx.get_global_value("load");
        Ok(ctx.stash(Executor::start(ctx, closure.into(), load)))
    })?;
    assert!(lua.execute::<bool>(&executor)?);
    Ok(())
}

// As in PUC-Rio Lua, the base library's `load` is bound to the globals wherever it is called
// from: a host must not put it in a restricted environment (it uses `stdlib::load_callback`).
#[test]
fn base_load_is_bound_to_globals() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        ctx.set_global("privileged", true)?;
        let restricted = Table::new(&ctx);
        restricted.set(ctx, "load", ctx.get_global_value("load"))?;
        let closure = Closure::load_with_env(
            ctx,
            Some("restricted"),
            b"return load('return privileged')() == true",
            restricted,
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    assert!(lua.execute::<bool>(&executor)?);
    Ok(())
}
