use std::cell::Cell;

use thiserror::Error;

use crate::Context;

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
