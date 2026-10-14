/// A counter for tracking the amount of time spent in `Executor::step` and in callbacks.
///
/// The fuel unit is *approximately* one VM instruction, but this is just a rough estimate
/// (especially since VM instructions are highly variable in cost, such as with the len operator).
///
/// All operations that take a variable amount of time should consume some amount of fuel, so that
/// it is always possible to bound the amount of time spent in `Executor::step`.
#[derive(Debug, Clone)]
pub struct Fuel {
    fuel: i32,
    interrupted: bool,
}

impl Fuel {
    pub fn empty() -> Self {
        Self::with(0)
    }

    pub fn with(fuel: i32) -> Self {
        Self {
            fuel,
            interrupted: false,
        }
    }

    /// Refills fuel up to a given maximum and also clears the fuel interrupt flag.
    ///
    /// This is a convenience method that is intended to be called outside of a call to
    /// `Executor::step` in some loop. It does the operations that all loops that would *re-use* a
    /// fuel container from one tick to another would need to do.
    ///
    /// It credits the running thread with fuel while also preventing available fuel from growing to
    /// an unbounded level, and also clears the interrupt flag since we are now on the "outside" of
    /// whatever lua code is being run and this is where we would want to interrupt to.
    pub fn refill(&mut self, fuel: i32, max_fuel: i32) {
        self.fuel = self.fuel.saturating_add(fuel).min(max_fuel);
        self.interrupted = false;
    }

    /// Add to or subtract from the current remaining fuel.
    pub fn adjust(&mut self, fuel: i32) {
        self.fuel = self.fuel.saturating_add(fuel);
    }

    /// Subtract from the current remaining fuel.
    ///
    /// This is a convenience method that is equivalent to `self.adjust(-fuel)`.
    pub fn consume(&mut self, fuel: i32) {
        self.adjust(fuel.saturating_neg());
    }

    pub fn remaining(&self) -> i32 {
        self.fuel
    }

    pub fn set_remaining(&mut self, fuel: i32) {
        self.fuel = fuel;
    }

    /// Marks that the calling `Executor` should immediately stop executing, without actually
    /// consuming any fuel.
    ///
    /// This is useful in situations where you need to interrupt a running `Executor` because it
    /// may be waiting on an external event that has not yet occurred, but still want to record the
    /// correct fuel usage.
    pub fn interrupt(&mut self) {
        self.interrupted = true;
    }

    pub fn is_interrupted(&self) -> bool {
        self.interrupted
    }

    pub fn clear_interrupt(&mut self) {
        self.interrupted = false;
    }

    /// Returns true if we have positive fuel remaining *and* we have not been interrupted.
    pub fn should_continue(&self) -> bool {
        self.fuel > 0 && !self.interrupted
    }
}

pub(crate) fn count_fuel(per_item: i32, len: usize) -> i32 {
    i32::try_from(len)
        .unwrap_or(i32::MAX)
        .saturating_mul(per_item)
}

/// Fuel for `len` units of work at `units_per_fuel` units per fuel, rounded up.
pub fn fuel_for(len: usize, units_per_fuel: usize) -> i32 {
    i32::try_from(len.div_ceil(units_per_fuel.max(1))).unwrap_or(i32::MAX)
}

/// The cost model: how much work one unit of fuel pays for, for every operation whose time depends
/// on a size.
///
/// A fuel unit is about one VM instruction (5–50 ns on a desktop CPU). Every operation whose time
/// depends on a size charges fuel for it, before doing the work where possible, or while doing it
/// for resumable operations. The rates aim at about 0.25 µs per unit of fuel, below what a host
/// budgeting 1 µs per fuel unit expects. Measured upper bounds (`docs/hardening-notes.md` has the
/// measurements): up to 0.43 µs per fuel at opt-level 3 and 0.82 µs at opt-level z to the end of
/// a step, and up to 1.4–2.4 µs with the incremental collection that follows a step, which
/// allocation paces rather than fuel pays for.
///
/// | Work | Rate |
/// |---|---|
/// | Bytes copied or filled: reallocating stacks and table storage, string buffers growing | 1,024 per fuel (`BYTES_COPIED`) |
/// | Bytes processed: building, hashing, interning, converting, comparing strings (every string `Context::intern` or `new_string` makes); parsing numbers; string library work | 256 per fuel (`BYTES_PROCESSED`) |
/// | Table slots visited: `next` skipping removed entries, counting entries before growth, rehashing | 32 per fuel (`TABLE_SLOTS`) |
/// | Hash lookups of the border search of `#t`, links followed in an `__index` or `__newindex` chain | 8 per fuel (`TABLE_LOOKUPS`) |
/// | Values moved by library functions (`select`, `table.pack`, `unpack`), returned by a function (a handful are free), and by each hop of a `__call` chain (the arguments shift to put the callee first) | 8 per fuel (`VALUES`) |
/// | Frames dropped when a limit error stops an executor | 4 per fuel (`FRAMES`) |
/// | Compiler work (16 bytes of source, a token, a name or label compared) | 2 per fuel (`COMPILE_WORK`) |
/// | Source bytes compiled | 128 per fuel (`SOURCE_BYTES`) |
/// | Pattern matcher steps (a bracket class adds a step per 8 bytes scanned) | 16 per fuel (`MATCH_STEPS`) |
/// | Elements moved or read by `table.insert`/`remove`/`concat` (plus the border search of `#t` for `insert` and `remove`, and `BYTES_COPIED` for the bytes `concat` and `gsub` copy into their buffer) | 1 fuel each |
/// | Each VM instruction; each call (+ 1 per argument or vararg); each callback; each sequence poll; each round of the executor | 1; 4 + 1; 8; 4; 4 |
///
/// Code without access to the step's [`Fuel`] (table growth, interning, metamethod
/// helpers) charges a fuel debt with [`Context::charge_fuel`](crate::Context::charge_fuel); the
/// executor consumes it after every operation (fuel may go negative), and the VM ends its batch
/// of instructions when the debt uses up the step's fuel or a limit was exceeded. Operations that
/// are charged after the fact are bounded: the largest is moving the array part on
/// `table.insert` or `remove`, at most 100,000 values in one step (a larger shift takes the
/// suspending path, moving one value per fuel over as many steps as it takes). One scan is not
/// charged at all: `string.format`'s `%s` looking for a zero byte in its argument (and `c_str` in
/// 5.1 mode), about 92 fuel per ms on a 1 MiB string, bounded by the host's deadline.
pub mod cost {
    /// Bytes copied (or filled) per fuel: reallocating stacks and table storage.
    pub const BYTES_COPIED: usize = 1024;
    /// Bytes processed per fuel by byte-at-a-time work: building, hashing, interning, converting
    /// and comparing strings, parsing numbers, string library functions.
    pub const BYTES_PROCESSED: usize = 256;
    /// Table slots visited per fuel: `next` skipping empty slots, counting entries before a
    /// table grows, rehashing.
    pub const TABLE_SLOTS: usize = 32;
    /// Hash lookups per fuel: the border search of `#` on tables.
    pub const TABLE_LOOKUPS: usize = 8;
    /// Values moved per fuel by library functions (`select`, `table.pack`, `table.unpack`, ...).
    pub const VALUES: usize = 8;
    /// Frames dropped per fuel when an `Executor` stops after a limit error.
    pub const FRAMES: usize = 4;
    /// Units of compiler work per fuel (see `CompileBudget::work`): one per token, name or label
    /// compared, or 16 bytes of source.
    pub const COMPILE_WORK: usize = 2;
    /// Bytes of source per fuel, charged when compiling in addition to the work (which counts the
    /// source once more, at its own rate).
    pub const SOURCE_BYTES: usize = 128;
    /// Steps of the pattern matcher per fuel (`string.find`, `match`, `gmatch`, `gsub`).
    pub const MATCH_STEPS: usize = 16;
}
