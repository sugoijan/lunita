use piccolo::{Closure, Executor, Lua};

fn run(lua: &mut Lua, source: &str) {
    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(ctx, None, source.as_bytes())?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    lua.execute::<()>(&executor).unwrap();
}

const POINTER: usize = std::mem::size_of::<usize>();

// Running a chunk makes a few objects of its own (its prototype, closure, executor and thread),
// which may be live while the objects of the next chunk are made.
const SLACK: usize = 16 * 4 * POINTER;

fn assert_about(reserve: usize, expected: usize) {
    assert!(
        reserve >= expected && reserve <= expected + SLACK,
        "{reserve} is not about {expected}"
    );
}

#[test]
fn reserve_follows_the_most_objects_live() {
    let mut lua = Lua::core();
    let start = lua.mark_queue_reserve();

    // Closures can enter one queue, tables both.
    run(
        &mut lua,
        "T = {} for i = 1, 10000 do T[i] = function() end end",
    );
    let closures = lua.mark_queue_reserve() - start;
    assert!(closures >= 10_000 * 2 * POINTER, "{closures}");
    assert!(closures < 11_000 * 2 * POINTER, "{closures}");

    run(&mut lua, "U = {} for i = 1, 10000 do U[i] = {} end");
    let tables = lua.mark_queue_reserve() - start - closures;
    assert!(tables >= 10_000 * 4 * POINTER, "{tables}");
    assert!(tables < 11_000 * 4 * POINTER, "{tables}");

    // The queues do not shrink, so neither does the reserve once the objects are freed.
    let before = lua.mark_queue_reserve();
    run(&mut lua, "T = nil U = nil");
    lua.gc_collect();
    lua.gc_collect();
    assert_about(lua.mark_queue_reserve(), before);

    // New objects take the place of the freed ones before the reserve grows again.
    run(
        &mut lua,
        "T = {} for i = 1, 10000 do T[i] = function() end end",
    );
    assert_about(lua.mark_queue_reserve(), before);
}

#[test]
fn strings_take_no_reserve() {
    let mut lua = Lua::core();
    let start = lua.mark_queue_reserve();
    run(
        &mut lua,
        "local t = {} for i = 1, 10000 do t[i] = 'k' .. i end S = t",
    );
    // The table holding them only.
    assert!(lua.mark_queue_reserve() - start < 64 * POINTER);
}

#[test]
fn upvalues_threads_and_iterators_count() {
    let mut lua = Lua::core();
    let start = lua.mark_queue_reserve();
    run(
        &mut lua,
        "T = {} for i = 1, 1000 do local x = i T[i] = function() return x end end",
    );
    // A closure and an upvalue each.
    assert!(lua.mark_queue_reserve() - start >= 1000 * 6 * POINTER);

    let start = lua.mark_queue_reserve();
    run(
        &mut lua,
        "C = {} for i = 1, 1000 do C[i] = coroutine.create(function() end) end",
    );
    assert!(lua.mark_queue_reserve() - start >= 1000 * 4 * POINTER);

    let start = lua.mark_queue_reserve();
    run(
        &mut lua,
        "G = {} for i = 1, 1000 do G[i] = ('a'):gmatch('a') end",
    );
    // The iterator's state and its callback.
    assert!(lua.mark_queue_reserve() - start >= 1000 * 6 * POINTER);
}

#[test]
fn instances_count_their_own_objects() {
    let mut outer = Lua::core();
    run(&mut outer, "T = {} for i = 1, 1000 do T[i] = {} end");
    let reserve = outer.mark_queue_reserve();

    // An instance made, used and dropped while another is in use counts against itself only.
    outer.enter(|_| {
        let mut inner = Lua::core();
        run(&mut inner, "T = {} for i = 1, 5000 do T[i] = {} end");
        assert!(inner.mark_queue_reserve() >= 5000 * 4 * POINTER);
        drop(inner);
    });
    assert_about(outer.mark_queue_reserve(), reserve);

    // Its objects are still all counted: freeing them all and making as many again does not
    // grow the reserve.
    run(&mut outer, "T = nil");
    outer.gc_collect();
    outer.gc_collect();
    run(&mut outer, "T = {} for i = 1, 1000 do T[i] = {} end");
    assert_about(outer.mark_queue_reserve(), reserve);
}
