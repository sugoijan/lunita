use lunita::{
    Closure, CompilerError, Executor, ExternError, FunctionPrototype, Lua, Table,
    compiler::{
        BudgetError, CompileBudget, CompileErrorKind, CompileLimits, HOST_LOAD_MAX_WORK,
        LOAD_MAX_WORK, ParseErrorKind,
    },
    stdlib,
};

// Compiles `source` with `work` units of compiler work at most, returning the work it took.
fn compile_within(source: &[u8], work: Option<u64>) -> (u64, Result<(), CompilerError>) {
    Lua::core().enter(|ctx| {
        let budget = CompileBudget::new(CompileLimits::for_context(ctx, work));
        let result = FunctionPrototype::compile_with_budget(ctx, "chunk", source, &budget);
        (budget.work(), result.map(|_| ()))
    })
}

// The three loaders that compile with the default work limit of the host.
fn host_loaders(source: &[u8]) -> [Result<(), CompilerError>; 3] {
    Lua::core().enter(|ctx| {
        [
            FunctionPrototype::compile(ctx, "chunk", source).map(|_| ()),
            Closure::load(ctx, Some("chunk"), source).map(|_| ()),
            Closure::load_with_env(ctx, Some("chunk"), source, Table::new(&ctx)).map(|_| ()),
        ]
    })
}

// A chunk whose cost is in resolving names: every call of the global `g` compares its name with
// the 250 locals in scope, 250 units of work for 4 bytes of source.
fn many_lookups(calls: usize) -> Vec<u8> {
    let locals = (0..250)
        .map(|i| format!("a{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("local {locals}\n{}", "g() ".repeat(calls)).into_bytes()
}

// The host loaders are not held to the limit of Lua's `load`, which ordinary scripts of a few
// tens of kilobytes exceed.
#[test]
fn host_loaders_accept_chunk_over_lua_load_limit() {
    let block = "do\n    local c = a + b\n    x = c * g(a, b)\nend\n";
    let source = block.repeat(1000).into_bytes();
    assert!(source.len() < 50_000);

    let (work, result) = compile_within(&source, None);
    result.unwrap();
    assert!(
        work > LOAD_MAX_WORK && work < HOST_LOAD_MAX_WORK,
        "{work} units"
    );
    let (_, result) = compile_within(&source, Some(LOAD_MAX_WORK));
    assert!(result.unwrap_err().is_work_limit());

    for result in host_loaders(&source) {
        result.unwrap();
    }
}

// Under 200 KiB of source can take more work than megabytes of ordinary code: the host loaders
// refuse it, in the compiler rather than the parser, and an explicit budget compiles it.
#[test]
fn host_loaders_reject_chunk_over_work_limit() {
    let source = many_lookups(HOST_LOAD_MAX_WORK as usize / 250 + 1);
    assert!(source.len() < 200_000);

    let (work, result) = compile_within(&source, None);
    result.unwrap();
    assert!(work > HOST_LOAD_MAX_WORK, "{work} units");

    for result in host_loaders(&source) {
        let error = result.unwrap_err();
        assert!(error.is_work_limit(), "{error:?}");
        let CompilerError::Compilation(error) = error else {
            panic!("the parser ran out of work: {error:?}");
        };
        assert!(matches!(
            error.kind,
            CompileErrorKind::Budget(BudgetError::Work)
        ));
        assert!(
            error
                .to_string()
                .ends_with("chunk is too complex to compile")
        );
    }
}

// Reading the source is charged before any of it is parsed, so the limit is exact for a chunk of
// whitespace, and the parser reports it.
#[test]
fn work_limit_in_the_parser() {
    let at_limit = vec![b' '; LOAD_MAX_WORK as usize * CompileBudget::SOURCE_BYTES_PER_WORK];
    let (work, result) = compile_within(&at_limit, Some(LOAD_MAX_WORK));
    result.unwrap();
    assert_eq!(work, LOAD_MAX_WORK);

    let over_limit = vec![b' '; at_limit.len() + CompileBudget::SOURCE_BYTES_PER_WORK];
    let (_, result) = compile_within(&over_limit, Some(LOAD_MAX_WORK));
    let error = result.unwrap_err();
    assert!(error.is_work_limit(), "{error:?}");
    let CompilerError::Parsing(error) = error else {
        panic!("the compiler ran out of work: {error:?}");
    };
    assert!(matches!(
        error.kind,
        ParseErrorKind::Budget(BudgetError::Work)
    ));

    // The same chunk is within the limit of the host loaders.
    for result in host_loaders(&over_limit) {
        result.unwrap();
    }
}

// The work limit in the compiler, one unit under and at the work a chunk takes.
#[test]
fn work_limit_in_the_compiler() {
    let source = many_lookups(100);
    let (work, result) = compile_within(&source, None);
    result.unwrap();

    let (_, result) = compile_within(&source, Some(work));
    result.unwrap();
    let (_, result) = compile_within(&source, Some(work - 1));
    let error = result.unwrap_err();
    assert!(error.is_work_limit(), "{error:?}");
    assert!(matches!(error, CompilerError::Compilation(_)));
}

#[test]
fn other_errors_are_not_the_work_limit() {
    for source in ["x =", "goto nowhere", "local x <const> = 1 x = 2"] {
        let (_, result) = compile_within(source.as_bytes(), Some(LOAD_MAX_WORK));
        let error = result.unwrap_err();
        assert!(!error.is_work_limit(), "{source}: {error:?}");
    }
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
