//! The memory accounting behind the memory limit (see [`crate::limits`]).
//!
//! gc-arena measures its heap in `Gc` pointers, not bytes, so the instance counts the bytes
//! itself. Every value a `Gc` points to carries a zero-sized field (`Traced`, `Written` or
//! `Boxed`) that adds the size of its heap block when the value is made and removes it when the
//! collector frees it, and the storage those values own (stacks, the parts of a table, string
//! bytes, buffers kept across calls into Lua) lives in containers that count their capacity
//! ([`TrackedVec`], `TrackedTable`, `TrackedBytes`). The counts go to the instance whose arena
//! is in use on the current thread, because constructors only have a `Mutation`, which does not
//! lead back to the instance: [`Lua`](crate::Lua) makes its accounting current while it uses its
//! arena, and while it drops it.
//!
//! The same fields count the objects that may enter gc-arena's mark queues, for the reserve the
//! memory limit leaves for them (see [`Limits::memory`](crate::limits::Limits::memory)).

use std::{
    cell::Cell,
    fmt, mem,
    ops::{Deref, DerefMut},
    rc::Rc,
    vec,
};

use gc_arena::Collect;
use hashbrown::HashTable;

/// The accounting of one [`Lua`](crate::Lua) instance.
#[derive(Debug, Default)]
pub(crate) struct Accounting {
    // The bytes counted against the memory limit.
    bytes: Cell<usize>,
    // All storage bytes ever counted (not the heap blocks of values, which gc-arena counts), for
    // pacing the collector by allocation.
    allocated: Cell<u64>,
    // Objects that may contain pointers, which may enter the queue of objects to trace.
    traced: Cell<usize>,
    traced_peak: Cell<usize>,
    // Those of them that can also be written to, which may enter the queue of objects to trace
    // again after a write.
    written: Cell<usize>,
    written_peak: Cell<usize>,
}

impl Accounting {
    /// The bytes counted: the heap blocks of the live values (garbage included until it is
    /// collected) and the storage they own.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes.get()
    }

    /// All storage bytes ever counted (see [`TrackedBytes`]), which only grows.
    pub(crate) fn allocated(&self) -> u64 {
        self.allocated.get()
    }

    /// The most bytes the mark queues may hold: a queue's capacity is at most twice its longest
    /// length (or 4), and its length at most the number of objects that may enter it.
    pub(crate) fn reserve(&self) -> usize {
        let entries = self
            .traced_peak
            .get()
            .saturating_add(self.written_peak.get())
            .saturating_add(4);
        entries.saturating_mul(2 * mem::size_of::<usize>())
    }

    // The heap block of a value: a `Gc` allocation, which gc-arena paces its collector by.
    fn add_box(&self, bytes: usize) {
        self.bytes.set(self.bytes.get().saturating_add(bytes));
    }

    // Storage outside of the heap blocks, which the instance paces the collector by.
    fn add_bytes(&self, bytes: usize) {
        self.bytes.set(self.bytes.get().saturating_add(bytes));
        self.allocated
            .set(self.allocated.get().saturating_add(bytes as u64));
    }

    fn remove_bytes(&self, bytes: usize) {
        self.bytes.set(self.bytes.get().saturating_sub(bytes));
    }

    fn add(count: &Cell<usize>, peak: &Cell<usize>) {
        let n = count.get().saturating_add(1);
        count.set(n);
        if n > peak.get() {
            peak.set(n);
        }
    }

    fn remove(count: &Cell<usize>) {
        count.set(count.get().saturating_sub(1));
    }
}

thread_local! {
    // The accounting of the instance whose arena is in use on this thread.
    static CURRENT: Cell<Option<Rc<Accounting>>> = const { Cell::new(None) };
}

fn with_current(f: impl FnOnce(&Accounting)) {
    let _ = CURRENT.try_with(|current| {
        let accounting = current.take();
        if let Some(accounting) = &accounting {
            f(accounting);
        }
        current.set(accounting);
    });
}

/// Makes the values made or freed on this thread count against `accounting` until dropped, when
/// the accounting in use before is restored. `Lua` holds one while it uses its arena.
pub(crate) struct Accounted {
    previous: Option<Rc<Accounting>>,
}

impl Accounted {
    pub(crate) fn new(accounting: &Rc<Accounting>) -> Self {
        let previous = CURRENT
            .try_with(|current| current.replace(Some(Rc::clone(accounting))))
            .ok()
            .flatten();
        Accounted { previous }
    }
}

impl Drop for Accounted {
    fn drop(&mut self) {
        let previous = self.previous.take();
        let _ = CURRENT.try_with(|current| current.set(previous));
    }
}

/// Counts the heap block of a `Gc<T>` as made, for a value that counts it as freed in its own
/// `Drop` with [`uncount_box`] (one whose size the counting fields cannot name).
pub(crate) fn count_box<T>() {
    with_current(|a| a.add_box(box_size::<T>()));
}

/// Counts the heap block of a `Gc<T>` as freed (see [`count_box`]).
pub(crate) fn uncount_box<T>() {
    with_current(|a| a.remove_bytes(box_size::<T>()));
}

/// Counts `bytes` allocated outside of a `Gc` as made, for a value that counts them as freed in
/// its own `Drop` with [`uncount_bytes`].
pub(crate) fn count_bytes(bytes: usize) {
    with_current(|a| a.add_bytes(bytes));
}

/// Counts `bytes` as freed (see [`count_bytes`]).
pub(crate) fn uncount_bytes(bytes: usize) {
    with_current(|a| a.remove_bytes(bytes));
}

/// The size of the heap block gc-arena allocates for a `Gc<T>`: a header of two pointers, then
/// `T` at its alignment, padded to the alignment of the whole.
pub(crate) const fn box_size<T>() -> usize {
    let header = 2 * mem::size_of::<usize>();
    let align = if mem::align_of::<T>() > mem::align_of::<usize>() {
        mem::align_of::<T>()
    } else {
        mem::align_of::<usize>()
    };
    let offset = header.div_ceil(align) * align;
    (offset + mem::size_of::<T>()).div_ceil(align) * align
}

macro_rules! counting_field {
    ($(#[$doc:meta])* $name:ident, $add:expr, $remove:expr) => {
        $(#[$doc])*
        #[derive(Collect)]
        #[collect(require_static)]
        pub(crate) struct $name<T: 'static>(std::marker::PhantomData<fn() -> T>);

        impl<T: 'static> $name<T> {
            /// Counts the value this is a field of, a `Gc<T>`, as made.
            pub(crate) fn new() -> Self {
                with_current(|a: &Accounting| ($add)(a, box_size::<T>()));
                $name(std::marker::PhantomData)
            }
        }

        impl<T: 'static> Drop for $name<T> {
            fn drop(&mut self) {
                with_current(|a: &Accounting| ($remove)(a, box_size::<T>()));
            }
        }

        impl<T: 'static> fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(stringify!($name))
            }
        }
    };
}

counting_field!(
    /// A field of a value held by a `Gc<T>` that cannot contain pointers, counting the bytes of
    /// its heap block while it lives.
    Boxed,
    |a: &Accounting, bytes| a.add_box(bytes),
    |a: &Accounting, bytes| a.remove_bytes(bytes)
);

counting_field!(
    /// A field of a value held by a `Gc<T>` that may contain pointers, counting the bytes of its
    /// heap block while it lives and the object for the mark queue reserve.
    Traced,
    |a: &Accounting, bytes| {
        a.add_box(bytes);
        Accounting::add(&a.traced, &a.traced_peak);
    },
    |a: &Accounting, bytes| {
        a.remove_bytes(bytes);
        Accounting::remove(&a.traced);
    }
);

counting_field!(
    /// [`Traced`], for a value that can also be written to.
    Written,
    |a: &Accounting, bytes| {
        a.add_box(bytes);
        Accounting::add(&a.traced, &a.traced_peak);
        Accounting::add(&a.written, &a.written_peak);
    },
    |a: &Accounting, bytes| {
        a.remove_bytes(bytes);
        Accounting::remove(&a.traced);
        Accounting::remove(&a.written);
    }
);

/// A field of a value that may contain pointers whose bytes are counted elsewhere, counting only
/// the object for the mark queue reserve.
#[derive(Debug, Collect)]
#[collect(require_static)]
pub(crate) struct Queued(());

impl Queued {
    pub(crate) fn new() -> Self {
        with_current(|a| Accounting::add(&a.traced, &a.traced_peak));
        Queued(())
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        with_current(|a| Accounting::remove(&a.traced));
    }
}

/// Bytes allocated outside of a `Gc` (a buffer, the storage of a container), counted until
/// dropped.
#[derive(Debug, Collect)]
#[collect(require_static)]
pub(crate) struct TrackedBytes {
    bytes: usize,
}

impl TrackedBytes {
    pub(crate) fn new(bytes: usize) -> Self {
        with_current(|a| a.add_bytes(bytes));
        TrackedBytes { bytes }
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Changes the bytes counted.
    pub(crate) fn set(&mut self, bytes: usize) {
        if bytes == self.bytes {
            return;
        }
        if bytes > self.bytes {
            with_current(|a| a.add_bytes(bytes - self.bytes));
        } else {
            with_current(|a| a.remove_bytes(self.bytes - bytes));
        }
        self.bytes = bytes;
    }
}

impl Drop for TrackedBytes {
    fn drop(&mut self) {
        with_current(|a| a.remove_bytes(self.bytes));
    }
}

/// A `Vec` whose capacity is counted against the memory limit.
///
/// It reads as a slice; the methods that may change the capacity update the count.
#[derive(Collect)]
#[collect(no_drop)]
pub struct TrackedVec<T> {
    vec: Vec<T>,
    bytes: TrackedBytes,
}

impl<T> TrackedVec<T> {
    pub fn new() -> Self {
        TrackedVec {
            vec: Vec::new(),
            bytes: TrackedBytes::new(0),
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let vec = Vec::with_capacity(capacity);
        let bytes = TrackedBytes::new(vec.capacity().saturating_mul(mem::size_of::<T>()));
        TrackedVec { vec, bytes }
    }

    pub fn from_iter_exact(iter: impl IntoIterator<Item = T>) -> Self {
        let vec = iter.into_iter().collect::<Vec<T>>();
        let mut this = TrackedVec {
            vec,
            bytes: TrackedBytes::new(0),
        };
        this.vec.shrink_to_fit();
        this.recount();
        this
    }

    fn recount(&mut self) {
        self.bytes
            .set(self.vec.capacity().saturating_mul(mem::size_of::<T>()));
    }

    pub fn capacity(&self) -> usize {
        self.vec.capacity()
    }

    pub fn as_slice(&self) -> &[T] {
        &self.vec
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.vec
    }

    pub fn push(&mut self, value: T) {
        self.vec.push(value);
        self.recount();
    }

    pub fn pop(&mut self) -> Option<T> {
        self.vec.pop()
    }

    pub fn insert(&mut self, index: usize, value: T) {
        self.vec.insert(index, value);
        self.recount();
    }

    pub fn remove(&mut self, index: usize) -> T {
        self.vec.remove(index)
    }

    pub fn truncate(&mut self, len: usize) {
        self.vec.truncate(len);
    }

    pub fn clear(&mut self) {
        self.vec.clear();
    }

    pub fn resize(&mut self, len: usize, value: T)
    where
        T: Clone,
    {
        self.vec.resize(len, value);
        self.recount();
    }

    pub fn reserve_exact(&mut self, additional: usize) {
        self.vec.reserve_exact(additional);
        self.recount();
    }

    pub fn extend_from_slice(&mut self, values: &[T])
    where
        T: Clone,
    {
        self.vec.extend_from_slice(values);
        self.recount();
    }

    pub fn extend_from_within<R: std::ops::RangeBounds<usize>>(&mut self, range: R)
    where
        T: Clone,
    {
        self.vec.extend_from_within(range);
        self.recount();
    }

    pub fn retain(&mut self, f: impl FnMut(&T) -> bool) {
        self.vec.retain(f);
    }

    pub fn drain<R: std::ops::RangeBounds<usize>>(&mut self, range: R) -> vec::Drain<'_, T> {
        self.vec.drain(range)
    }
}

impl<T> Default for TrackedVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Extend<T> for TrackedVec<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.vec.extend(iter);
        self.recount();
    }
}

impl<T> Deref for TrackedVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.vec
    }
}

impl<T> DerefMut for TrackedVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.vec
    }
}

impl<T: fmt::Debug> fmt::Debug for TrackedVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.vec.fmt(f)
    }
}

impl<'a, T> IntoIterator for &'a TrackedVec<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.vec.iter()
    }
}

impl<'a, T> IntoIterator for &'a mut TrackedVec<T> {
    type Item = &'a mut T;
    type IntoIter = std::slice::IterMut<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.vec.iter_mut()
    }
}

/// A hashbrown `HashTable` whose storage is counted against the memory limit.
///
/// It reads as the table; changes go through [`TrackedTable::update`], which recounts the storage
/// after them.
#[derive(Collect)]
#[collect(no_drop)]
pub(crate) struct TrackedTable<T> {
    table: HashTable<T>,
    bytes: TrackedBytes,
}

impl<T> TrackedTable<T> {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let table = HashTable::with_capacity(capacity);
        let bytes = TrackedBytes::new(table.allocation_size());
        TrackedTable { table, bytes }
    }

    /// Changes the table, recounting its storage after.
    pub(crate) fn update<R>(&mut self, f: impl FnOnce(&mut HashTable<T>) -> R) -> R {
        let r = f(&mut self.table);
        self.bytes.set(self.table.allocation_size());
        r
    }
}

impl<T> Deref for TrackedTable<T> {
    type Target = HashTable<T>;

    fn deref(&self) -> &HashTable<T> {
        &self.table
    }
}
