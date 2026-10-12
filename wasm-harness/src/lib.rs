//! A wasm32-unknown-unknown module running one Lua chunk under the limits of `tests/limits.rs`,
//! one `Executor::step` at a time, for `run.mjs`.
//!
//! The module imports nothing. Its exports take and return integers only:
//!
//! - `source_buffer(len)` allocates the buffer for a chunk's source and returns its address, where
//!   the host writes the source;
//! - `start()` creates the instance (`Lua::core()` with a `print` that discards its arguments)
//!   and compiles the source: 0 if it compiled, 1 on a compile error, 2 on the limit error;
//! - `step()` runs one step with 1,000 fuel: 0 if still running, 1 if the chunk returned, 2 on an
//!   error, 3 on the limit error;
//! - `step_fuel()` is the fuel the last step used, `tracked_memory()` the bytes gc-arena and
//!   piccolo track, and `compile_work()` the compiler's work on the source (see
//!   `CompileBudget::work`).

use std::cell::RefCell;

use piccolo::{
    compiler::{CompileBudget, CompileLimits},
    Callback, CallbackReturn, Closure, Executor, Fuel, FunctionPrototype, Limits, Lua,
    StashedExecutor, Value, Variadic,
};

const MEMORY_LIMIT: usize = 32 << 20;
const STRING_LIMIT: usize = 1 << 20;
const FUEL_PER_STEP: i32 = 1_000;

struct State {
    source: Vec<u8>,
    lua: Option<(Lua, StashedExecutor)>,
    step_fuel: i32,
    compile_work: u64,
}

thread_local! {
    static STATE: RefCell<State> = const {
        RefCell::new(State {
            source: Vec::new(),
            lua: None,
            step_fuel: 0,
            compile_work: 0,
        })
    };
}

#[no_mangle]
pub extern "C" fn source_buffer(len: u32) -> u32 {
    STATE.with_borrow_mut(|state| {
        state.lua = None;
        state.source = vec![0; len as usize];
        state.source.as_ptr() as u32
    })
}

#[no_mangle]
pub extern "C" fn start() -> u32 {
    STATE.with_borrow_mut(|state| {
        let mut lua = Lua::core();
        lua.set_limits(Limits {
            memory: Some(MEMORY_LIMIT),
            string_len: Some(STRING_LIMIT),
        });
        lua.enter(|ctx| {
            ctx.set_global(
                "print",
                Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)),
            ).unwrap();
        });
        // As `Closure::load` does, with the budget at hand to read the work done.
        let executor = lua.try_enter(|ctx| {
            let budget = CompileBudget::new(CompileLimits::for_context(ctx, None));
            let proto =
                FunctionPrototype::compile_with_budget(ctx, "case", &state.source[..], &budget);
            state.compile_work = budget.work();
            let closure = Closure::new(&ctx, proto?, Some(ctx.globals()))
                .expect("a compiled chunk has no upvalue but _ENV");
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        });
        match executor {
            Ok(executor) => {
                state.lua = Some((lua, executor));
                0
            }
            Err(err) if err.limit_error().is_some() => 2,
            Err(_) => 1,
        }
    })
}

#[no_mangle]
pub extern "C" fn step() -> u32 {
    STATE.with_borrow_mut(|state| {
        let Some((lua, executor)) = &mut state.lua else {
            return 2;
        };
        let mut fuel = Fuel::with(FUEL_PER_STEP);
        let finished = lua.enter(|ctx| ctx.fetch(&*executor).step(ctx, &mut fuel));
        state.step_fuel = FUEL_PER_STEP - fuel.remaining();
        match finished {
            Ok(false) => 0,
            Ok(true) => lua.enter(|ctx| {
                match ctx
                    .fetch(&*executor)
                    .take_result::<Variadic<Vec<Value>>>(ctx)
                {
                    Ok(Ok(_)) => 1,
                    Ok(Err(err)) if err.limit_error().is_some() => 3,
                    _ => 2,
                }
            }),
            Err(_) => 2,
        }
    })
}

#[no_mangle]
pub extern "C" fn step_fuel() -> i32 {
    STATE.with_borrow(|state| state.step_fuel)
}

#[no_mangle]
pub extern "C" fn compile_work() -> u32 {
    STATE.with_borrow(|state| u32::try_from(state.compile_work).unwrap_or(u32::MAX))
}

#[no_mangle]
pub extern "C" fn tracked_memory() -> u32 {
    STATE.with_borrow_mut(|state| {
        state
            .lua
            .as_mut()
            .map_or(0, |(lua, _)| lua.total_memory() as u32)
    })
}
