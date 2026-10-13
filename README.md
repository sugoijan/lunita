[![crates.io](https://img.shields.io/crates/v/lunita)](https://crates.io/crates/lunita)
[![docs.rs](https://docs.rs/lunita/badge.svg)](https://docs.rs/lunita)
[![CI](https://github.com/sugoijan/lunita/actions/workflows/ci.yml/badge.svg)](https://github.com/sugoijan/lunita/actions/workflows/ci.yml)

## lunita - a stackless Lua VM in pure Rust, hardened for untrusted scripts

`lunita` is a fork of [piccolo](https://github.com/kyren/piccolo), kyren's stackless Lua VM,
made to run scripts it does not trust inside a game, natively and on `wasm32-unknown-unknown`
with `panic = "abort"`. No script may crash the host, take unbounded memory or time in one
step, or behave differently between platforms. On top of piccolo it adds:

* **Memory and string length limits** (`Lua::set_limits`), checked before every allocation a
  script controls. Passing a limit stops the executor with an error Lua cannot catch, and the
  real heap stays within the limit.
* **Fuel in proportion to work**, with a documented cost model (`lunita::fuel::cost`), so that
  one `Executor::step` is bounded in time whatever the script does: frames are capped, long
  library calls suspend between attempts, and the compiler has a budget.
* **No panic reachable from a script**: the panicking lints are denied in the library, and
  internal invariants return errors.
* **Hashing keyed by the host** (`Lua::with_hash_seed`), so that scripts cannot make colliding
  table keys.
* **A Lua 5.1 compatibility mode** (`Compat::Lua51`), tested byte for byte against PUC-Rio Lua
  5.1.5, next to the default mode that follows 5.4.
* **The complete `string` and `table` libraries**, with Lua patterns ported from `lstrlib.c`,
  `load`, `xpcall`, `error` levels, and deterministic `math.random`.

The [CHANGELOG](CHANGELOG.md) lists every change and what a host written against piccolo has to
adapt, [COMPATIBILITY.md](COMPATIBILITY.md) the differences from PUC-Rio Lua kept on purpose,
and [docs/hardening-notes.md](docs/hardening-notes.md) the tests, harnesses, measurements and
open decisions.

## Usage

```rust
use lunita::{Closure, Compat, Executor, Limits, Lua};

let mut lua = Lua::with_hash_seed(seed_from_the_host);
lua.set_compat(Compat::Lua51);
lua.set_limits(Limits {
    memory: Some(32 << 20),
    string_len: Some(1 << 20),
});
lua.load_core()?;

let executor = lua.try_enter(|ctx| {
    let closure = Closure::load(ctx, Some("script"), source)?;
    Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
})?;
lua.execute::<()>(&executor)?;
```

`Lua::execute` runs the executor to completion, collecting garbage between steps. A host that
needs to bound each step gives `Executor::step` its own `Fuel`; see `examples/` for a REPL and
the ways to call Lua functions, bind Rust callbacks and define userdata.

## Design

The design is piccolo's, described in kyren's
[blog post](https://kyju.org/blog/piccolo-a-stackless-lua-interpreter/) and in the
[gc-arena](https://github.com/kyren/gc-arena) README:

* Values live in a `gc-arena` arena with an incremental, cycle-detecting garbage collector.
  `Gc` pointers are machine-word sized, `Copy`, and usable from safe Rust, because they are
  branded by a lifetime that cannot escape `Lua::enter`.
* The VM is stackless: Lua to Rust and Rust to Lua nesting does not use the Rust stack. A
  callback returns a result at once or a `Sequence` that the `Executor` drives step by step,
  so control returns to the host between steps, yields pass through Rust callbacks, and an
  `async` block can implement a `Sequence` through a shadow stack of stashed values.
* `Executor::step` takes fuel and stops when it runs out, which bounds the CPU time and the
  allocation of one step; the memory limit bounds the rest.

Most of `lunita` is safe Rust. The `unsafe` blocks are isolated (non-`'static` userdata
downcasting, the async `Sequence` tunnel, thin pointers in `Value`) and are the same ones as
upstream's; the fork added none.

## What it does not do

* The `io`, `os`, `package`, `utf8` and `debug` libraries, `coroutine.wrap`, `string.pack`
  and `__gc` finalizers are not provided; a host adds what it needs.
* Runtime error messages keep piccolo's wording and carry no position; see COMPATIBILITY.md
  for this and the other differences from PUC-Rio Lua.
* Nothing guards against side channels such as Spectre.

## License

`lunita` is licensed under the MIT license ([LICENSE-MIT](LICENSE-MIT) or
http://opensource.org/licenses/MIT). Copyright of the original work is kyren's; piccolo is
offered under either MIT or the Creative Commons CC0 1.0 Universal Public Domain Dedication
([LICENSE-CC0](LICENSE-CC0)), at your option.
