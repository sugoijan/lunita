# Hardening notes

Notes for whoever maintains this fork. The CHANGELOG lists what changed and what hosts migrate;
COMPATIBILITY.md lists the differences from PUC-Rio Lua kept on purpose; the rustdoc of
`lunita::limits`, `lunita::fuel::cost` and `Compat::Lua51` describes the limits, the cost model
and the Lua 5.1 mode. This file keeps the rest: how the work is checked, what was measured, and
what is still open.

- [Working rules](#working-rules)
- [Denied lints](#denied-lints)
- [Tests and harnesses](#tests-and-harnesses)
- [Measurements](#measurements)
- [Notes for gc-arena and open decisions](#notes-for-gc-arena-and-open-decisions)
- [Left for the maintainer](#left-for-the-maintainer)

## Working rules

- Every experiment with untrusted input runs under `scripts/wd.sh <seconds> <command>`, a
  watchdog that kills the command above 2 GB of RSS or after the time limit.
- No new `unsafe`: the tree has fewer `unsafe` lines than upstream (the insertion-ordered tables
  deleted three blocks, the gc-arena 0.7 port the sequence box's cast), and every remaining block
  reads as upstream's. The one `Drop` on a manually collected type (`HeaderCallback`) only
  touches the memory accounting.
- Behaviour that differs from PUC-Rio Lua may change to match it, and anything that already
  matches must keep matching; a difference kept on purpose is marked where it lives in the code
  with a comment holding `DELIBERATE(puc)` (pointing at COMPATIBILITY.md) or, for a cost-model
  choice a reader would take for a bug (a charge after the fact, uncharged hash probes, the
  zero-byte scan of `string.format`'s `%s`), `DELIBERATE(cost)` (pointing at `fuel::cost`).
  `grep -rn 'DELIBERATE(' src` lists them.
- `scripts/strip_comments.py <src> <dest>` copies a tree with every other comment blanked (line
  numbers kept, the copy still builds), for a review that reads the code without its claims;
  the marked comments are the only ones it keeps. The fork was reviewed that way once by
  invariant (fuel and bounded steps, the memory limit, panics, the executor, PUC parity), and
  then the complement: every comment checked as a claim against the code, with the PUC 5.1.5
  and 5.4.9 binaries for the claims about Lua. Reference builds of both, with their test suites,
  live in `target/lua-ref/` (not committed).
- The game (DitoDitoInfinito) runs every noteskin in `Compat::Lua51` with a 32 MiB memory limit, a
  1 MiB string limit, 200,000 fuel per frame and 20 million per load. A change is checked against
  its noteskin corpus (outcome, elements, warnings and fuel per skin) before it lands.

## Denied lints

`src/lib.rs` denies, outside of tests: `clippy::unwrap_used`, `clippy::expect_used`,
`clippy::panic`, `clippy::unreachable`, `clippy::todo`, `clippy::unimplemented`,
`clippy::indexing_slicing`. The ten sites that cannot fire carry `#[expect(..., reason = "...")]`
(registers below 256, singleton type ids, GC finalization invariants, the documented panicking
APIs `Opt254::new`, which the library reaches only through `VarCount::constant` with counts of at
most 253, and `Stack`'s `Index` impls, which the library itself does not use).
`cargo clippy --lib` passes; `cargo clippy --all-targets` reports fewer warnings than upstream's
tree (70 against 111), all in upstream code.

PUC-Rio Lua 5.4.9's own test suite does not run as is (it needs `debug`, `io`, `os`, `utf8`,
`package`, `string.pack`, `coroutine.wrap` and PUC's runtime error messages). With small shims for
those, `pm.lua`, `math.lua`, `sort.lua` and `vararg.lua` run to the end apart from the lines listed
in COMPATIBILITY.md, error messages, and `table.sort` not detecting an invalid order function;
`strings.lua`, `events.lua`, `nextvar.lua` and `constructs.lua` run until they need what the shims
lack. These runs found the exact integer and float comparisons, `string.rep`'s length errors and
the `rawlen`, `pairs` and `ipairs` fixes.

## Tests and harnesses

```sh
cargo test                                            # everything, debug
cargo test --release --test limits -- --nocapture     # the limit cases with the time gates
tests/lua51/regen.sh [lua5.1.5]                       # regenerate the 5.1 outputs
cargo test --test limits -- --ignored export_cases    # write target/limit-cases
(cd wasm-harness && cargo build --release --target wasm32-unknown-unknown)
node wasm-harness/run.mjs                             # the limit cases in wasm
fuzz/update-seeds.sh                                  # refresh the fuzz seeds
(cd fuzz && cargo +nightly fuzz run grammar -a)       # see below for the full command lines
```

- `tests/limits.rs`: every limit case of the game's brief plus those found since (C12–C16, M12,
  M13, P4, P5, R1–R6, R3g–h), with `Limits { memory: 32 MiB, string_len: 1 MiB }`, 1,000 fuel a
  step, a host stop after 2 s. Gates: outcome, peak real heap within the 32 MiB limit above the
  baseline, at most 300,000 fuel in a step (deterministic), and in release builds the time
  gates, as ratios to the time per fuel of a reference workload timed before each run of each
  case: the longest step (`Lua::enter` with one `Executor::step`, collection included) at most
  775,000 fuel of the reference, and at most 77 times its time per fuel (to the end of the step)
  in steps over 38,750 fuel of it, which are 10 ms, 1 µs and 0.5 ms where they were set (the
  reference took 0.0129 µs per fuel). Both thread CPU time and wall time are gated. Execution is
  deterministic, so a case over a time gate runs again, up to five times, each step keeping its
  least time over the runs. Compile time and the compiler's work are reported, not gated. The
  table goes to `target/limits-report.md` and ends with the reference's time and the load
  average. The harness uses `peak_alloc` and `cpu-time` (dev-dependencies) rather than its own
  allocator.
- `tests/queue_reserve.rs`: the mark queue reserve follows the most objects live, does not shrink,
  takes nothing for strings, counts upvalues, threads and iterators, and counts an instance's
  objects against it alone when instances nest.
- `tests/lua51/`: scripts compared byte for byte with PUC-Rio Lua 5.1.5 (`numbers`, `strings`,
  `errors`, `tables`, `math`, `meta`, `loops`; the harness adds 5.1's `newproxy` for userdata).
  Where lunita's error wording differs on purpose (the VM's runtime errors), a script compares
  the outcome only.
- `tests/scripts/*_54.lua`: default-mode scripts that also pass under PUC-Rio Lua 5.4.9 (run
  them from `tests/scripts`, with the file name as the argument for `errors_54.lua`).
- `tests/hash_seed.rs`: the hash seed (strings colliding under one seed or under the old fixed
  keys spread under others, the seed is set only before any string, tables keep their entries);
  `tests/data/r2-collisions.txt` holds the colliding strings.
- `tests/frames.rs`, `tests/near_limit.rs`, `tests/fuel.rs`, `tests/singleton.rs`,
  `tests/nesting.rs`, `tests/random.rs`: the frame cap and checked stack growth, behaviour at the
  memory limit, the cost model, singleton creation, the compiler's recursion guards, the random
  generator against PUC's values.
- `wasm-harness/`: the same cases in wasm32-unknown-unknown under Node, gated on no trap, the
  expected outcome and memory growth ≤ 36 MiB (the limit and room for fragmentation); the module
  imports nothing. It reports the longest step, the worst time per fuel (collection included) and
  the compile time per unit of work. `HARNESS_WASM` runs another build, such as the game's
  opt-level z (`CARGO_PROFILE_RELEASE_OPT_LEVEL=z cargo build --release --target
  wasm32-unknown-unknown`), and `HARNESS_RUNS=n` runs each case n times, each step keeping its
  least time.
- `fuzz/`: the `chunk` and `grammar` targets (see `fuzz/src/lib.rs` for what counts as a
  finding: a crash, a step over 1 s, memory past the limit), seeded from the limit cases by
  `fuzz/update-seeds.sh`; `fuzz/examples/replay.rs` runs one input and prints the steps the
  target would flag, `fuzz/examples/heap.rs` runs inputs one after another and prints the live
  heap every 1,000 (telling ASan's quarantine from a leak). Runs so far: with `cargo +nightly
  fuzz build -a -s none` (debug assertions on), `chunk -rss_limit_mb=2048 -timeout=10
  -max_len=65536 -jobs=4 -workers=4 -runs=2600000` (10.4 × 10^6 executions) and `grammar
  -rss_limit_mb=2048 -timeout=10 -max_len=4096 -jobs=6 -workers=6 -runs=1700000` (10.2 × 10^6):
  no crash, timeout or out-of-memory; one slow unit (10 s under load, 2.7 s alone in the fuzz
  build, 1.63 ms for its longest step in release). An ASan run (`cargo +nightly fuzz build -a`),
  niced: `grammar -rss_limit_mb=2048 -timeout=30 -max_len=4096 -max_total_time=3600 -jobs=4
  -workers=4` (301,723 executions) and `chunk` with the same options and `-max_len=65536`
  (422,643 executions with
  `ASAN_OPTIONS=quarantine_size_mb=16:malloc_context_size=4:allocator_release_to_os_interval_ms=1000`;
  with a 64 MB quarantine the workers ran out of memory loading the corpus, over 2 GB of RSS
  for 36–39 MB of live heap). No ASan error. The eight `oom-*` inputs of an earlier ASan run
  were the quarantine, not the inputs: replayed without ASan they run to the end or the fuel
  budget in at most 9 MB of RSS. The later rounds of fixes (the checked stack growth, the frame
  cap, the comment review) have had only a five-minute smoke run per target; a longer run is
  due on an idle machine.

Upstream tests changed: `tests/error.rs` (`error_unwind`), `tests/scripts/pcall.lua` and
`tests/scripts/coroutine.lua`, because `error` adds the position PUC-Rio Lua adds;
`tests/scripts.rs` passes each script its chunk name, for `errors_54.lua`; the unit test
`string::tests::test_string_header` passes the hash keys the string constructors take;
`tests/scripts/metaops.lua` and `tests/scripts/table_ops_54.lua` tested lunita's own
`table.concat`, which now follows PUC.

## Measurements

Measured on an Apple M1 Max on 2026-10-12, with other work loading the machine (load average
5–14), as the least time of each step over its runs, so upper bounds. The limits harness's
reference workload of strings, tables and concatenation runs at 0.013 µs per fuel at opt-level 3
and 0.024 µs at opt-level z.

- Up to the end of the step, steps over 0.5 ms take up to 0.43 µs per fuel at opt-level 3 and
  0.82 µs at opt-level z (R5b, quoting control characters with `%q`; R5a and the `load` of long
  numerals come next).
- With the incremental collection after the step, which allocation paces rather than fuel pays
  for, up to 1.4 µs per fuel at opt-level 3 (R2b) and 2.1 µs at opt-level z (C12d) natively, and
  1.9 µs (opt-level 3) to 2.4 µs (opt-level z) in wasm (C8b, R2b, M3b, C12d): the collector
  sweeping after a step that allocated little. The step in which a script is stopped at the
  memory limit also frees what it built, uncharged: 3.4–3.8 µs per fuel in wasm (M2b). Wasm
  steps can only be timed with their collection.
- Compiling takes up to 0.093 µs per unit of compiler work at opt-level 3 and 0.14 µs at
  opt-level z and in wasm (P5a–d: long numerals, long comments, short statements, function
  definitions), up to 0.22 µs under a load average of 15; a compilation also has a fixed cost of
  about 0.03 ms natively and 0.2–0.3 ms in wasm. So a `load` at `LOAD_MAX_WORK` takes at most
  about 3–4 ms.
- A match attempt of the pattern matcher (`MAX_MATCH_STEPS`, 10^5 steps) takes under 1 ms
  natively (about 7 ns a step on this machine) and 1.1–1.4 ms in wasm under Node.
- No step of `tests/limits.rs` takes more than 3.9 ms (opt-level 3) or 4.8 ms (opt-level z)
  natively, or 7.5 ms (opt-level 3) and 9.7 ms (opt-level z) in wasm (M3c, a table of closures
  with upvalues, collecting), and no step uses more than 250,874 fuel (R1b), so their time is
  bounded without a clock: fuel per step times time per fuel.
- The parser's 200-level recursion guard needs about 650 KiB of native stack in a release build
  (most of wasm's default 1 MiB, more than it unoptimized).

## Notes for gc-arena and open decisions

- **gc-arena 0.7 counts objects, not bytes**, so lunita counts the bytes itself (`src/memory.rs`;
  piccolo pinned an unreleased revision between 0.5.3 and 0.6.0, whose `MetricsAlloc` and
  external allocation tracking 0.6 removed, and crates.io refuses git dependencies). Every value
  a `Gc` points to carries a zero-sized field (`Traced<T>`, `Written<T>` or `Boxed<T>`, named
  with the boxed type at `'static`, since the field is `require_static`) that adds the size of its
  heap block (two pointers of header, then the value at its alignment) when it is made and
  removes it in `Drop`, through the thread-local accounting the mark-queue reserve already used;
  callbacks and userdata, whose boxed types carry a type parameter, count by hand (a `Drop` on
  `HeaderCallback`, a `TrackedBytes` field in `any::Value`). The storage values own is in
  `TrackedVec<T>` (a `Vec` recounted after every method that may change its capacity),
  `TrackedTable<T>` (a hashbrown `HashTable` recounted after `update`) and `TrackedBytes` (a
  count with `Drop`, for buffers and the bytes of owned strings); none of them needs `unsafe`,
  and the `BoxSequence` cast that `allocator-api2`'s `Box` needed is gone. Static strings and the
  function lists of composed functions, which only the host makes, are not counted. The limits
  harness checks the real peak heap against the count on every case (`peak_alloc`).
  The collector is paced in objects: every `Lua::enter` turns the storage bytes allocated since
  the last one into debt at the heap's average bytes per object (`Metrics::adjust_debt`), the
  near-limit pacing is `sleep_factor` 0 with the work factors divided by four, and the debt is
  reset after a whole collection (gc-arena only resets it when a cycle ran in one call from
  sleeping, which left idle `Lua::enter` calls collecting ten times over).
- **Mark queues are untracked, and lunita reserves room for them.** gc-arena keeps the objects
  it has yet to trace in two `Vec`s outside `Metrics` that never shrink: `gray` takes white
  objects as they are reached, `gray_again` black objects written to during marking. An object is
  gray while queued and enters a queue only from another color, so it is at most once in each at
  a time, and a `Vec`'s capacity is at most twice its longest length (or 4). The queues are
  therefore bounded by the objects that may contain pointers: two pointers for each one ever
  live at once, and two more for each one that can be written to. As a share of the heap the bound
  is large (about 40% natively for 40-byte upvalues behind 8-byte pointers, in both queues), so
  the reserve counts the objects instead: tables, closures, prototypes, upvalues, threads,
  executors, `gmatch` states and callbacks with GC data carry a zero-sized field that counts them
  against the instance whose arena is in use on the thread (their constructors only take a
  `Mutation`), and the reserve is 2 pointers per object (4 for writable ones) for the most of them
  ever live at once: 16 bytes per closure and 32 per table natively, half that in wasm, nothing
  for strings. Userdata, callbacks made with `Callback::new` directly and the function list a
  composed function holds are not counted (only the host makes them). The remaining 1/16 of the
  limit covers other untracked memory, such as the buffer a concatenation builds its result in
  (M9b). With it, the peak real heap of every case stays within the 32 MiB limit (M3b: 20.00 MiB,
  was 33.98). Counting the queues in `Metrics` in gc-arena would make the reserve unnecessary.
- **Garbage counts against the memory limit** until it is collected, and gc-arena can only collect
  between `Lua::enter` calls, so lunita cannot collect at the moment it would refuse an
  allocation. It paces the collector by the limit: once the heap passes a quarter of the limit,
  each cycle starts as soon as the last one ends and the collector works four times as fast
  (gc-arena's default waits for the heap to grow by half of what the last cycle kept, and does
  one unit of work for a little under two of debt). That keeps a script whose live data stays
  under half the limit under about 25 MiB of a 32 MiB limit whatever garbage it makes (limit
  cases R3a–f). With more live data the
  incremental collector can still fall behind, so once the memory counted against the limit
  passes three quarters of it, the next `Lua::enter` first finishes a collection outside the
  mutation (a cycle already sweeping only frees what was garbage when it started marking, so a
  whole one follows at the next call; one per call at most). It is charged to the next
  `Executor::step` at `cost::BYTES_COPIED` for the memory it goes through (about 25,000 fuel for
  25 MiB, which takes 2–3.5 ms natively), and due again once the memory has grown by half of the
  room the last one left, a 32nd of the limit at least (R3g: 90,000 tables live and 512 KiB
  strings of garbage, R3h: 22 MiB of strings live, both refused before, now host stops).
  The collector's work is per object, so a few objects owning most of the memory (long strings,
  big tables) are cheap to collect but the debt for their storage is paid slowly; the faster
  pacing and the collection near the limit make up for it.
- **Thread stacks grow by half** instead of doubling (still checking the whole new capacity
  against the limit): the blocks a doubling stack leaves behind never fit the next one, so wasm's
  allocator grew by their sum (C15 grew the wasm memory by 39.75 MiB, now 27.94).
- **Equal strings are interned**, long ones included, so a script repeating the same string cannot
  reach the memory limit with it (limit case M9 ends in a host stop; M9b uses distinct strings).
- **Hashing is keyed by the host's seed** (`Lua::with_hash_seed`, `Lua::set_hash_seed`). Before,
  strings were hashed with `AHasher::default()`, whose keys are fixed: 65,536 strings colliding in
  their low 16 bits can be found offline in under a minute, and interning them took 40 ms in one
  step (the interner's rehash). Unseeded instances use the keys of seed 0, which anyone can
  compute from the source, so a host that runs untrusted scripts must set a seed they cannot know
  (the game sets one from `crypto.getRandomValues` for every play). With secret keys, probe
  sequences have chance length, so lookups are not charged per probe (hashbrown does not report
  probes); the border search of `#t` is charged per probe as before.
- **`LOAD_MAX_WORK`** (20,000 units of compiler work for `load` from Lua) and **`MAX_MATCH_STEPS`**
  (10^5 per match attempt) are fixed constants, chosen to keep the work of one call within a few
  milliseconds.

## Left for the maintainer

Found by the reviews and left as they are, each a decision rather than a fix:

- The differences from PUC listed under "Not implemented from PUC" in COMPATIBILITY.md (the
  string metatable consulted for `__index` only, `coroutine.yield` from the main chunk, the Lua
  `table.move`/`table.sort`, `load` with a reader, `next(t, 1.0)`, NaN's sign, no 200-level
  C-call limit), and the VM's runtime errors as userdata rather than positioned strings.
- Host-created traced objects (`Callback::new`, `UserData::new`, the function list of
  `Function::compose`) are not counted in the mark queue reserve: only the host makes them.
- A thread already running as the top of one executor can be given to another; a resume whose
  arguments do not fit kills the coroutine where PUC fails the resume.
- `lunita-util`'s `UserMethods::new` and the serde serializer `unwrap()` table sets that a
  memory limit can refuse; a host loading them under a limit would see a panic there.
- `fuzz/seeds/` is generated from `tests/limits.rs` and committed so that the fuzzer starts
  without a build step; `tests/data/r2-collisions.txt` (400 KB) could be generated at test time.
- A longer fuzz run on an idle machine, after the last rounds of fixes.
