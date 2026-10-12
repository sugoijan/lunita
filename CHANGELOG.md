## Unreleased (this fork)

Hardening for running untrusted Lua (StepMania noteskins) inside a game, natively and on
`wasm32-unknown-unknown` with `panic = "abort"`: no script may crash the host, take unbounded
memory or time in one step, or behave differently between platforms. gc-arena stays at the pinned
revision. `COMPATIBILITY.md` lists the differences from PUC-Rio Lua kept on purpose,
`docs/hardening-notes.md` the tests, harnesses, measurements and open decisions.

### Added

* Memory and string length limits (`Lua::set_limits`, the `piccolo::limits` module), checked
  before every allocation a script controls (stacks, frames, table growth, concatenation,
  varargs, buffers, the compiler's budget per token and literal byte); a limit error stops the
  executor and cannot be caught by Lua. A reserve for gc-arena's mark queues and a pacing of the
  collector by the limit keep the real heap within it.
* Fuel charged in proportion to work, with a documented cost model (`piccolo::fuel::cost`) and a
  fuel debt for code without access to `Fuel` (`Context::charge_fuel`); frames capped at
  `MAX_FRAMES` (a catchable "stack overflow"); resumable `table.insert`/`remove`/`concat` and
  string matching that suspend between attempts.
* No panic reachable from a script: the panicking lints are denied in `src/lib.rs` and internal
  invariants return errors (`VMError`, `CompileErrorKind::Internal`, `BadThreadMode` on
  re-entry). `rand` and `getrandom` are gone: `math.random` is PUC 5.4's xoshiro256**, seeded
  by `Lua::set_random_seed` (0 by default), and `ahash` is built without `runtime-rng`.
* `Lua::with_hash_seed(seed)`: strings and table keys hashed with keys derived from a host seed
  (`piccolo::hash`), so that scripts cannot make colliding keys.
* `Compat::Lua51` (`Lua::set_compat`): Lua 5.1 number formatting, parsing and arithmetic,
  metamethod rules and library behaviour, documented on the enum and tested byte for byte
  against PUC 5.1.5 (`tests/lua51/`).
* The whole `string` library with Lua patterns ported from lstrlib.c (a step cap per match
  attempt), `load` (with a compiler work cap), `xpcall`, `rawequal`, `error` levels, library
  errors positioned as `luaL_error` positions them, and many parity fixes against PUC 5.4.9
  (`tests/scripts/*_54.lua` pass under it).
* Fuzz targets (`fuzz/`), a wasm harness (`wasm-harness/`) and the limits harness
  (`tests/limits.rs`) with time and memory gates; `scripts/wd.sh`, a watchdog for experiments;
  `scripts/strip_comments.py`, for reviews that read the code without its comments.

### Changed, for hosts written against upstream 0.3.3

* `Table::set_field` and `Context::set_global` return `Result<Value, LimitError>`; the library
  loaders `load_base`, `load_coroutine`, `load_math` (and its parts), `load_string`,
  `load_table`, `load_io`, `Lua::load_core` and `Lua::load_io` return `Result<(), LimitError>`.
  A host's own loader becomes `fn load(ctx: Context<'_>) -> Result<(), LimitError>` with `?` on
  each call, and `lua.enter(load)` gives the result (on a fresh instance without limits nothing
  can be refused). `Lua::core()` and `Lua::full()` are unchanged.
* `piccolo-util`: `UserMethods::add` and `add_write` return `Result<bool, LimitError>`,
  `LuaSerdeExt::load_serde` returns `Result<(), LimitError>`.
* `Lua::set_compat` goes before `load_core`: `_VERSION` is set when the base library loads.
  `FunctionPrototype::compile` (and `Closure::load`) in Lua51 mode compile integer literals past
  2^53 as floats; a chunk compiled in one mode and run in the other keeps them as compiled. The
  constant folding functions fold fewer expressions.
* `Stack::new` takes a `Context` first. A callback's `push_back`, `push_front`, `into_back`,
  `into_front`, `extend` (by value or by reference), `resize` and `reserve` check the stack size
  and memory limits: a refused push is dropped and the script stops at the end of the step.
  `try_reserve` first, to get the error instead.
* `Context::intern` and `Context::new_string` charge fuel and mark the string length and memory
  limits as exceeded for strings that do not fit, which stops the running executor. Outside of a
  step both are discarded when the next step starts, so building values before running scripts
  is unaffected; a host callback making a string over the limit stops the script.
* `String::from_slice`, `from_buffer` and `from_static` take the `&HashKeys` to hash with
  (`ctx.interned_strings().hash_keys()`); `compiler::StringInterner` has a `hash_keys` method.
* New: `Table::try_with_capacity(ctx, array, map)`, which checks the memory limit, and
  `Table::with_capacity`, which makes a small table when refused (and marks the limit);
  `CompileLimits::for_context(ctx, work)`; `LexError::Budget(BudgetError)` and
  `Lexer::set_budget`; `Context::memory_room`, `check_memory`, `check_string`, `exceed_limit`,
  `limit_exceeded`, `charge_fuel`, `new_string`, `compat`; `Lua::total_memory`,
  `mark_queue_reserve`, `limits`; `Error::limit_error`, `ExternError::limit_error`,
  `RuntimeError::limit_error`.
* `piccolo::string::InternGrowth` is gone: interner growth is a `piccolo::table::Growth` (same
  fields). `closure::UpValueInner` is a struct (with a `Lock` of the state) instead of a
  `RefLock`; code using `UpValue::get` and `set` is unaffected.
* `MetaOperatorError` has two new variants, `ChainTooLong(MetaMethod)` and `Loop(MetaMethod)`
  (the Lua 5.1 wording), and `FormatError` one, `QModifiers`: exhaustive matches need them.
* `meta_ops::index` and `meta_ops::new_index` follow chains themselves (up to PUC's
  `MAXTAGLOOP`) and return a `MetaCall` only for a function; a callable table as `__index` is
  indexed, as in PUC-Rio Lua, where piccolo called it. `meta_ops::len`, `negate` and
  `bitwise_not` return `MetaResult<'gc, 2>` (the operand twice, or with nil in Lua51 mode's
  `__len`).
* Floats convert to strings as Lua 5.4 does in the default mode (`tostring`, `..`,
  `Value::into_string`, `string.format("%s")`, `print`); `Value::display` keeps Rust's
  formatting.
* `table.concat` in the default mode takes strings and numbers only, as PUC does ("invalid value
  (table) at index 2 in table for 'concat'"); upstream joined any values with `__concat`, which
  `meta_ops::concat_separated` still does.
* Library errors raised in a function called in a tail call carry a position, and
  `Execution::upper_lua_frame` and `lua_frame(1)` see the Lua function that tail called a
  callback. A chain of tail calls through callbacks keeps one frame per callback (as PUC-Rio
  Lua does with C functions).
* `pcall` and `xpcall` return (rather than raise) the error of calling a value that cannot be
  called; `error` adds the position its level selects; `pairs` and `ipairs` return three values
  and `next` one nil at the end; a unary metamethod gets its operand twice.
* `Executor` methods called from a callback the executor is running return an error or do
  nothing, instead of panicking.

### Choices worth knowing

* Tables keep their hash part in insertion order (a `Vec` of entries plus a `hashbrown`
  index), in safe code, so `pairs` is deterministic on every platform; `next` after a removal
  during iteration scans forward, charged per entry.
* Equal strings are one object, long ones included, so a script repeating the same string cannot
  reach the memory limit with it. Hash lookups are not charged per probe: with a secret seed,
  probe sequences have chance length.
* Thread stacks grow by half instead of doubling: a doubling stack's old blocks never fit the
  next one, and wasm's allocator grew by their sum.
* `MAX_MATCH_STEPS` is 10^5 per match attempt (one attempt runs within a step: under 1 ms
  natively) and `LOAD_MAX_WORK` 20,000 units of compiler work (a `load` within 3–4 ms); both are
  constants.
* The memory limit refuses allocations at 15/16 of itself, less the mark-queue reserve, because
  gc-arena's mark queues and a few buffers are untracked. Past three quarters of the limit a
  collection is finished before the next mutation and charged to the next step.
* `%a` is formatted by piccolo, since the C libraries differ; `math.random` is xoshiro256**.
* VM runtime errors keep piccolo's wording and `pcall` returns them as userdata, not as positioned
  strings; library errors are positioned and name the function as registered.

## [0.3.3]
* Bugfix to not reset live threads held in upvalues of dead threads.

## [0.3.2]

* Bugfix for tail-calling uncallable values. Fixes internal panics.
* Major bugfix for finalization, make sure to transition the collector
  immediately to `Collecting` after finalization is done. Fixes lost `Thread`
  finalization and unclosed upvalues.
* Make the `type` builtin match PUC-Rio Lua by @Jengamon.
* Fix Lua stack corruption during tail calls with less arguments than expected.
* Make function statements act like local / upvalue assignment when appropriate.
* Fix `math.random` and `math.log` to better match PUC-Rio Lua by @Jengamon.
* Fix `select` to better match PUC-Rio Lua by @Jengamon.
* Implement `math.randomseed` by @Jengamon.
* Let `__index` and `__newindex` chain through `UserData` in addition to
  `Table`.
* Implement "dead keys" to make table iteration behavior match PUC-Rio Lua.
* Implement `gc_arena::Collect` for `piccolo_util::UserDataMethods` and
  `piccolo_util::StaticUserDataMethods`.
* Implement `string.sub`, `string.lower`, `string.upper`, `string.reverse` by
  @Jengamon.
* Better match PUC-Rio Lua behavior with longstring newlines.

## [0.3.1]

Small fixups from 0.3

* Actually export `ExecutorInner`
* Add a missing `#[doc(hidden)]` around an internal macro.

## [0.3]

Huge release! Much safer `Executor` API that no longer requires recursion
from Rust -> Lua -> Rust for Rust callbacks to call Lua functions, eliminating
problems with unrestricted Rust stack usage. The `Executor` API also has a
bunch more weird powers that other implementations of Lua can't have, like "tail
resuming" other coroutines and "tail yield".

There is a new `piccolo-util` crate that adds support for some very common use
cases that are not trivial to do in `piccolo` proper:

* Serde support for convenient conversion between Rust types and Lua tables.
* "Freeze" system to safely support the common case where you need to pass
  a non-'static (and non-'gc) value into Lua. Not specific to piccolo, it is
  actually a general way of safely erasing a single lifetime parameter from a
  type (and replacing it with a runtime check).
* Super quick and simple way to wrap Rust types into a Lua userdata with
  methods.

`piccolo-util` will always be an **optional** dependency, and it may contain
code that is more opinionated or limited than vanilla `piccolo` should be.
`piccolo-util` will have opionions about things, and those opinions may be
different than yours... if it is in your way or incomplete for your use, you can
always use it as a starting point for something better.

Also includes a lot of quality of life API improvements, error message
improvements, and more!

- New `Executor` API that enables safe thread recursion and "tail resume" /
  "tail yield".
- New `piccolo-util` crate with very commonly requested, useful features that
  are too opinionated or limited to belong in `piccolo` proper.
- API changes to `Stack` to support a single, unified thread stack shared
  between Lua and callbacks, similar to PUC-Rio Lua et al.
- Upvalues no longer keep entire threads alive and instead use new gc-arena
  finalization support to become closed when threads are garbage collected.
- `IntoMultiValue` / `FromMultiValue` conversion for tuples now allows every
  element to be multi-converted rather than just the last element.
- Support the `__eq` metamethod.
- Error message improvements in lexer / parser errors (they now have line
  numbers at least!).
- API changes to second callback parameter, now an `Execution` type with `Fuel`
  access *and* also calling thread information.
- Add "chunk name" information to compiled chunks for future use in runtime
  errors / tracebacks.
- Simplified `ctx` access, most methods are now directly implemented on `Context`.
- Lots of type renames for clarity, `AnyCallback` -> `Callback`, `AnyUserData`
  -> `UserData`, `AnyValue` -> `Any`, and others.
- Add line number annotations to opcodes for future tracebacks.
- Clean up general ptr handling and allow the user to access internal `Gc`
  pointers in all cases, allows for weak pointers to all pointer types.

## [0.2]
- Allow `Thread` to be forcibly reset to a stopped state.
- Improve the `Table` API, add functions that skip `IntoValue` conversion and
  simplify `Table::next`.
- Support `__newindex`.
- Auto conversion improvements, add a `Variadic` wrapper type to indicate
  variadic multi-values instead of bare arrays.
- Add `Function::compose` and `Function::bind` for easier generic function
  handling from Rust.
- Completely track used memory within interpreter instances. Tracks both
  `gc-arena` allocated `Gc` pointers as well as all normal heap allocations
  using `gc-arena` external allocation tracking.
- `Fuel` system to limit the execution time of Lua code.
- Properly handle `...` in table constructors.
- Implement `table.select('#')`, `table.pack`, and `table.unpack`.
- Fix local function declarations to be visible in their own function body.
- Guard against arbitrary recursion depth of callbacks (only ever a risk for
  Threads calling callbacks on *other* Threads, aka Lua coroutines).

## [0.1.1]
- Initial crates.io release
