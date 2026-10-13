//! Runs a `chunk` input (or, with `--lua`, a Lua file in the default mode, or with `--lua51` in
//! the Lua 5.1 mode) the way the target does, printing every step that uses more than twice its
//! fuel or takes over a millisecond: `cargo run --release --example replay -- <input> [--lua]`.

use std::time::{Duration, Instant};

use lunita::{Closure, Compat, Executor, Fuel};

fn main() {
    let path = std::env::args().nth(1).expect("an input file");
    let data = std::fs::read(path).unwrap();
    let (compat, source) = match std::env::args().nth(2).as_deref() {
        Some("--lua") => (Compat::Default, &data[..]),
        Some("--lua51") => (Compat::Lua51, &data[..]),
        _ => lunita_fuzz::split_compat(&data),
    };
    println!("-- {compat:?}");
    let mut lua = lunita_fuzz::new_lua(compat);
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("replay"), source)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    });
    let executor = match executor {
        Ok(executor) => executor,
        Err(err) => {
            println!("-- compile error: {err}");
            return;
        }
    };
    let (mut used, mut steps, mut longest) = (0i64, 0, Duration::ZERO);
    let start = Instant::now();
    let finished = loop {
        let mut fuel = Fuel::with(lunita_fuzz::FUEL_PER_STEP);
        let t = Instant::now();
        let finished = lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel));
        let elapsed = t.elapsed();
        let step = i64::from(lunita_fuzz::FUEL_PER_STEP - fuel.remaining());
        if step > 2 * i64::from(lunita_fuzz::FUEL_PER_STEP) || elapsed > Duration::from_millis(1) {
            println!("step {steps}: {step} fuel, {elapsed:?}");
        }
        longest = longest.max(elapsed);
        used += step;
        steps += 1;
        if !matches!(finished, Ok(false)) || used >= lunita_fuzz::FUEL_BUDGET {
            break finished;
        }
    };
    let outcome = match finished {
        Ok(true) => lua.enter(|ctx| {
            match ctx
                .fetch(&executor)
                .take_result::<lunita::Variadic<Vec<lunita::Value>>>(ctx)
            {
                Ok(Ok(values)) => format!("returned {values:?}"),
                Ok(Err(err)) => format!("error {err}"),
                Err(err) => format!("{err:?}"),
            }
        }),
        Ok(false) => "budget spent".to_owned(),
        Err(err) => format!("{err:?}"),
    };
    println!(
        "-- {outcome}\n-- {steps} steps, {used} fuel, {:?} in all, longest step {longest:?}, memory {}",
        start.elapsed(),
        lua.total_memory()
    );
}
