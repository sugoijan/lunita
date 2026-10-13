use lunita::{Closure, Executor, Lua};

fn random_values(seed: Option<u64>) -> Vec<f64> {
    let mut lua = Lua::core();
    if let Some(seed) = seed {
        lua.set_random_seed(seed);
    }
    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                &b"return math.random(), math.random(), math.random()"[..],
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    let (a, b, c) = lua.execute::<(f64, f64, f64)>(&executor).unwrap();
    vec![a, b, c]
}

#[test]
fn random_is_deterministic() {
    // Without a seed set by the host, the generator is seeded with 0.
    assert_eq!(random_values(None), random_values(Some(0)));
    assert_eq!(random_values(Some(42)), random_values(Some(42)));
    assert_ne!(random_values(Some(42)), random_values(Some(43)));
    // The same sequence as `math.randomseed(42)` in PUC-Rio Lua 5.4.
    assert_eq!(random_values(Some(42))[0], 0.9308121780395682);
}
