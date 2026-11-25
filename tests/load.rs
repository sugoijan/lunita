use lunita::{Closure, Executor, ExternError, Lua, Table};

#[test]
fn load_inherits_restricted_closure_environment() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        ctx.set_global("privileged", true)?;

        let restricted = Table::new(&ctx);
        restricted.set(ctx, "load", ctx.get_global_value("load"))?;
        restricted.set(ctx, "pcall", ctx.get_global_value("pcall"))?;

        let closure = Closure::load_with_env(
            ctx,
            Some("restricted-load"),
            br#"
                local loader = load
                local function nested()
                    return loader("return privileged")()
                end

                local ok, loaded = pcall(load, "return privileged")
                local pieces = { "return privileged" }
                local index = 0
                local streamed = load(function()
                    index = index + 1
                    return pieces[index]
                end)

                return nested() == nil and ok and loaded() == nil and streamed() == nil
            "#,
            restricted,
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    assert!(lua.execute::<bool>(&executor)?);
    Ok(())
}

#[test]
fn load_honors_explicit_environment() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        let restricted = Table::new(&ctx);
        restricted.set(ctx, "load", ctx.get_global_value("load"))?;

        let closure = Closure::load_with_env(
            ctx,
            Some("explicit-load-environment"),
            br#"
                local explicit = { allowed = true }
                return load("return allowed", nil, nil, explicit)()
            "#,
            restricted,
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    assert!(lua.execute::<bool>(&executor)?);
    Ok(())
}

#[test]
fn load_keeps_default_global_environment() -> Result<(), ExternError> {
    let mut lua = Lua::core();
    let executor = lua.try_enter(|ctx| {
        ctx.set_global("visible", true)?;
        let closure = Closure::load(
            ctx,
            Some("global-load-environment"),
            &b"return load('return visible')()"[..],
        )?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;

    assert!(lua.execute::<bool>(&executor)?);
    Ok(())
}
