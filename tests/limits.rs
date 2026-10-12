//! Runs scripts that try to exhaust time, memory or the native stack, and checks that each ends in
//! its expected outcome without exceeding the memory and time gates.
//!
//! Every case runs with `Limits { memory: Some(32 MiB), string_len: Some(1 MiB) }`, 1,000 fuel per
//! `Executor::step`, the host stopping it after 2 s of steps, under a counting global allocator
//! (`peak_alloc`, which counts both blocks of a `realloc`). Run in release for the time gates:
//!
//! ```sh
//! cargo test --release --test limits -- --nocapture
//! ```
//!
//! `cargo test --test limits -- --ignored export_cases` writes the cases to `target/limit-cases`
//! (for the wasm harness and the fuzzers): `<id>.lua` for each source and `cases.txt`, with a line
//! `<id> <expected outcomes, comma-separated> <byte length>` before each source.
//!
//! `LIMITS_CASES=M1,C2` runs only the listed cases. The results table is printed and written to
//! `target/limits-report.md`.
//!
//! The time gates apply to both the CPU time of the thread and the wall time, so a step cannot pass
//! by sleeping or waiting. They are ratios to the time of a reference workload timed in the same
//! run, and a case over one runs again, each step keeping its least time over the runs (see
//! `run_limit_cases`), so that they hold on a loaded machine. The fuel a step uses is gated as
//! well, and the compile time of each case is reported (not gated). The report ends with the
//! reference's time and the load average: the times in it are upper bounds under that load.

use std::{
    fmt::Write as _,
    time::{Duration, Instant},
};

use cpu_time::ThreadTime;
use peak_alloc::PeakAlloc;

use piccolo::{
    compiler::{CompileBudget, CompileLimits},
    Callback, CallbackReturn, Closure, Executor, Fuel, FunctionPrototype, Limits, Lua,
    StashedExecutor, Value, Variadic,
};

#[global_allocator]
static ALLOCATOR: PeakAlloc = PeakAlloc;

const MEMORY_LIMIT: usize = 32 << 20;
const STRING_LIMIT: usize = 1 << 20;
const FUEL_PER_STEP: i32 = 1_000;
const HOST_STOP: Duration = Duration::from_secs(2);

// The gates.
// The real heap stays within the memory limit (see `Limits::memory`).
const MAX_PEAK_HEAP: usize = MEMORY_LIMIT;
// The most fuel a step may use: its 1,000 and what an operation charges past them up front, such
// as a call moving the 250,000 arguments of R1b along a `__call` chain (250,874 fuel), an operation
// over a stack of a million values (M2, M6b) or a collection finished before the step (see
// `Lua::enter`). It does not depend on the load.
const MAX_STEP_FUEL: i64 = 300_000;
// The time gates are ratios to the time per fuel of a reference workload (`REFERENCE`) timed in the
// same run, so that they hold on a loaded machine, where both slow down, and on another machine.
// Where they were set (Apple M1 Max, release build at opt-level 3), the reference took 0.0129 µs per
// fuel at best, so these are 10 ms per step and 1 µs per fuel (in steps over 0.5 ms) there.
//
// The longest step, in fuel of the reference workload.
const MAX_STEP: f64 = 775_000.0;
// Steps longer than this (in fuel of the reference workload) are gated on their time per fuel.
const SLOW_STEP: f64 = 38_750.0;
// The most time per fuel, relative to the reference's.
const MAX_TIME_PER_FUEL: f64 = 77.0;
// Strings, tables, concatenation and the collector, as most work is.
const REFERENCE: &str =
    "local t, i = {}, 0 while true do i = i + 1 t[i % 512 + 1] = ('x'):rep(i % 64) .. i end";

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Outcome {
    /// The executor was still returning every step when the host stopped it.
    HostStop,
    /// A Lua error.
    Error,
    /// The uncatchable limit error.
    Limit,
    CompileError,
    Success,
}

use Outcome::*;

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HostStop => "host stop",
            Error => "error",
            Limit => "limit",
            CompileError => "compile error",
            Success => "ok",
        })
    }
}

struct Case {
    id: String,
    source: String,
    expected: Vec<Outcome>,
    /// If set, the displayed first return value must be one of these.
    value: Option<Vec<&'static str>>,
}

fn case(id: &str, source: impl Into<String>, expected: &[Outcome]) -> Case {
    Case {
        id: id.to_owned(),
        source: source.into(),
        expected: expected.to_vec(),
        value: None,
    }
}

fn value_case(id: &str, source: impl Into<String>, values: &[&'static str]) -> Case {
    Case {
        id: id.to_owned(),
        source: source.into(),
        expected: vec![Success],
        value: Some(values.to_vec()),
    }
}

/// CPU time used by the current thread.
fn thread_time() -> Duration {
    ThreadTime::now().as_duration()
}

fn nested(prefix: &str, open: &str, middle: &str, close: &str, n: usize) -> String {
    format!("{prefix}{}{middle}{}", open.repeat(n), close.repeat(n))
}

/// The largest `n` below 1,000 for which `accepted(source(n))`, by bisection.
fn deepest(source: impl Fn(usize) -> String, accepted: impl Fn(&str) -> bool) -> String {
    let (mut yes, mut no) = (0, 1000);
    while no - yes > 1 {
        let n = (yes + no) / 2;
        if accepted(&source(n)) {
            yes = n;
        } else {
            no = n;
        }
    }
    source(yes)
}

fn compiles(source: &str) -> bool {
    Lua::core()
        .try_enter(|ctx| {
            Closure::load(ctx, None, source.as_bytes())?;
            Ok(())
        })
        .is_ok()
}

fn runs(source: &str) -> bool {
    let mut lua = Lua::core();
    let Result::Ok(executor) = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    }) else {
        return false;
    };
    lua.execute::<()>(&executor).is_ok()
}

/// The deepest nesting the parser accepts, which must also run (with wasm's default stack too).
fn deepest_nesting(prefix: &str, open: &str, middle: &str, close: &str) -> String {
    deepest(|n| nested(prefix, open, middle, close, n), compiles)
}

/// The differences between the numbers of `tests/data/r2-collisions.txt`, comma-separated.
fn review_collisions() -> String {
    include_str!("data/r2-collisions.txt").replace('\n', "")
}

fn cases() -> Vec<Case> {
    const DEEP: usize = 100_000;

    let mut cases = vec![
        // Time.
        case("C1", "while true do end", &[HostStop]),
        case(
            "C2",
            "while true do pcall(function() while true do end end) end",
            &[HostStop],
        ),
        case(
            "C3",
            "local co = coroutine.create(function() while true do end end) coroutine.resume(co)",
            &[HostStop],
        ),
        case(
            "C4",
            "local mt = {} mt.__tostring = function(t) return tostring(setmetatable({}, mt)) end \
             print(tostring(setmetatable({}, mt)))",
            &[HostStop, Error, Limit],
        ),
        case("C5", "local function f() return 1 + f() end f()", &[Error, Limit]),
        // Each level returns a value more than the one below: past the frame limit, the stack
        // overflow unwinds quadratically, charged per value, so the host may stop it first.
        case(
            "C6",
            "local function f(n) return pcall(f, n + 1) end f(1)",
            &[Error, Limit, HostStop],
        ),
        case(
            "C7",
            "local function f() coroutine.resume(coroutine.create(f)) end f()",
            &[Error, Limit],
        ),
        case(
            "C8a",
            "local t = setmetatable({}, {}) getmetatable(t).__index = function(t, k) return t[k] end \
             return t.x",
            &[Error, Limit],
        ),
        case(
            "C8b",
            "local mt = {} mt.__concat = function(a, b) return a .. b end \
             local t = setmetatable({}, mt) return t .. 'x'",
            &[Error, Limit],
        ),
        case(
            "C9a",
            "local t = {} for i = 1, 1e6 do t[i] = (i * 7919) % 1000003 end table.sort(t)",
            &[Success, HostStop],
        ),
        case(
            "C9b",
            "local t = {} for i = 1, 100 do t[i] = i end table.sort(t, function() return true end)",
            &[Success, Error],
        ),
        case(
            "C9c",
            "local t = {3, 1, 2} table.sort(t, function(a, b) error('comparator') end)",
            &[Error],
        ),
        case(
            "C10",
            "local t = {} for i = 1, 1e6 do t[i] = i end \
             while true do local n = #t for k, v in next, t do end end",
            &[HostStop, Success],
        ),
        case(
            "C11a",
            "string.find(('a'):rep(30), ('a*'):rep(30) .. 'b')",
            &[HostStop, Error],
        ),
        case(
            "C11b",
            "string.find(('a'):rep(30), ('a.-'):rep(30) .. 'b')",
            &[HostStop, Error],
        ),
        case(
            "C11c",
            "string.find(('a'):rep(30), ('(a*)'):rep(30) .. 'b')",
            &[HostStop, Error],
        ),
        // A match attempt runs within one step: the step cap bounds its time.
        case(
            "R4",
            "local s, p = ('a'):rep(30), ('a.-'):rep(30) .. 'b' \
             while true do pcall(string.find, s, p) end",
            &[HostStop],
        ),
        // Table operations that move elements one at a time.
        case(
            "C12a",
            "local t = {} for i = 1, 1e6 do table.insert(t, 1, i) end",
            &[HostStop, Limit],
        ),
        case(
            "C12b",
            "local p = setmetatable({}, {__index = function() return 1 end, \
             __newindex = function() end, __len = function() return 1e15 end}) \
             table.insert(p, 1, 0)",
            &[HostStop],
        ),
        case(
            "C12c",
            "table.remove(setmetatable({}, {__len = function() return math.maxinteger end}), 1)",
            &[HostStop],
        ),
        case(
            "C12d",
            "local p = setmetatable({}, {__index = function() return ('x'):rep(1024) end, \
             __len = function() return 1e9 end}) return #table.concat(p)",
            &[HostStop, Error, Limit],
        ),
        case(
            "C12e",
            "local t = {} for i = 1, 2^20 do t[i] = 'abcdefgh' end \
             while true do table.concat(t, ',') end",
            &[HostStop, Limit],
        ),
        // Found by fuzzing: repeating an empty string looped once per repetition.
        value_case("C13", "return #(''):rep(5^20) + #(''):rep(math.maxinteger, '')", &["0"]),
        // Found by fuzzing: a plain `find` compared up to `#s * #p` bytes in one step (and charged
        // `#s` for every candidate position).
        case(
            "C14",
            "local s = ('a'):rep(2^20 - 1) local p = ('a'):rep(2^19) .. 'b' \
             while true do s:find(p) end",
            &[HostStop],
        ),
        // Found by fuzzing: finding out that a pattern has no special characters scans it, which
        // was not charged when the subject was too short to search.
        case(
            "C14b",
            "local p = ('a'):rep(2^19) .. 'b' while true do (''):find(p) end",
            &[HostStop],
        ),
        // Found by fuzzing: `table.concat` collected values without charging fuel, and went on
        // past `math.maxinteger`.
        case(
            "C15",
            "while true do pcall(table.concat, {}, '', math.mininteger, -1) end",
            &[HostStop],
        ),
        value_case(
            "C15b",
            "local t = setmetatable({}, {__index = function(_, i) return i % 10 end}) \
             return table.concat(t, '', math.maxinteger - 1, math.maxinteger)",
            &["67"],
        ),
        // Found by fuzzing: testing a character against a bracket class scans the class, which
        // was one match step whatever its length.
        case(
            "C16",
            "local s = ('x'):rep(1000) local p = '[' .. ('a'):rep(250) .. 'x]*y' \
             while true do s:find(p) s:find('[%f' .. ('a'):rep(250) .. 'x]') end",
            &[HostStop],
        ),
        // Found in review: every hop of a `__call` chain moves the arguments to put the callee
        // first, which was not charged, so steps grew longer with the chain.
        case(
            "R1a",
            "local t = setmetatable({}, {}) getmetatable(t).__call = t return t()",
            &[HostStop, Error, Limit],
        ),
        case(
            "R1b",
            "local a = {} for i = 1, 250000 do a[i] = i end \
             local t = setmetatable({}, {}) getmetatable(t).__call = t t(table.unpack(a))",
            &[HostStop, Error, Limit],
        ),
        // A finite chain of 200 tables, called over and over.
        case(
            "R1c",
            "local f = function() end for i = 1, 200 do f = setmetatable({}, {__call = f}) end \
             while true do f() end",
            &[HostStop, Error],
        ),
        // Memory.
        case("M1", "local s = 'x' for i = 1, 40 do s = s .. s end", &[Limit]),
        case("M2", "local t = {} for i = 1, 1e9 do t[i] = i end", &[Limit]),
        // The limit error cannot be caught by `pcall` or a coroutine.
        case(
            "M2b",
            "while true do pcall(function() local t = {} while true do t[#t + 1] = {} end end) end",
            &[Limit],
        ),
        case(
            "M2c",
            "local co = coroutine.create(function() local s = 'x' for i = 1, 40 do s = s .. s end end) \
             while true do coroutine.resume(co) end",
            &[Limit],
        ),
        case("M3", "local t = {} for i = 1, 1e7 do t[i .. ''] = i end", &[Limit]),
        case("M4a", "string.rep('x', 1.5e9)", &[Limit]),
        case("M4b", "('x'):rep(1e9, ',')", &[Limit]),
        value_case("M4c", "return #('x'):rep(-1)", &["0"]),
        case(
            "M5",
            "local t = {} for i = 1, 64 do t[i] = ('x'):rep(512 * 1024) end table.concat(t)",
            &[Limit],
        ),
        // Many small objects, which gc-arena's mark queues hold pointers to.
        case(
            "M3b",
            "local t = {} for i = 1, 1e7 do t[i] = function() end end",
            &[Limit],
        ),
        case(
            "M3c",
            "local t = {} for i = 1, 1e7 do local x = i t[i] = function() return x end end",
            &[Limit],
        ),
        case("M6a", "table.unpack({}, 1, 1e8)", &[Limit, Error]),
        case("M6b", "string.byte(('x'):rep(2^20), 1, -1)", &[Limit, Error]),
        case(
            "M6c",
            "local t = {} for i = 1, 1e6 do t[i] = 65 end string.char(table.unpack(t))",
            &[Success, Limit, Error],
        ),
        case("M7", "local s = ('x'):rep(2^20) s:gsub('.', s)", &[Limit]),
        // Buffers held while a coroutine is suspended inside `gsub` count toward the limit.
        case(
            "M12",
            "local s = ('x'):rep(2^20 - 1) local cos = {} \
             local function f(c) if c == 'a' then return s end coroutine.yield() end \
             for i = 1, 1000 do \
             local co = coroutine.create(function() return ('ab'):gsub('%a', f) end) \
             coroutine.resume(co) cos[i] = co end",
            &[Limit],
        ),
        case(
            "M13",
            "local s = ('x'):rep(2^19) local cos = {} \
             for i = 1, 1000 do \
             local n = 0 \
             local co = coroutine.create(function() return load(function() \
             n = n + 1 if n == 1 then return 'return ' .. 'x' elseif n == 2 then return s end \
             coroutine.yield() end) end) \
             coroutine.resume(co) cos[i] = co end",
            &[Limit],
        ),
        case(
            "M8",
            "local s = ('x'):rep(2^20) for i = 1, 1e6 do local u = s:upper() end",
            &[HostStop],
        ),
        // Equal strings are interned, so this table holds the same string many times and never
        // reaches the memory limit within the time the host allows.
        case(
            "M9",
            "while true do pcall(function() local t = {} \
             while true do t[#t + 1] = ('x'):rep(2^19) end end) end",
            &[Limit, HostStop],
        ),
        // With different strings, the limit is reached and `pcall` does not swallow it.
        case(
            "M9b",
            "while true do pcall(function() local t = {} \
             while true do t[#t + 1] = ('x'):rep(2^19) .. #t end end) end",
            &[Limit],
        ),
        // Found in review: about 15 MiB of live data and garbage made faster than the collector
        // freed it reached the memory limit, which counts uncollected garbage. A script whose
        // live data stays under half the limit must never reach it, whatever garbage it makes.
        case(
            "R3a",
            "local keep = {} for i = 1, 60000 do keep[i] = {i} end \
             while true do local s = ('x'):rep(2^19) end",
            &[HostStop],
        ),
        case(
            "R3b",
            "local keep = {} for i = 1, 60000 do keep[i] = {i} end \
             local j = 0 while true do local s = ('x'):rep(2^19) .. j j = j + 1 end",
            &[HostStop],
        ),
        case(
            "R3c",
            "local keep = {} for i = 1, 40000 do keep[i] = {i} end \
             while true do local big = {} for i = 1, 2^18 do big[i] = i end end",
            &[HostStop],
        ),
        case(
            "R3d",
            "local keep = {} for i = 1, 200000 do keep[i] = function() end end \
             local j = 0 while true do local s = ('x'):rep(2^19) .. j j = j + 1 end",
            &[HostStop],
        ),
        case(
            "R3e",
            "local keep = {} for i = 1, 100000 do keep[i] = 'k' .. i end \
             local j = 0 while true do local t = {j, tostring(j), {}} j = j + 1 end",
            &[HostStop],
        ),
        case(
            "R3f",
            "local keep = {} for i = 1, 15 do keep[i] = ('x'):rep(2^20 - 64) .. i end \
             local j = 0 while true do local t = {j} local s = ('y'):rep(2^16) .. j j = j + 1 end",
            &[HostStop],
        ),
        // Live data past three quarters of what the limit allows, and garbage made faster than
        // the incremental collector reaches it: a collection finishes between steps before the
        // garbage gets an allocation refused.
        case(
            "R3g",
            "local keep = {} for i = 1, 90000 do keep[i] = {i} end \
             local j = 0 while true do j = j + 1 local g = ('x'):rep(2^19) .. j end",
            &[HostStop],
        ),
        case(
            "R3h",
            "local keep = {} for i = 1, 22 do keep[i] = ('x'):rep(2^20 - 8) .. i end \
             local j = 0 while true do j = j + 1 local g = ('y'):rep(2^19) .. j end",
            &[HostStop],
        ),
        case("M10a","string.format('%099999d', 1)", &[Error]),
        value_case(
            "M10b",
            "return ('%5.2s'):format(('x'):rep(2^20))",
            &["   xx"],
        ),
        // Found in review: `%q` checked the length of its output after writing it (2 MiB here).
        case("R5a", "string.format('%q', ('\\0'):rep(2^20))", &[Limit]),
        case(
            "R5b",
            "local s = ('\\0'):rep(2^18) .. ('9'):rep(2^18) \
             while true do local _ = string.format('%q', s) end",
            &[HostStop, Limit],
        ),
        // Found in review: strings were hashed with fixed keys, so 65,536 strings whose hashes
        // share their low 16 bits could be computed offline, making the interner's rehash take
        // 37 ms in one step. These strings collide under those keys (see `tests/hash_seed.rs`);
        // piccolo's keys come from a seed (seed 0 here), under which they do not.
        value_case(
            "R2a",
            format!(
                "local s = '{}' local keep, n = {{}}, 0 \
                 for d in s:gmatch('%d+') do n = n + d keep[#keep + 1] = 'k' .. n end \
                 return #keep",
                review_collisions()
            ),
            &["65536"],
        ),
        case(
            "R2b",
            format!(
                "local s = '{}' \
                 while true do local keep, n = {{}}, 0 \
                 for d in s:gmatch('%d+') do n = n + d keep[#keep + 1] = 'k' .. n end end",
                review_collisions()
            ),
            &[HostStop],
        ),
        // Pattern matching on long subjects suspends between attempts.
        case(
            "M11",
            "local s = ('x'):rep(2^20) while true do s:find('y') s:find('[y]') s:gsub('x', 'y') end",
            &[HostStop],
        ),
        // Found in review: `load` compiled up to 100,000 units of work in one step, 16.5 ms for
        // this chunk, which then failed the memory limit. Reading the source counts as work now,
        // and past `LOAD_MAX_WORK` `load` returns an error.
        value_case(
            "R6a",
            "local f, e = load(('x=' .. ('9'):rep(20) .. ' '):rep(40000)) return e",
            &["parse error at line 1: chunk is too complex to compile"],
        ),
        // The longest numerals `load` accepts, in a loop.
        case(
            "R6b",
            "local src = ('x=1e' .. ('0'):rep(250) .. '1 '):rep(950) \
             while true do assert(load(src)) end",
            &[HostStop],
        ),
        case(
            "R6c",
            "local src = ('--[==[' .. (']'):rep(250) .. ']==]'):rep(4000) \
             while true do load(src) end",
            &[HostStop],
        ),
        // Parser and compiler.
        case(
            "P1a",
            format!("local a = {{}} a.b = a local x = a{}", ".b".repeat(DEEP)),
            &[CompileError],
        ),
        case(
            "P1b",
            format!("local a = {{}} a[a] = a local x = a{}", "[a]".repeat(DEEP)),
            &[CompileError],
        ),
        case(
            "P1c",
            format!("local function f() return f end local x = f{}", "()".repeat(DEEP)),
            &[CompileError],
        ),
        case(
            "P1d",
            format!(
                "local a = {{}} function a:b() return a end local x = a{}",
                ":b()".repeat(DEEP)
            ),
            &[CompileError],
        ),
        case(
            "P2a",
            format!("local x = 1{}", "+1".repeat(DEEP)),
            &[Success, CompileError],
        ),
        case(
            "P2b",
            format!("local x = 'a'{}", "..'a'".repeat(DEEP)),
            &[Success, CompileError],
        ),
        case(
            "P2c",
            format!("local x = 2{}", "^2".repeat(DEEP)),
            &[Success, CompileError],
        ),
        case(
            "P2d",
            nested("local x = ", "- ", "1", "", DEEP),
            &[Success, CompileError],
        ),
        case(
            "P2e",
            nested("local x = ", "not ", "1", "", DEEP),
            &[Success, CompileError],
        ),
        case(
            "P2f",
            nested("local x = ", "(", "1", ")", DEEP),
            &[Success, CompileError],
        ),
        case(
            "P2g",
            nested("local x = ", "{", "", "}", DEEP),
            &[Success, CompileError],
        ),
        case(
            "P2h",
            nested("local x = ", "function() return ", "1", " end", 5_000),
            &[Success, CompileError],
        ),
        case("P2i", nested("", "do ", "", "end ", DEEP), &[Success, CompileError]),
        case(
            "P2j",
            nested("local a = true ", "if a then ", "", "end ", DEEP),
            &[Success, CompileError],
        ),
        case(
            "P3a",
            "-- a comment line of 64 bytes, repeated to fill sixteen MiB....\n".repeat(16 << 14),
            &[Success, CompileError, Limit],
        ),
        case(
            "P3b",
            format!("local s = '{}' return #s", "x".repeat(1 << 20)),
            &[Success, CompileError, Limit],
        ),
        case(
            "P3c",
            format!("local s = [==[{}", "x".repeat(1 << 10)),
            &[CompileError],
        ),
        case(
            "P3d",
            format!("local t = {{{}}}", "1,".repeat(2_000_000)),
            &[Success, CompileError, Limit],
        ),
        // The recursion guards' deepest accepted inputs.
        case(
            "P4a",
            deepest_nesting("local a = {} a.b = a local x = a", ".b", "", ""),
            &[Success],
        ),
        case("P4b", deepest_nesting("local x = ", "(", "1", ")"), &[Success]),
        case("P4c", deepest_nesting("local x = ", "{", "", "}"), &[Success]),
        case(
            "P4d",
            deepest_nesting("local x = ", "function() return ", "1", " end"),
            &[Success],
        ),
        case(
            "P4e",
            deepest_nesting("local a = true ", "if a then ", "", "end "),
            &[Success],
        ),
        case(
            "P4f",
            deepest_nesting(
                "local function f(...) return ... end local x = ",
                "f({(",
                "1",
                ")})",
            ),
            &[Success],
        ),
        // The pattern matcher's deepest accepted recursion.
        case(
            "P4g",
            deepest(
                |n| format!("assert(string.find(('a'):rep({n}), ('a?'):rep({n}) .. '$'))"),
                runs,
            ),
            &[Success],
        ),
        // The shapes of source that cost the compiler most per unit of work (see
        // `CompileBudget::work`), compiled as chunks to report their time per unit: long numerals
        // and comments (work per 16 bytes of source) and short statements (work per token).
        case(
            "P5a",
            format!("x=1e{}1 ", "0".repeat(250)).repeat(950),
            &[Success],
        ),
        case(
            "P5b",
            format!("--[==[{}]==]", "]".repeat(250)).repeat(1000),
            &[Success],
        ),
        case(
            "P5c",
            format!("x={} ", "9".repeat(20)).repeat(10_000),
            &[Success],
        ),
        // And function definitions, each a prototype of its own (at most 256 in a function).
        case(
            "P5d",
            format!("f=function() {}end ", "g=function() end ".repeat(100)).repeat(100),
            &[Success],
        ),
        // Integers.
        value_case("I1a", "local a, b = math.mininteger, -1 return a % b", &["0"]),
        value_case(
            "I1b",
            "local a, b = math.mininteger, -1 return a // b",
            &["-9223372036854775808"],
        ),
        value_case("I1c", "return math.mininteger % -1", &["0"]),
        value_case(
            "I1d",
            "return math.mininteger // -1",
            &["-9223372036854775808"],
        ),
        case("I1e", "local z = 0 return 1 % z", &[Error]),
        case("I1f", "local z = 0 return 1 // z", &[Error]),
        case("I1g", "return 1 % 0", &[Error]),
        case("I1h", "return 1 // 0", &[Error]),
        value_case(
            "I1i",
            "return math.maxinteger + 1",
            &["-9223372036854775808"],
        ),
        value_case(
            "I2a",
            "return ('x'):sub(math.mininteger, math.maxinteger)",
            &["x"],
        ),
        case("I2b", "return string.format('%d', 2^63)", &[Error]),
        case("I2c", "return math.floor(0/0)", &[Success]),
        case("I2d", "return math.floor(1e308)", &[Success]),
        case("I2e", "return string.char(256)", &[Error]),
        case("I2f", "return select(math.mininteger, 1)", &[Error]),
        case("I2g", "table.insert({}, math.maxinteger, 1)", &[Error]),
        value_case(
            "I2h",
            "return select('#', table.unpack({}, math.maxinteger - 1, math.maxinteger))",
            &["2"],
        ),
        value_case("I2i", "return tonumber('0x' .. ('f'):rep(100))", &["-1"]),
        value_case("I2j", "return tonumber('1e99999')", &["inf"]),
        case(
            "I3a",
            "for i = math.maxinteger - 1, math.maxinteger do end",
            &[Success],
        ),
        case("I3b", "for i = 1, 10, 0 do end", &[Error]),
        value_case(
            "I3c",
            "local n = 0 for i = 2^53 - 3, 2^53 - 1 do n = n + 1 end return n",
            &["3"],
        ),
        // Errors.
        case(
            "E1",
            "error(setmetatable({}, {__tostring = function() while true do end end}))",
            &[Error],
        ),
        case("E2a", "error()", &[Error]),
        case("E2b", "error(nil)", &[Error]),
        case("E2c", "error({})", &[Error]),
        case(
            "E2d",
            "local ok, e = xpcall(function() error('a') end, function(m) error('b') end) error(e)",
            &[Error],
        ),
        case(
            "E2e",
            "local function f(n) if n == 0 then error('deep') end \
             local ok, e = pcall(f, n - 1) error(e, 0) end f(10000)",
            &[Error, Limit],
        ),
    ];

    cases.retain(|c| {
        std::env::var("LIMITS_CASES").map_or(true, |filter| filter.split(',').any(|id| id == c.id))
    });
    cases
}

struct Record {
    outcome: Outcome,
    value: Option<String>,
    steps: u64,
    fuel: i64,
    max_step_fuel: i64,
    peak_heap: usize,
    peak_tracked: usize,
    compile: Duration,
    // The compiler's work (see `CompileBudget::work`).
    compile_work: u64,
}

/// The times of one step: the whole `Lua::enter` around `Executor::step`, collection included,
/// and up to the end of the step, which leaves out the collection that follows it.
#[derive(Copy, Clone)]
struct Sample {
    cpu_ns: u32,
    wall_ns: u32,
    step_cpu_ns: u32,
    step_wall_ns: u32,
    fuel: i32,
}

// Room for the samples of every step of a run (the most any case takes in 2 s is about 450,000),
// allocated before the cases so that it does not count in their peak heap.
const MAX_SAMPLES: usize = 1 << 20;

fn nanos(d: Duration) -> u32 {
    u32::try_from(d.as_nanos()).unwrap_or(u32::MAX)
}

fn run(case: &Case, samples: &mut Vec<Sample>) -> Record {
    samples.clear();
    let mut lua = Lua::core();
    lua.set_limits(Limits {
        memory: Some(MEMORY_LIMIT),
        string_len: Some(STRING_LIMIT),
    });
    lua.enter(|ctx| {
        // A `print` that discards its arguments.
        ctx.set_global(
            "print",
            Callback::from_fn(&ctx, |_, _, _| Ok(CallbackReturn::Return)),
        )
        .unwrap();
    });
    lua.gc_collect();

    // The baseline is taken with the source in memory.
    let source = case.source.as_bytes();
    let baseline = ALLOCATOR.current_usage();
    ALLOCATOR.reset_peak_usage();

    // As `Closure::load` does, with the budget at hand to read the work done.
    let mut compile_work = 0;
    let start = Instant::now();
    let executor: Result<StashedExecutor, _> = lua.try_enter(|ctx| {
        let budget = CompileBudget::new(CompileLimits::for_context(ctx, None));
        let proto = FunctionPrototype::compile_with_budget(ctx, &case.id, source, &budget);
        compile_work = budget.work();
        let closure = Closure::new(&ctx, proto?, Some(ctx.globals())).unwrap();
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    });
    let compile = start.elapsed();

    let mut record = Record {
        outcome: CompileError,
        value: None,
        steps: 0,
        fuel: 0,
        max_step_fuel: 0,
        peak_heap: 0,
        peak_tracked: 0,
        compile,
        compile_work,
    };

    match executor {
        Err(err) => {
            record.outcome = if err.limit_error().is_some() {
                Limit
            } else {
                CompileError
            };
        }
        Ok(executor) => {
            let start = Instant::now();
            record.outcome = loop {
                let mut fuel = Fuel::with(FUEL_PER_STEP);
                let enter_start = (Instant::now(), thread_time());
                let (finished, step_cpu, step_wall) = lua.enter(|ctx| {
                    let finished = ctx.fetch(&executor).step(ctx, &mut fuel).unwrap();
                    (
                        finished,
                        thread_time() - enter_start.1,
                        enter_start.0.elapsed(),
                    )
                });
                let (cpu, wall) = (thread_time() - enter_start.1, enter_start.0.elapsed());

                record.peak_tracked = record.peak_tracked.max(lua.total_memory());
                let fuel_used = FUEL_PER_STEP - fuel.remaining();
                record.steps += 1;
                record.fuel += i64::from(fuel_used);
                record.max_step_fuel = record.max_step_fuel.max(i64::from(fuel_used));
                if samples.len() < samples.capacity() {
                    samples.push(Sample {
                        cpu_ns: nanos(cpu),
                        wall_ns: nanos(wall),
                        step_cpu_ns: nanos(step_cpu),
                        step_wall_ns: nanos(step_wall),
                        fuel: fuel_used,
                    });
                }

                if finished {
                    break lua.enter(|ctx| {
                        match ctx
                            .fetch(&executor)
                            .take_result::<Variadic<Vec<Value>>>(ctx)
                            .unwrap()
                        {
                            Result::Ok(values) => {
                                record.value = values.first().map(|v| v.display().to_string());
                                Success
                            }
                            Err(err) => {
                                // Formatting the error must not run Lua (E1).
                                let message = err.to_string();
                                assert!(message.len() < 1 << 21);
                                if err.limit_error().is_some() {
                                    Limit
                                } else {
                                    Error
                                }
                            }
                        }
                    });
                }

                if start.elapsed() >= HOST_STOP {
                    break HostStop;
                }
            };
        }
    }

    record.peak_heap = ALLOCATOR.peak_usage().saturating_sub(baseline);

    // The instance must stay usable (or at least droppable) after any outcome.
    let after = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, None, &b"return 1 + 1"[..])?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    });
    if let Result::Ok(after) = after {
        let _ = lua.execute::<i64>(&after);
    }
    drop(lua);

    record
}

/// The time per fuel of the reference workload, in thread CPU time and in wall time: the least of
/// a few runs of 100 steps.
fn time_reference() -> (f64, f64) {
    let mut best = (f64::MAX, f64::MAX);
    for _ in 0..5 {
        let mut lua = Lua::core();
        lua.set_limits(Limits {
            memory: Some(MEMORY_LIMIT),
            string_len: Some(STRING_LIMIT),
        });
        let executor = lua
            .try_enter(|ctx| {
                let closure = Closure::load(ctx, None, REFERENCE.as_bytes())?;
                Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
            })
            .unwrap();
        let (wall, cpu) = (Instant::now(), thread_time());
        let mut used = 0;
        for _ in 0..100 {
            let mut fuel = Fuel::with(FUEL_PER_STEP);
            lua.enter(|ctx| ctx.fetch(&executor).step(ctx, &mut fuel))
                .unwrap();
            used += FUEL_PER_STEP - fuel.remaining();
        }
        let used = f64::from(used);
        best.0 = best.0.min((thread_time() - cpu).as_secs_f64() / used);
        best.1 = best.1.min(wall.elapsed().as_secs_f64() / used);
    }
    best
}

/// The time measures of a case, from the least time of each step over its runs, in seconds and
/// relative to the reference (in fuel of the reference workload). The time per fuel is up to the
/// end of the step, as the collection after it is paced by allocation rather than charged.
#[derive(Default)]
struct Timing {
    longest: (f64, f64),
    per_fuel: (f64, f64),
    // Over the whole `Lua::enter`, collection included (reported, not gated).
    per_fuel_with_collection: (f64, f64),
    longest_ratio: (f64, f64),
    per_fuel_ratio: (f64, f64),
}

impl Timing {
    fn of(samples: &[Sample], reference: (f64, f64)) -> Timing {
        let mut t = Timing::default();
        for s in samples {
            let fuel = f64::from(s.fuel.max(1));
            let (enter_cpu, enter_wall) = (f64::from(s.cpu_ns) * 1e-9, f64::from(s.wall_ns) * 1e-9);
            t.longest.0 = t.longest.0.max(enter_cpu);
            t.longest.1 = t.longest.1.max(enter_wall);
            if enter_cpu / reference.0 > SLOW_STEP {
                t.per_fuel_with_collection.0 = t.per_fuel_with_collection.0.max(enter_cpu / fuel);
            }
            if enter_wall / reference.1 > SLOW_STEP {
                t.per_fuel_with_collection.1 = t.per_fuel_with_collection.1.max(enter_wall / fuel);
            }
            let (cpu, wall) = (
                f64::from(s.step_cpu_ns) * 1e-9,
                f64::from(s.step_wall_ns) * 1e-9,
            );
            if cpu / reference.0 > SLOW_STEP {
                t.per_fuel.0 = t.per_fuel.0.max(cpu / fuel);
            }
            if wall / reference.1 > SLOW_STEP {
                t.per_fuel.1 = t.per_fuel.1.max(wall / fuel);
            }
        }
        t.longest_ratio = (t.longest.0 / reference.0, t.longest.1 / reference.1);
        t.per_fuel_ratio = (t.per_fuel.0 / reference.0, t.per_fuel.1 / reference.1);
        t
    }

    fn over(&self) -> bool {
        self.longest_ratio.0.max(self.longest_ratio.1) > MAX_STEP
            || self.per_fuel_ratio.0.max(self.per_fuel_ratio.1) > MAX_TIME_PER_FUEL
    }
}

fn load_average() -> String {
    if let Result::Ok(load) = std::fs::read_to_string("/proc/loadavg") {
        return load
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
    }
    std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().trim_matches(['{', '}']).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn ms(seconds: f64) -> String {
    format!("{:.2}", seconds * 1e3)
}

#[test]
fn limit_cases() {
    // Debug builds need more than the default 2 MiB of stack for the deepest accepted nesting.
    std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(run_limit_cases)
        .unwrap()
        .join()
        .unwrap();
}

fn run_limit_cases() {
    // The time gates are only meaningful in release builds.
    let timed = !cfg!(debug_assertions);
    let load_before = load_average();
    let mut report = String::new();
    let _ = writeln!(
        report,
        "| Case | Outcome | Steps | Fuel | Most fuel in a step | Peak heap (MiB) | \
         Peak tracked (MiB) | Compile (ms) | Compile work | Compile µs/work | \
         Longest step, cpu / wall (ms) | \
         Longest step, cpu / wall (× ref) | Worst µs/fuel, cpu / wall | \
         Worst per fuel, cpu / wall (× ref) | Worst µs/fuel with collection, cpu / wall | \
         Runs |"
    );
    let _ = writeln!(
        report,
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    );

    let mut samples = Vec::with_capacity(MAX_SAMPLES);
    let mut least = Vec::with_capacity(MAX_SAMPLES);
    let mut references = Vec::new();
    let mut failures = Vec::new();
    for case in cases() {
        // Each step does the same work in every run, so a step another process preempted or
        // slowed down in one run is not in another: a case over a time gate is run again, up to
        // five times in all, and each step keeps its least time over the runs. A step that is
        // really too long is so in every run.
        let mut reference = if timed { time_reference() } else { (1.0, 1.0) };
        let mut record = run(&case, &mut samples);
        least.clear();
        least.extend_from_slice(&samples);
        let mut timing = Timing::of(&least, reference);
        let mut runs = 1;
        while timed && runs < 5 && timing.over() {
            let again = time_reference();
            reference = (reference.0.min(again.0), reference.1.min(again.1));
            let again = run(&case, &mut samples);
            runs += 1;
            record.compile = record.compile.min(again.compile);
            for (l, s) in least.iter_mut().zip(&samples) {
                if l.fuel == s.fuel {
                    l.cpu_ns = l.cpu_ns.min(s.cpu_ns);
                    l.wall_ns = l.wall_ns.min(s.wall_ns);
                    l.step_cpu_ns = l.step_cpu_ns.min(s.step_cpu_ns);
                    l.step_wall_ns = l.step_wall_ns.min(s.step_wall_ns);
                }
            }
            timing = Timing::of(&least, reference);
        }
        references.push(reference.0);
        let _ = writeln!(
            report,
            "| {} | {}{} | {} | {} | {} | {:.2} | {:.2} | {} | {} | {:.3} | {} / {} | \
             {:.0} / {:.0} | \
             {:.3} / {:.3} | {:.1} / {:.1} | {:.3} / {:.3} | {} |",
            case.id,
            record.outcome,
            record
                .value
                .as_ref()
                .map(|v| format!(" ({})", v.chars().take(24).collect::<String>()))
                .unwrap_or_default(),
            record.steps,
            record.fuel,
            record.max_step_fuel,
            record.peak_heap as f64 / (1 << 20) as f64,
            record.peak_tracked as f64 / (1 << 20) as f64,
            ms(record.compile.as_secs_f64()),
            record.compile_work,
            record.compile.as_secs_f64() * 1e6 / record.compile_work.max(1) as f64,
            ms(timing.longest.0),
            ms(timing.longest.1),
            timing.longest_ratio.0,
            timing.longest_ratio.1,
            timing.per_fuel.0 * 1e6,
            timing.per_fuel.1 * 1e6,
            timing.per_fuel_ratio.0,
            timing.per_fuel_ratio.1,
            timing.per_fuel_with_collection.0 * 1e6,
            timing.per_fuel_with_collection.1 * 1e6,
            runs,
        );

        if !case.expected.contains(&record.outcome) {
            failures.push(format!(
                "{}: outcome {:?}, expected one of {:?}",
                case.id, record.outcome, case.expected
            ));
        }
        if let (Some(values), Success) = (&case.value, record.outcome) {
            let value = record.value.as_deref().unwrap_or("<none>");
            if !values.contains(&value) {
                failures.push(format!(
                    "{}: value {value}, expected one of {values:?}",
                    case.id
                ));
            }
        }
        if record.peak_heap > MAX_PEAK_HEAP {
            failures.push(format!(
                "{}: peak heap {} bytes over the gate",
                case.id, record.peak_heap
            ));
        }
        if record.max_step_fuel > MAX_STEP_FUEL {
            failures.push(format!(
                "{}: a step used {} fuel, over the gate",
                case.id, record.max_step_fuel
            ));
        }
        if timed && timing.longest_ratio.0.max(timing.longest_ratio.1) > MAX_STEP {
            failures.push(format!(
                "{}: longest step {} / {} ms (cpu / wall), {:.0} / {:.0} times the reference's \
                 time per fuel, over the gate",
                case.id,
                ms(timing.longest.0),
                ms(timing.longest.1),
                timing.longest_ratio.0,
                timing.longest_ratio.1
            ));
        }
        if timed && timing.per_fuel_ratio.0.max(timing.per_fuel_ratio.1) > MAX_TIME_PER_FUEL {
            failures.push(format!(
                "{}: {:.3} / {:.3} µs per fuel (cpu / wall), {:.1} / {:.1} times the \
                 reference's, over the gate",
                case.id,
                timing.per_fuel.0 * 1e6,
                timing.per_fuel.1 * 1e6,
                timing.per_fuel_ratio.0,
                timing.per_fuel_ratio.1
            ));
        }
    }

    references.sort_by(f64::total_cmp);
    let _ = writeln!(
        report,
        "\nReference workload: {:.4} to {:.4} µs per fuel (cpu, least of each case's runs). \
         Load average: {} before, {} after. Times are upper bounds under that load.",
        references.first().copied().unwrap_or(0.0) * 1e6,
        references.last().copied().unwrap_or(0.0) * 1e6,
        load_before,
        load_average(),
    );
    println!("{report}");
    let _ = std::fs::create_dir_all("target");
    let _ = std::fs::write("target/limits-report.md", &report);

    assert!(failures.is_empty(), "failures:\n{}", failures.join("\n"));
}

#[test]
#[ignore]
fn export_cases() {
    std::thread::Builder::new()
        .stack_size(16 << 20)
        .spawn(write_cases)
        .unwrap()
        .join()
        .unwrap();
}

fn write_cases() {
    let dir = std::path::Path::new("target/limit-cases");
    std::fs::create_dir_all(dir).unwrap();
    let mut index = Vec::new();
    for case in cases() {
        let expected = case
            .expected
            .iter()
            .map(|o| match o {
                HostStop => "host_stop",
                Error => "error",
                Limit => "limit",
                CompileError => "compile_error",
                Success => "ok",
            })
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(dir.join(format!("{}.lua", case.id)), &case.source).unwrap();
        index.extend_from_slice(
            format!("{} {} {}\n", case.id, expected, case.source.len()).as_bytes(),
        );
        index.extend_from_slice(case.source.as_bytes());
        index.push(b'\n');
    }
    std::fs::write(dir.join("cases.txt"), index).unwrap();
}
