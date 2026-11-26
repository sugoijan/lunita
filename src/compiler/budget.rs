use std::cell::Cell;

use thiserror::Error;

use crate::Context;

/// The most work Lua's `load` does to compile a chunk (see [`CompileBudget::work`]), so that a
/// single `load` call fits in an `Executor` step: at most about 2–3 ms natively on an Apple M1
/// Max, and 3–4 ms in wasm. Past it the chunk is a "chunk is too complex to compile" error, which
/// `load` returns as its second result.
///
/// The charges add up, and ordinary code pays for resolving its names too (a name costs a unit
/// for each local and upvalue it is compared with), about 0.5 to 1.3 units per byte of source in
/// all: `load` compiles 15 to 35 KiB of ordinary code, fewer with many locals in scope. Only a
/// chunk of nothing but whitespace, comments or long literals reaches 320 KiB, and only one of
/// short tokens that names no variable reaches 20,000 tokens.
pub const LOAD_MAX_WORK: u64 = 20_000;

/// The most work the host loaders [`FunctionPrototype::compile`], [`Closure::load`] and
/// [`Closure::load_with_env`] do to compile a chunk (see [`CompileBudget::work`]), 500 times what
/// Lua's `load` may do ([`LOAD_MAX_WORK`]). Past it the chunk is a "chunk is too complex to
/// compile" error ([`CompilerError::is_work_limit`]).
///
/// A host loader runs outside of any `Executor`, so no fuel bounds it, and resolving names and
/// jumps can take time quadratic in the size of a chunk. This bounds a compilation to about a
/// second natively on an Apple M1 Max and 1.4 s in wasm, at the most time a unit takes. At 0.5 to
/// 1.3 units per byte of ordinary code that is several megabytes of source: of thirteen scripts
/// and generated tables of 80 to 600 KiB that ship with Neovim, Nmap and gnuplot, the costliest
/// takes 4.2 million units and the others about 130,000 at most.
///
/// A host that compiles larger chunks it trusts, or that wants a tighter bound for the ones it
/// does not, gives [`FunctionPrototype::compile_with_budget`] its own [`CompileLimits`].
///
/// [`FunctionPrototype::compile`]: crate::FunctionPrototype::compile
/// [`FunctionPrototype::compile_with_budget`]: crate::FunctionPrototype::compile_with_budget
/// [`Closure::load`]: crate::Closure::load
/// [`Closure::load_with_env`]: crate::Closure::load_with_env
/// [`CompilerError::is_work_limit`]: crate::closure::CompilerError::is_work_limit
pub const HOST_LOAD_MAX_WORK: u64 = 10_000_000;

/// Bounds on the resources a single compilation may use, `None` meaning unbounded.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct CompileLimits {
    /// Bytes of memory the parser and compiler may use, as estimated by [`CompileBudget`].
    pub memory: Option<usize>,
    /// Longest string literal, name or numeral, in bytes.
    pub string_len: Option<usize>,
    /// Units of work the parser and compiler may do, see [`CompileBudget::work`].
    pub work: Option<u64>,
}

impl CompileLimits {
    /// The limits of a compilation in `ctx`: the memory left and the string length limit of the
    /// instance (see [`crate::limits`]), and `work`.
    pub fn for_context(ctx: Context<'_>, work: Option<u64>) -> Self {
        CompileLimits {
            memory: ctx.memory_room(),
            string_len: ctx.limits().string_len,
            work,
        }
    }
}

/// A compilation exceeded its [`CompileLimits`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, Error)]
pub enum BudgetError {
    #[error("chunk needs too much memory to compile")]
    Memory,
    #[error("chunk is too complex to compile")]
    Work,
}

/// Tracks the resources used by a compilation against its [`CompileLimits`].
#[derive(Debug, Default)]
pub struct CompileBudget {
    limits: CompileLimits,
    memory: Cell<usize>,
    work: Cell<u64>,
}

impl CompileBudget {
    /// An upper bound on the memory the parser and compiler use for each token, which the memory
    /// estimate charges along with the bytes of each string literal and name.
    ///
    /// Measured at up to ~300 bytes per token for some statements (such as `x = <constant>`
    /// repeated), most code uses far less.
    pub const BYTES_PER_TOKEN: usize = 384;

    /// The bytes of source (comments and whitespace included) for one unit of work, charged before
    /// parsing: lexing 16 bytes of a long numeral or comment takes about as long as compiling a
    /// token.
    pub const SOURCE_BYTES_PER_WORK: usize = 16;

    pub fn new(limits: CompileLimits) -> Self {
        CompileBudget {
            limits,
            memory: Cell::new(0),
            work: Cell::new(0),
        }
    }

    pub fn limits(&self) -> CompileLimits {
        self.limits
    }

    /// The memory charged so far, an upper bound on the memory the compilation uses.
    pub fn memory(&self) -> usize {
        self.memory.get()
    }

    /// The work done so far: one unit per [`Self::SOURCE_BYTES_PER_WORK`] bytes of source, one per
    /// token read, and one per name or label compared while resolving variables and
    /// jumps (which can be quadratic in the size of a chunk). A unit takes at most about 0.1 µs on
    /// an Apple M1 Max at opt-level 3, and 0.14 µs at opt-level z or in wasm.
    pub fn work(&self) -> u64 {
        self.work.get()
    }

    pub(crate) fn charge_memory(&self, bytes: usize) -> Result<(), BudgetError> {
        let memory = self.memory.get().saturating_add(bytes);
        self.memory.set(memory);
        if self.limits.memory.is_some_and(|max| memory > max) {
            Err(BudgetError::Memory)
        } else {
            Ok(())
        }
    }

    pub(crate) fn charge_work(&self, units: u64) -> Result<(), BudgetError> {
        let work = self.work.get().saturating_add(units);
        self.work.set(work);
        if self.limits.work.is_some_and(|max| work > max) {
            Err(BudgetError::Work)
        } else {
            Ok(())
        }
    }
}
