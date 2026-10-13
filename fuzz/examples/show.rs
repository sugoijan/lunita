//! Prints the program the grammar target generates from a file, and runs it timing each step:
//! `cargo run --release --example show -- <input> [--run]`.

use std::time::Instant;

use arbitrary::Unstructured;
use lunita::{Closure, Compat, Executor, Fuel};

fn main() {
    let path = std::env::args().nth(1).expect("an input file");
    let data = std::fs::read(path).unwrap();
    let mut u = Unstructured::new(&data);
    let compat = match u.arbitrary::<u8>() {
        Ok(b) if b & 1 == 1 => Compat::Lua51,
        _ => Compat::Default,
    };
    let source = lunita_fuzz::grammar::program(&mut u, lunita_fuzz::seeds()).unwrap();
    println!("-- {compat:?}\n{source}");
    if std::env::args().nth(2).as_deref() != Some("--run") {
        return;
    }
    let mut lua = lunita_fuzz::new_lua(compat);
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("show"), source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    });
    let Ok(executor) = executor else {
        println!("-- compile error");
        return;
    };
    let (mut used, mut steps, mut longest) = (0i64, 0, std::time::Duration::ZERO);
    let start = Instant::now();
    loop {
        let mut fuel = Fuel::with(lunita_fuzz::FUEL_PER_STEP);
        let t = Instant::now();
        let finished = lua
            .enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel))
            .unwrap();
        longest = longest.max(t.elapsed());
        used += i64::from(lunita_fuzz::FUEL_PER_STEP - fuel.remaining());
        steps += 1;
        if finished || used >= lunita_fuzz::FUEL_BUDGET {
            break;
        }
    }
    println!(
        "-- {steps} steps, {used} fuel, {:?} in all, longest step {longest:?}, memory {}",
        start.elapsed(),
        lua.total_memory()
    );
}
