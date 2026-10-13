//! Runs fuzzed chunks the way `tests/limits.rs` runs the limit cases: with
//! `Limits { memory: Some(32 MiB), string_len: Some(1 MiB) }`, 1,000 fuel a step and a budget of
//! 10⁶ fuel, under a counting allocator.
//!
//! A finding is a panic (including failed debug assertions and arithmetic overflows, which the
//! fuzz profile turns on), a step that uses or takes far more than its share, a run that does not
//! stop within its budget, or a heap past the cap.

use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use lunita::{
    Callback, CallbackReturn, Closure, Compat, Executor, Fuel, Limits, Lua, Value, Variadic,
};
use peak_alloc::PeakAlloc;

pub mod grammar;

#[global_allocator]
static ALLOCATOR: PeakAlloc = PeakAlloc;

pub const MEMORY_LIMIT: usize = 32 << 20;
pub const STRING_LIMIT: usize = 1 << 20;
pub const FUEL_PER_STEP: i32 = 1_000;
pub const FUEL_BUDGET: i64 = 1_000_000;
/// The most fuel a single step may use past its 1,000: work that is charged after it is done,
/// such as moving the array part of a table on `table.insert` (at most 2 Mi values within the
/// memory limit) or draining the fuel debt of a long string operation.
pub const MAX_STEP_OVERSHOOT: i64 = 4_000_000;
/// The longest a step may take: steps take tens of microseconds, and the gate of
/// `tests/limits.rs` is 10 ms in a release build. This is a thousand times that, for the slower
/// fuzz build on a busy machine; libFuzzer's `-timeout` catches runs that do not end.
pub const MAX_STEP_TIME: Duration = Duration::from_secs(1);
/// The peak heap allowed above the baseline, as in `tests/limits.rs`.
pub const MAX_PEAK_HEAP: usize = MEMORY_LIMIT;

/// The bytes allocated and not yet freed, as the counting allocator sees them.
pub fn heap_in_use() -> usize {
    ALLOCATOR.current_usage()
}

/// Splits a `chunk` input into the compatibility setting its first byte chooses and the source.
pub fn split_compat(data: &[u8]) -> (Compat, &[u8]) {
    match data.split_first() {
        Some((&b, rest)) if b & 1 == 1 => (Compat::Lua51, rest),
        Some((_, rest)) => (Compat::Default, rest),
        None => (Compat::Default, data),
    }
}

/// An instance with the limits of the targets.
pub fn new_lua(compat: Compat) -> Lua {
    let mut lua = Lua::core();
    lua.set_limits(Limits {
        memory: Some(MEMORY_LIMIT),
        string_len: Some(STRING_LIMIT),
    });
    lua.set_compat(compat);
    lua.set_random_seed(0);
    lua
}

/// Compiles and runs `source`, panicking on a finding.
pub fn run(source: &[u8], compat: Compat) {
    let mut lua = new_lua(compat);
    lua.enter(|ctx| {
        ctx.set_global(
            "print",
            Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)),
        )
        .unwrap();
    });

    let baseline = ALLOCATOR.current_usage();
    ALLOCATOR.reset_peak_usage();

    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("fuzz"), source)?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    });

    if let Ok(executor) = executor {
        let mut used = 0i64;
        loop {
            let mut fuel = Fuel::with(FUEL_PER_STEP);
            let start = Instant::now();
            let finished = lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel));
            let elapsed = start.elapsed();
            assert!(elapsed < MAX_STEP_TIME, "a step took {elapsed:?}");
            let step = i64::from(FUEL_PER_STEP - fuel.remaining());
            assert!(
                step <= i64::from(FUEL_PER_STEP) + MAX_STEP_OVERSHOOT,
                "a step used {step} fuel"
            );
            used += step;
            match finished {
                Ok(true) => {
                    lua.enter(|ctx| {
                        // Taking and formatting the result must work whatever it is.
                        match ctx
                            .fetch(&executor)
                            .take_result::<Variadic<Vec<Value>>>(ctx)
                        {
                            Ok(Ok(values)) => {
                                for v in values.iter() {
                                    let _ = v.display().to_string();
                                }
                            }
                            Ok(Err(err)) => {
                                let _ = err.to_string();
                            }
                            Err(err) => panic!("no result after finishing: {err}"),
                        }
                    });
                    break;
                }
                Ok(false) if used >= FUEL_BUDGET => break,
                Ok(false) => {}
                Err(err) => panic!("bad thread mode: {err}"),
            }
        }
    }

    let peak = ALLOCATOR.peak_usage().saturating_sub(baseline);
    assert!(peak <= MAX_PEAK_HEAP, "peak heap of {peak} bytes");

    // The instance must stay usable after any outcome.
    let after = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, &b"return 1 + 1"[..])?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    });
    if let Ok(after) = after {
        let _ = lua.execute::<i64>(&after);
    }
}

/// The sources of the limit cases up to 4 KiB (`seeds.txt`, written by `update-seeds.sh` from
/// `tests/limits.rs`), which the grammar target splices into its programs.
pub fn seeds() -> &'static [&'static str] {
    static SEEDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    SEEDS.get_or_init(|| {
        let mut seeds = Vec::new();
        let mut rest = include_str!("../seeds.txt");
        while let Some((header, after)) = rest.split_once('\n') {
            let Some(len) = header
                .rsplit(' ')
                .next()
                .and_then(|l| l.parse::<usize>().ok())
            else {
                break;
            };
            let Some(source) = after.get(..len) else {
                break;
            };
            seeds.push(source);
            rest = after.get(len + 1..).unwrap_or_default();
        }
        seeds
    })
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    /// Prints how long grammar runs take with and without a spliced seed.
    #[test]
    #[ignore]
    fn grammar_timing() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let (mut with, mut without) = (
            (0, std::time::Duration::ZERO),
            (0, std::time::Duration::ZERO),
        );
        for _ in 0..300 {
            let bytes = (0..16)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect::<Vec<_>>();
            let mut u = arbitrary::Unstructured::new(&bytes);
            let _ = u.arbitrary::<u8>();
            let source = super::grammar::program(&mut u, super::seeds()).unwrap();
            let start = Instant::now();
            super::run(source.as_bytes(), lunita::Compat::Default);
            let t = start.elapsed();
            let slot = if source.contains("\ndo\n") {
                &mut with
            } else {
                &mut without
            };
            slot.0 += 1;
            slot.1 += t;
        }
        println!(
            "with a seed: {} runs, {:?} each",
            with.0,
            with.1 / with.0.max(1)
        );
        println!(
            "without: {} runs, {:?} each",
            without.0,
            without.1 / without.0.max(1)
        );
    }

    /// Prints how long runs take with the fuzz profile (`cargo test --release -- --ignored
    /// --nocapture timing`).
    #[test]
    #[ignore]
    fn timing() {
        for source in [
            &b"local a = 1"[..],
            b"while true do end",
            b"local t = {} for i = 1, 1e9 do t[i % 100] = i .. '' end",
        ] {
            let start = Instant::now();
            for _ in 0..20 {
                super::run(source, lunita::Compat::Default);
            }
            println!(
                "{:?}: {:?} a run",
                std::str::from_utf8(source).unwrap(),
                start.elapsed() / 20
            );
        }
    }
}
