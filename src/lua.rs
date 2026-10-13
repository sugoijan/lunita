use std::{ops, rc::Rc};

use gc_arena::{
    arena::{CollectionPhase, Root},
    lock::Lock,
    metrics::{Metrics, Pacing},
    Arena, Collect, Gc, Mutation, Rootable,
};

use crate::{
    compat::{Compat, Settings},
    finalizers::Finalizers,
    fuel::{cost, fuel_for},
    limits::{self, CountObjects, LimitError, LimitState, Limits, QueuedObjects},
    stash::{Fetchable, Stashable},
    stdlib::{load_base, load_coroutine, load_io, load_math, load_string, load_table},
    string::InternedStringSet,
    table::Growth,
    thread::BadThreadMode,
    Error, ExternError, FromMultiValue, FromValue, Fuel, IntoValue, Registry, RuntimeError,
    Singleton, StashedExecutor, String, Table, TypeError, Value,
};

/// A value representing the main "execution context" of a Lua state.
///
/// It provides access to the table of global variables, the registry, the string interner, and
/// other state that most every piece of running Lua code will need access to.
///
/// It is a cheap, copyable reference type that references internal state variables inside a [`Lua`]
/// instance.
///
/// As a convenience, it also contains the [`gc_arena::Mutation`] reference provided by `gc-arena`
/// when mutating a [`gc_arena::Arena`]. This allows code that uses lunita to accept a single `ctx:
/// Context<'gc>` parameter, rather than having to accept both the lunita `ctx` *and* the usual
/// `mc: &Mutation<'gc>` parameter.
///
/// To access the contained [`Mutation`] context, there is a `Deref` impl on `Context` that derefs
/// to `Mutation` that can be used like so:
///
/// ```
/// # use gc_arena::Gc;
/// # use lunita::Lua;
/// # let mut lua = Lua::empty();
/// lua.enter(|ctx| {
///     // Create a new `Gc<'gc, i32>` pointer using the `&Mutation` held inside `ctx`
///     let p = Gc::new(&ctx, 13);
/// });
/// ```
#[derive(Copy, Clone)]
pub struct Context<'gc> {
    mutation: &'gc Mutation<'gc>,
    state: &'gc State<'gc>,
}

impl<'gc> Context<'gc> {
    /// Get a reference to [`Mutation`] (the `gc-arena` mutation handle) out of the `Context`
    /// object.
    ///
    /// This can also be done automatically with `Deref` coercion.
    pub fn mutation(self) -> &'gc Mutation<'gc> {
        self.mutation
    }

    pub fn globals(self) -> Table<'gc> {
        self.state.globals
    }

    pub fn registry(self) -> Registry<'gc> {
        self.state.registry
    }

    pub fn interned_strings(self) -> InternedStringSet<'gc> {
        self.state.strings
    }

    pub fn finalizers(self) -> Finalizers<'gc> {
        self.state.finalizers
    }

    // Calls `ctx.globals().get(key)`
    pub fn get_global<V: FromValue<'gc>>(self, key: &'static str) -> Result<V, TypeError> {
        self.state.globals.get(self, key)
    }

    // Calls `ctx.globals().get_value(key)`
    pub fn get_global_value(self, key: &'static str) -> Value<'gc> {
        self.state.globals.get_value(self, key)
    }

    // Calls `ctx.globals().set_field(key, value)`
    pub fn set_global<V: IntoValue<'gc>>(
        self,
        key: &'static str,
        value: V,
    ) -> Result<Value<'gc>, LimitError> {
        self.state.globals.set_field(self, key, value)
    }

    /// Calls `ctx.registry().singleton::<S>(ctx)`.
    pub fn singleton<S>(self) -> &'gc Root<'gc, S>
    where
        S: for<'a> Rootable<'a> + 'static,
        Root<'gc, S>: Sized + Singleton<'gc> + Collect,
    {
        self.state.registry.singleton::<S>(self)
    }

    /// Calls `ctx.registry().stash(ctx, s)`.
    pub fn stash<S: Stashable<'gc>>(self, s: S) -> S::Stashed {
        self.state.registry.stash(&self, s)
    }

    /// Calls `ctx.registry().fetch(f)`.
    pub fn fetch<F: Fetchable>(self, f: &F) -> F::Fetched<'gc> {
        self.state.registry.fetch(f)
    }

    /// Calls `ctx.interned_strings().intern_checked(&ctx, s, ..)`, charging fuel for copying,
    /// hashing and interning the string and for the growth of the set of interned strings, and
    /// checking the string length and memory limits.
    ///
    /// A string past the length limit is still created, and if the set may not grow, the string is
    /// created without being interned: either marks the limit as exceeded, stopping the running
    /// `Executor`. Code that builds a long string should check [`Context::check_string`] first.
    pub fn intern(self, s: &[u8]) -> String<'gc> {
        self.charge_string(s.len());
        self.state
            .strings
            .intern_checked(&self, s, |growth| self.check_intern_growth(growth))
    }

    // Charges the creation of a string of `len` bytes (every string a script makes is charged
    // here), marking the string length limit as exceeded if it is too long.
    fn charge_string(self, len: usize) {
        self.charge_fuel(fuel_for(len, cost::BYTES_PROCESSED));
        let _ = self.state.limits.check_string_len(len);
        // The string is made whether or not its bytes fit: past the limit, this marks it.
        let _ = self.check_memory(len);
    }

    fn check_intern_growth(self, growth: Growth) -> bool {
        self.check_growth(growth).is_ok()
    }

    /// Charges fuel for growing a table, stack or other storage, and checks the memory limit for
    /// its new storage.
    pub(crate) fn check_growth(self, growth: Growth) -> Result<(), LimitError> {
        // The entries are visited (to decide how to grow) even if the growth is then refused.
        self.charge_fuel(
            fuel_for(growth.entries, cost::TABLE_SLOTS)
                .saturating_add(fuel_for(growth.bytes, cost::BYTES_COPIED)),
        );
        if growth.bytes > 0 {
            self.check_memory(growth.bytes)?;
        }
        Ok(())
    }

    /// Calls `ctx.interned_strings().intern_static(&ctx, s)`.
    pub fn intern_static(self, s: &'static [u8]) -> String<'gc> {
        self.state.strings.intern_static(&self, s)
    }

    /// Create a string from an owned buffer.
    ///
    /// The string is interned and charged like with [`Context::intern`], but takes ownership of
    /// the buffer instead of copying it if there is no equal interned string.
    pub fn new_string(self, bytes: Vec<u8>) -> String<'gc> {
        self.charge_string(bytes.len());
        self.state
            .strings
            .intern_buffer(&self, bytes.into_boxed_slice(), |growth| {
                self.check_intern_growth(growth)
            })
    }

    /// The metatable shared by all strings, set by `load_string` to one with `__index = string`.
    pub fn string_metatable(self) -> Option<Table<'gc>> {
        self.state.string_metatable.get()
    }

    /// Set the metatable shared by all strings.
    pub fn set_string_metatable(self, metatable: Option<Table<'gc>>) {
        self.state.string_metatable.set(&self, metatable);
    }

    /// Which Lua version this instance follows where versions differ.
    pub fn compat(self) -> Compat {
        self.state.settings.compat()
    }

    /// The resource limits of this Lua instance.
    pub fn limits(self) -> Limits {
        self.state.limits.limits()
    }

    /// Checks that `additional` more bytes may be allocated without exceeding the memory limit.
    ///
    /// If they may not, this marks the limit as exceeded (which stops the running `Executor`) and
    /// returns an error.
    pub fn check_memory(self, additional: usize) -> Result<(), LimitError> {
        self.state
            .limits
            .check_memory(self.metrics().total_allocation(), additional)
    }

    /// The bytes that may still be allocated within the memory limit, if there is one.
    pub fn memory_room(self) -> Option<usize> {
        self.state
            .limits
            .memory_room(self.metrics().total_allocation())
    }

    /// Checks that a string of `len` bytes may be created without exceeding the string length or
    /// memory limits.
    ///
    /// If it may not, this marks the limit as exceeded (which stops the running `Executor`) and
    /// returns an error.
    pub fn check_string(self, len: usize) -> Result<(), LimitError> {
        self.state.limits.check_string_len(len)?;
        self.check_memory(len)
    }

    /// Marks a limit as exceeded, which stops the running `Executor` with this error (or with the
    /// first one marked in the step).
    ///
    /// This is how host-defined limits can stop Lua code in a way it cannot catch.
    pub fn exceed_limit(self, error: LimitError) -> LimitError {
        self.state.limits.exceed(error)
    }

    /// The limit exceeded since the running `Executor` started its current step, if any.
    pub fn limit_exceeded(self) -> Option<LimitError> {
        self.state.limits.exceeded()
    }

    pub(crate) fn take_limit_exceeded(self) -> Option<LimitError> {
        self.state.limits.take_exceeded()
    }

    /// Charge fuel to the running `Executor`'s step for work proportional to some size, from code
    /// that does not have access to the [`Fuel`] being used (callbacks can use
    /// [`Execution::fuel`](crate::Execution::fuel) instead).
    ///
    /// The fuel is consumed at the end of the current operation, which ends the step if it runs out
    /// (fuel may go negative).
    pub fn charge_fuel(self, fuel: i32) {
        self.state.limits.charge_fuel(fuel);
    }

    pub(crate) fn fuel_debt(self) -> i32 {
        self.state.limits.fuel_debt()
    }

    pub(crate) fn take_fuel_debt(self) -> i32 {
        self.state.limits.take_fuel_debt()
    }

    pub(crate) fn take_collection_fuel(self) -> i32 {
        self.state.limits.take_collection_fuel()
    }
}

impl<'gc> ops::Deref for Context<'gc> {
    type Target = Mutation<'gc>;

    fn deref(&self) -> &Self::Target {
        self.mutation
    }
}

/// A Lua execution environment.
///
/// This is the top-level `lunita` type. In order to load and call any Lua code, the first step is
/// to create a `Lua` instance.
pub struct Lua {
    arena: Arena<Rootable![State<'_>]>,
    // The memory limit, also kept here to pace the collector by it (see `Lua::pace`).
    memory_limit: Option<usize>,
    // Whether the collector is paced for a heap near the memory limit.
    near_limit: bool,
    // Whether the next `Lua::enter` finishes a collection first (see `Lua::collect_near_limit`).
    collection_due: bool,
    // The memory counted against the limit after the last such collection.
    collected_to: usize,
    // The objects the arena's mark queues may hold, counted while the arena is in use.
    objects: Rc<QueuedObjects>,
    // Set when the instance is dropped, so that the arena's objects, dropped after it, are
    // counted against it rather than against another instance in use.
    dropping: Option<CountObjects>,
}

impl Drop for Lua {
    fn drop(&mut self) {
        self.dropping = Some(CountObjects::new(&self.objects));
    }
}

impl Default for Lua {
    fn default() -> Self {
        Lua::core()
    }
}

impl Lua {
    /// Create a new `Lua` instance with no parts of the stdlib loaded.
    pub fn empty() -> Self {
        let objects = Rc::new(QueuedObjects::default());
        let arena = {
            let _count = CountObjects::new(&objects);
            Arena::<Rootable![State<'_>]>::new(|mc| State::new(mc, Rc::clone(&objects)))
        };
        Lua {
            arena,
            memory_limit: None,
            near_limit: false,
            collection_due: false,
            collected_to: 0,
            objects,
            dropping: None,
        }
    }

    /// Create a new `Lua` instance with the core stdlib loaded.
    pub fn core() -> Self {
        let mut lua = Self::empty();
        // A new instance has no limits, so nothing is refused.
        let _ = lua.load_core();
        lua
    }

    /// Create a new `Lua` instance with all of the stdlib loaded.
    pub fn full() -> Self {
        let mut lua = Lua::core();
        let _ = lua.load_io();
        lua
    }

    /// An empty `Lua` instance whose strings and table keys are hashed with keys derived from
    /// `seed` (see [`Lua::set_hash_seed`]); load the standard library with [`Lua::load_core`] and
    /// [`Lua::load_io`] after. Hosts running untrusted scripts should start from this with a seed
    /// the scripts cannot know: [`Lua::core`] and [`Lua::full`] use the keys of seed 0.
    pub fn with_hash_seed(seed: u64) -> Self {
        let mut lua = Self::empty();
        let seeded = lua.set_hash_seed(seed);
        debug_assert!(seeded);
        lua
    }

    /// Load the core parts of the stdlib that do not allow performing any I/O.
    ///
    /// Calls:
    ///   - `load_base`
    ///   - `load_coroutine`
    ///   - `load_math`
    ///   - `load_string`
    ///   - `load_table`
    ///
    /// Under a memory limit (see [`Limits`]), loading can be refused.
    pub fn load_core(&mut self) -> Result<(), LimitError> {
        self.enter(|ctx| {
            load_base(ctx)?;
            load_coroutine(ctx)?;
            load_math(ctx)?;
            load_string(ctx)?;
            load_table(ctx)
        })
    }

    /// Load the parts of the stdlib that allow I/O.
    pub fn load_io(&mut self) -> Result<(), LimitError> {
        self.enter(load_io)
    }

    /// Set the resource limits of this instance, see [`Limits`].
    ///
    /// Limits only refuse new allocations, so lowering them below the memory already in use does
    /// not free anything.
    pub fn set_limits(&mut self, limits: Limits) {
        self.memory_limit = limits.memory;
        self.enter(|ctx| ctx.state.limits.set_limits(limits));
    }

    /// The resource limits of this instance.
    pub fn limits(&mut self) -> Limits {
        self.enter(|ctx| ctx.limits())
    }

    /// Set which Lua version this instance follows where versions differ, see [`Compat`].
    ///
    /// Set it before loading the standard library: `_VERSION` is set when the base library loads.
    pub fn set_compat(&mut self, compat: Compat) {
        self.enter(|ctx| ctx.state.settings.set_compat(compat));
    }

    /// Seed `math.random`, which is seeded with 0 otherwise (it never uses a source of entropy).
    ///
    /// The generator is the same as PUC-Rio Lua 5.4's, so a seed gives the same sequence as
    /// `math.randomseed(seed)` there, and `math.randomseed()` without arguments restores this seed.
    pub fn set_random_seed(&mut self, seed: u64) {
        self.enter(|ctx| {
            ctx.singleton::<Rootable![crate::stdlib::RandomState]>()
                .set_host_seed(seed)
        });
    }

    /// Seed the hashing of strings and table keys, which uses the keys of seed 0 otherwise (see
    /// [`crate::hash`]). Hosts running untrusted scripts should set a seed the scripts cannot know,
    /// so that they cannot compute keys whose hashes collide.
    ///
    /// A string is hashed once, when it is made, so the seed can only be set before the instance
    /// makes any string: on [`Lua::empty`], before loading the standard library or compiling
    /// anything. Returns whether the seed was set. Tables made before take the new keys the first
    /// time they are set through a [`Context`].
    #[must_use = "the seed is not set once the instance has made a string"]
    pub fn set_hash_seed(&mut self, seed: u64) -> bool {
        self.enter(|ctx| ctx.state.strings.set_hash_seed(seed))
    }

    /// Size of all memory used by this Lua context.
    ///
    /// This is equivalent to `self.gc_metrics().total_allocation()`. This counts all `Gc` allocated
    /// memory and also all data Lua datastructures held inside `Gc`, as they are tracked as
    /// "external allocations" in `gc-arena`.
    pub fn total_memory(&self) -> usize {
        self.gc_metrics().total_allocation()
    }

    /// The bytes the memory limit reserves, on top of [`Lua::total_memory`], for the mark queues
    /// of the garbage collector, which it does not track (see [`Limits::memory`]).
    pub fn mark_queue_reserve(&self) -> usize {
        self.objects.reserve()
    }

    /// Finish the current collection cycle completely, calls `gc_arena::Arena::collect_all()`.
    pub fn gc_collect(&mut self) {
        let _count = CountObjects::new(&self.objects);
        if self.arena.collection_phase() != CollectionPhase::Sweeping {
            if let Some(marked) = self.arena.mark_all() {
                marked.finalize(|fc, root| {
                    root.finalizers.prepare(fc);
                });
            }
            if let Some(marked) = self.arena.mark_all() {
                marked.finalize(|fc, root| {
                    root.finalizers.finalize(fc);
                });
            }
        }

        self.arena.collect_all();
        debug_assert!(self.arena.collection_phase() == CollectionPhase::Sleeping);
    }

    pub fn gc_metrics(&self) -> &Metrics {
        self.arena.metrics()
    }

    /// Enter the garbage collection arena and perform some operation.
    ///
    /// In order to interact with Lua or do any useful work with Lua values, you must do so from
    /// *within* the garbage collection arena. All values branded with the `'gc` branding lifetime
    /// must forever live *inside* the arena, and cannot escape it.
    ///
    /// Garbage collection takes place *in-between* calls to `Lua::enter`, no garbage will be
    /// collected concurrently with accessing the arena.
    ///
    /// Automatically triggers garbage collection before returning if the allocation debt is larger
    /// than a small constant. With a memory limit, once the memory counted against it passes three
    /// quarters of it, the next call first finishes a collection, which the next
    /// [`Executor::step`](crate::Executor::step) pays for in fuel, so that garbage the incremental
    /// collector has not reached yet does not get an allocation refused.
    pub fn enter<F, T>(&mut self, f: F) -> T
    where
        F: for<'gc> FnOnce(Context<'gc>) -> T,
    {
        const COLLECTOR_GRANULARITY: f64 = 1024.0;

        let _count = CountObjects::new(&self.objects);
        let collection_fuel = self.collect_near_limit();
        let r = self.arena.mutate(move |mc, state| {
            state.limits.charge_collection_fuel(collection_fuel);
            f(state.ctx(mc))
        });
        self.pace();
        if self.arena.metrics().allocation_debt() > COLLECTOR_GRANULARITY {
            if self.arena.collection_phase() == CollectionPhase::Sweeping {
                self.arena.collect_debt();
            } else {
                if let Some(marked) = self.arena.mark_debt() {
                    marked.finalize(|fc, root| {
                        root.finalizers.prepare(fc);
                    });
                    // `mark_all` only returns `None` while sweeping, and we have just finished
                    // marking.
                    if let Some(marked) = self.arena.mark_all() {
                        marked.finalize(|fc, root| {
                            root.finalizers.finalize(fc);
                        });
                    }
                    // Immediately transition to `CollectionPhase::Sweeping`.
                    if let Some(marked) = self.arena.mark_all() {
                        marked.start_sweeping();
                    }
                }
            }
        }
        self.check_near_limit();
        r
    }

    // The memory counted against the limit: the tracked memory and the mark queue reserve.
    fn counted_memory(&self) -> usize {
        self.total_memory().saturating_add(self.objects.reserve())
    }

    // An allocation refused for the memory limit stops the script, and the incremental collector
    // may not have reached the garbage that took the room: a script with much live data that
    // makes garbage fast could be refused with only garbage in the way. So once the memory counted
    // against the limit passes three quarters of it, the next `Lua::enter` first finishes a
    // collection, outside the arena's mutation, and charges the next executor step for it.
    //
    // The collection is due again once the memory has grown by half of the room the last one left
    // below the point where allocations are refused, and by a 32nd of the limit at least: with
    // live data at the limit, every `Lua::enter` would collect otherwise.
    fn check_near_limit(&mut self) {
        let Some(memory) = self.memory_limit else {
            return;
        };
        let counted = self.counted_memory();
        let room = limits::refused_at(memory).saturating_sub(self.collected_to);
        let rearm = self
            .collected_to
            .saturating_add((room / 2).max(memory / 32));
        if counted >= memory / 4 * 3 && counted >= rearm {
            self.collection_due = true;
        }
    }

    // DELIBERATE(cost): the collection runs outside a step and is charged to the next one, after
    // the fact; the memory limit bounds what it goes through (see `fuel::cost`).
    // Finishes the collection set due by `Lua::check_near_limit`, returning the fuel to charge for
    // it: as much as copying the memory it went through.
    //
    // A collection already sweeping only frees what was garbage when it started marking, so it is
    // followed by a whole one at the next `Lua::enter`. Each `Lua::enter` does one at most.
    fn collect_near_limit(&mut self) -> i32 {
        if !self.collection_due {
            return 0;
        }
        let work = self.total_memory();
        let sweeping = self.arena.collection_phase() == CollectionPhase::Sweeping;
        self.gc_collect();
        if !sweeping {
            self.collection_due = false;
            self.collected_to = self.counted_memory();
        }
        fuel_for(work, cost::BYTES_COPIED)
    }

    // The memory limit counts garbage that has not been collected yet, so the collector has to
    // finish its cycles before garbage fills the room between the live data and the limit.
    //
    // gc-arena's default pacing waits until the heap grows by half of what the last cycle kept,
    // then collects one and two thirds of a byte for each byte allocated: a
    // cycle can end with about twice the live data in memory, or more, so a script with half the
    // limit live reached it with garbage. Once the heap passes a quarter of the limit, the next
    // cycle starts right after the last one ends and collects five bytes for each byte allocated,
    // which keeps the heap within about one and a quarter times the live data (plus what a step
    // allocates). Below that, the default pacing does less work.
    fn pace(&mut self) {
        let Some(memory) = self.memory_limit else {
            return;
        };
        let near = self.arena.metrics().total_allocation() >= memory / 4;
        if near != self.near_limit {
            self.near_limit = near;
            self.arena.metrics().set_pacing(if near {
                Pacing::default()
                    .with_pause_factor(0.0)
                    .with_timing_factor(0.25)
            } else {
                Pacing::default()
            });
        }
    }

    /// A version of `Lua::enter` that expects failure and automatically converts [`Error`] into
    /// [`ExternError`], allowing the error type to escape the arena.
    pub fn try_enter<F, R>(&mut self, f: F) -> Result<R, ExternError>
    where
        F: for<'gc> FnOnce(Context<'gc>) -> Result<R, Error<'gc>>,
    {
        self.enter(move |ctx| f(ctx).map_err(Error::into_extern))
    }

    /// Run the given executor to completion.
    ///
    /// This will periodically exit the arena in order to collect garbage concurrently with running
    /// Lua code.
    pub fn finish(&mut self, executor: &StashedExecutor) -> Result<(), BadThreadMode> {
        const FUEL_PER_GC: i32 = 4096;

        loop {
            let mut fuel = Fuel::with(FUEL_PER_GC);

            if self.enter(|ctx| ctx.fetch(executor).step(ctx, &mut fuel))? {
                break;
            }
        }

        Ok(())
    }

    /// Run the given executor to completion and then take return values from the returning thread.
    ///
    /// This is equivalent to calling `Lua::finish` on an executor and then calling
    /// `Executor::take_result` yourself.
    pub fn execute<R: for<'gc> FromMultiValue<'gc>>(
        &mut self,
        executor: &StashedExecutor,
    ) -> Result<R, ExternError> {
        self.finish(executor).map_err(RuntimeError::new)?;
        self.try_enter(|ctx| ctx.fetch(executor).take_result::<R>(ctx)?)
    }
}

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
struct State<'gc> {
    globals: Table<'gc>,
    registry: Registry<'gc>,
    strings: InternedStringSet<'gc>,
    finalizers: Finalizers<'gc>,
    limits: Gc<'gc, LimitState>,
    settings: Gc<'gc, Settings>,
    // The metatable shared by all strings.
    string_metatable: Gc<'gc, Lock<Option<Table<'gc>>>>,
}

impl<'gc> State<'gc> {
    fn new(mc: &Mutation<'gc>, objects: Rc<QueuedObjects>) -> State<'gc> {
        Self {
            globals: Table::new(mc),
            registry: Registry::new(mc),
            strings: InternedStringSet::new(mc),
            finalizers: Finalizers::new(mc),
            limits: Gc::new(mc, LimitState::new(objects)),
            settings: Gc::new(mc, Settings::default()),
            string_metatable: Gc::new(mc, Lock::new(None)),
        }
    }

    fn ctx(&'gc self, mutation: &'gc Mutation<'gc>) -> Context<'gc> {
        Context {
            mutation,
            state: self,
        }
    }
}
