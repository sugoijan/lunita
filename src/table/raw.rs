use std::{
    cell::Cell,
    fmt,
    hash::{Hash, Hasher},
    mem,
};

use gc_arena::{Collect, Mutation};
use thiserror::Error;

use crate::{
    Callback, Closure, Function, LimitError, String, Table, Thread, UserData, Value,
    constant::f64_to_i64,
    hash::HashKeys,
    memory::{TrackedTable, TrackedVec},
};

#[derive(Debug, Copy, Clone, Error)]
pub enum InvalidTableKey {
    #[error("table key is NaN")]
    IsNaN,
    #[error("table key is Nil")]
    IsNil,
}

/// An error setting a table entry.
#[derive(Debug, Copy, Clone, Error)]
pub enum TableError {
    #[error(transparent)]
    InvalidKey(#[from] InvalidTableKey),
    /// Growing the table to fit the entry would exceed a resource limit.
    #[error(transparent)]
    Limit(#[from] LimitError),
}

/// The work of growing or compacting a table, which [`RawTable::try_set`] and the other `try_`
/// methods give to a callback to check and charge it before doing it.
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct Growth {
    /// Bytes of new storage that will be allocated (zero if the table is only compacted).
    pub bytes: usize,
    /// Entries that will be visited (counted, moved or rehashed).
    pub entries: usize,
}

#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum NextValue<'gc> {
    /// The key provided to [`Table::next`] was found and there is an element present after it.
    ///
    /// As a special case, this may be returned if the key provided to [`Table::next`] was an
    /// integer that is a valid index into the array part of the table, *whether or not* that key
    /// is actually present. This matches the behavior of PUC-Rio Lua and is necessary to allow any
    /// field to be set to `Nil` during iteration.
    Found { key: Value<'gc>, value: Value<'gc> },
    /// The key provided to [`Table::next`] was found and it was the last entry in iteration order.
    Last,
    /// The key provided to [`Table::next`] did not match an existing entry.
    NotFound,
}

/// The storage of a [`Table`]: an array part for the integer keys `1..=n` and a map part for all
/// other keys.
///
/// DELIBERATE(puc): iteration follows insertion order, deterministic on every platform, where
/// PUC's follows its hash layout; `#t` on a table with holes may find a different border (see
/// COMPATIBILITY.md).
///
/// The map part keeps its entries in a vector in the order they were added, with a hash table
/// from keys to their position in the vector. Iteration (see [`RawTable::next`]) goes through the
/// array part and then the entries in order, so it is deterministic. Removing an entry (setting it
/// to nil) leaves it in place, with a dead key, until the map part is compacted (which only
/// happens when it has to grow), so that any field can be cleared during iteration.
#[derive(Collect)]
#[collect(no_drop)]
pub struct RawTable<'gc> {
    array: TrackedVec<Value<'gc>>,
    entries: TrackedVec<(Key<'gc>, Value<'gc>)>,
    // The position in `entries` of each entry, hashed by its key.
    index: TrackedTable<usize>,
    // The number of entries with a nil value.
    #[collect(require_static)]
    removed: usize,
    // Whether some removed entries are missing from `index`: a dead key cannot be hashed again, so
    // `rekey` leaves them out until the map part is compacted.
    #[collect(require_static)]
    unindexed: bool,
    #[collect(require_static)]
    hash_builder: ahash::random_state::RandomState,
    // The generation of the instance's hash keys `hash_builder` has, 0 if none (see
    // `RawTable::rekey`).
    #[collect(require_static)]
    hash_generation: u64,
}

impl<'gc> fmt::Debug for RawTable<'gc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(
                (1..)
                    .zip(self.array.iter())
                    .map(|(i, v)| (Value::Integer(i).debug_shallow(), v.debug_shallow()))
                    .chain(self.entries.iter().filter_map(|(k, v)| {
                        if let (Key::Live(k), false) = (k, v.is_nil()) {
                            Some((k.to_value().debug_shallow(), v.debug_shallow()))
                        } else {
                            None
                        }
                    })),
            )
            .finish()
    }
}

impl<'gc> RawTable<'gc> {
    pub fn new(mc: &Mutation<'gc>) -> Self {
        Self::with_capacity(mc, 0, 0)
    }

    /// A table with room for `array_capacity` and `map_capacity` elements, which takes the hash
    /// keys of its instance (see `RawTable::rekey`) when first set through a `Context`.
    pub fn with_capacity(mc: &Mutation<'gc>, array_capacity: usize, map_capacity: usize) -> Self {
        // Any keys do until then.
        let hash_builder = ahash::random_state::RandomState::with_seeds(0, 0, 0, 0);
        Self::with_hasher(mc, array_capacity, map_capacity, hash_builder, 0)
    }

    /// Like [`RawTable::with_capacity`], keyed with `keys` of the given generation.
    pub(crate) fn with_keys(
        mc: &Mutation<'gc>,
        array_capacity: usize,
        map_capacity: usize,
        keys: &HashKeys,
        generation: u64,
    ) -> Self {
        Self::with_hasher(
            mc,
            array_capacity,
            map_capacity,
            keys.build_hasher(),
            generation,
        )
    }

    fn with_hasher(
        mc: &Mutation<'gc>,
        array_capacity: usize,
        map_capacity: usize,
        hash_builder: ahash::random_state::RandomState,
        hash_generation: u64,
    ) -> Self {
        let _ = mc;
        let mut array = TrackedVec::new();
        array.resize(array_capacity, Value::Nil);

        let entries = TrackedVec::with_capacity(map_capacity);
        let index = TrackedTable::with_capacity(map_capacity);

        Self {
            array,
            entries,
            index,
            removed: 0,
            unindexed: false,
            hash_builder,
            hash_generation,
        }
    }

    /// The bytes [`RawTable::with_capacity`] allocates for the given capacities: the array part,
    /// the entries of the map part and its index (hashbrown rounds its buckets up to a power of
    /// two, with a load factor of at most 7/8, a byte of control per bucket and a group more).
    pub(crate) fn capacity_bytes(array_capacity: usize, map_capacity: usize) -> usize {
        let index = if map_capacity == 0 {
            0
        } else {
            let buckets = if map_capacity < 8 {
                if map_capacity < 4 { 4 } else { 8 }
            } else {
                (map_capacity.saturating_mul(8) / 7)
                    .checked_next_power_of_two()
                    .unwrap_or(usize::MAX)
            };
            buckets
                .saturating_mul(mem::size_of::<usize>() + 1)
                .saturating_add(16)
        };
        array_capacity
            .saturating_mul(mem::size_of::<Value<'gc>>())
            .saturating_add(map_capacity.saturating_mul(mem::size_of::<(Key<'gc>, Value<'gc>)>()))
            .saturating_add(index)
    }

    /// The generation of the hash keys the table hashes its keys with (see
    /// [`InternedStringSet`](crate::string::InternedStringSet)), 0 if the table has not been
    /// keyed by its instance.
    pub(crate) fn hash_generation(&self) -> u64 {
        self.hash_generation
    }

    /// Hashes the keys with `keys` from now on, rehashing the entries. Returns the number of
    /// entries rehashed.
    pub(crate) fn rekey(&mut self, keys: &HashKeys, generation: u64) -> usize {
        self.hash_builder = keys.build_hasher();
        self.hash_generation = generation;
        self.rebuild_index()
    }

    // Rebuilds the index of the map part, returning the number of entries indexed. Entries with dead
    // keys are left out (see `unindexed`).
    fn rebuild_index(&mut self) -> usize {
        let (entries, hash_builder) = (&self.entries, &self.hash_builder);
        let hasher = entry_hasher(entries, hash_builder);
        let mut unindexed = false;
        let indexed = self.index.update(|index| {
            index.clear();
            // The index only grows here, after `reserve_map_inner` grew `entries` (and checked
            // the memory for both); `rekey` finds it already large enough.
            index.reserve(entries.capacity(), &hasher);
            let mut indexed = 0;
            for (i, (key, _)) in entries.iter().enumerate() {
                if let Some(key) = key.live_key() {
                    index.insert_unique(hash_builder.hash_one(key), i, &hasher);
                    indexed += 1;
                } else {
                    unindexed = true;
                }
            }
            indexed
        });
        self.unindexed = unindexed;
        indexed
    }

    pub fn get(&self, key: Value<'gc>) -> Value<'gc> {
        if let Some(&value) = to_array_index(key).and_then(|index| self.array.get(index)) {
            return value;
        }

        match CanonicalKey::new(key) {
            Ok(key) => self
                .find(key)
                .and_then(|i| self.entries.get(i))
                .map_or(Value::Nil, |&(_, v)| v),
            Err(_) => Value::Nil,
        }
    }

    // DELIBERATE(cost): hash probes are not charged one by one: with the host's seed, long chains
    // are chance (the maintainer's decision; see docs/hardening-notes.md).
    // The position in `entries` of the entry with the given key, live or dead.
    fn find(&self, key: CanonicalKey<'gc>) -> Option<usize> {
        self.find_hashed(key).1
    }

    // The hash of a key, and the position of its entry if the map part has one.
    fn find_hashed(&self, key: CanonicalKey<'gc>) -> (u64, Option<usize>) {
        let hash = self.hash_builder.hash_one(key);
        let entries = &self.entries;
        let position = self
            .index
            .find(hash, |&i| entries.get(i).is_some_and(|(k, _)| k.eq(key)))
            .copied();
        (hash, position)
    }

    /// Set a table entry, returning the previous value.
    ///
    /// This never refuses to grow the table, see [`RawTable::try_set`].
    pub fn set(
        &mut self,
        key: Value<'gc>,
        value: Value<'gc>,
    ) -> Result<Value<'gc>, InvalidTableKey> {
        match self.try_set(key, value, |_| Ok(())) {
            Ok(previous) => Ok(previous),
            Err(TableError::InvalidKey(err)) => Err(err),
            // The allocation check never fails.
            Err(TableError::Limit(_)) => Ok(Value::Nil),
        }
    }

    /// Set a table entry, returning the previous value.
    ///
    /// If the table has to grow (or be compacted), `check_growth` is first called with the work
    /// this takes, and the entry is not set if it returns an error.
    pub fn try_set(
        &mut self,
        key: Value<'gc>,
        value: Value<'gc>,
        mut check_growth: impl FnMut(Growth) -> Result<(), LimitError>,
    ) -> Result<Value<'gc>, TableError> {
        // If the key is an array candidate and less than the current length of the array, it will
        // go there.
        let index_key = to_array_index(key);
        if let Some(slot) = index_key.and_then(|index| self.array.get_mut(index)) {
            return Ok(mem::replace(slot, value));
        }

        let table_key = CanonicalKey::new(key)?;

        // If there is an existing entry in the map part, replace its value.
        let (hash, found) = self.find_hashed(table_key);
        if let Some(i) = found {
            let Some((k, v)) = self.entries.get_mut(i) else {
                return Ok(Value::Nil);
            };
            let previous = mem::replace(v, value);
            match (previous.is_nil(), value.is_nil()) {
                (false, true) => {
                    // Removing the entry: kill the key so that it does not keep an object alive.
                    if let Some(dead) = k.kill() {
                        *k = dead;
                    }
                    self.removed += 1;
                }
                (true, false) => {
                    // Resurrect the key if it is dead.
                    *k = Key::Live(table_key);
                    self.removed = self.removed.saturating_sub(1);
                }
                _ => {}
            }
            return Ok(previous);
        }

        // Removing a missing entry does nothing.
        if value.is_nil() {
            return Ok(Value::Nil);
        }

        // If a new element does not fit in the map part, we need to grow.
        if self.entries.len() == self.entries.capacity() {
            let mut counted = 0;
            if let Some(index_key) = index_key {
                // We have an array-candidate key, so we'd like to grow the array and place it there,
                // if possible.
                let optimal_size = self.optimal_array_size(index_key);
                counted = self.array.len() + self.entries.len();

                // If we can fit our new key in an optimally sized array, resize the array and do
                // that.
                if optimal_size > index_key {
                    self.grow_array_inner(
                        optimal_size - self.array.len(),
                        counted,
                        &mut check_growth,
                    )?;
                    if let Some(slot) = self.array.get_mut(index_key) {
                        debug_assert!(slot.is_nil());
                        *slot = value;
                        return Ok(Value::Nil);
                    }
                }
            }

            // If we can't grow the array, we need to grow (or compact) the map.
            self.reserve_map_inner(1, counted, &mut check_growth)?;
        }

        let i = self.entries.len();
        self.entries.push((Key::Live(table_key), value));
        let (entries, hash_builder) = (&self.entries, &self.hash_builder);
        self.index.update(|index| {
            index.insert_unique(hash, i, entry_hasher(entries, hash_builder));
        });
        Ok(Value::Nil)
    }

    // Find the new optimal size for the array part when the key at `index_key` is inserted: the
    // largest power of two such that more than half of the array would be in use.
    fn optimal_array_size(&self, index_key: usize) -> usize {
        const USIZE_BITS: usize = mem::size_of::<usize>() * 8;

        // Count of array-candidate elements based on the highest bit in the index (which is in
        // `0..=USIZE_BITS`).
        let mut array_counts = [0usize; USIZE_BITS + 1];
        // Total count of all array-candidate elements
        let mut array_total = 0usize;

        let mut count = |i: usize| {
            if let Some(c) = array_counts.get_mut(highest_bit(i)) {
                *c += 1;
            }
            array_total += 1;
        };

        for (i, e) in self.array.iter().enumerate() {
            if !e.is_nil() {
                count(i);
            }
        }

        for &(key, value) in &self.entries {
            // Removed entries have nil values.
            if let (false, Some(key)) = (value.is_nil(), key.live_key()) {
                if let Some(i) = to_array_index(key.to_value()) {
                    count(i);
                }
            }
        }

        count(index_key);

        let mut optimal_size = 0usize;
        let mut total = 0usize;
        for (i, &c) in array_counts.iter().enumerate().take(USIZE_BITS) {
            let size = 1usize << i;
            if size / 2 >= array_total {
                break;
            }

            if c > 0 {
                total += c;
                if total > size / 2 {
                    optimal_size = size;
                }
            }
        }
        optimal_size
    }

    /// Returns a 'border' for this table.
    ///
    /// See [`Table::length`] for a more full description of what this means.
    pub fn length(&self) -> i64 {
        self.length_with_probes().0
    }

    /// An upper bound on the number of non-nil entries: the slots of the array part and the
    /// entries of the map part, empty or dead ones included. It is bounded by the table's memory,
    /// where a border ([`RawTable::length`]) of a table with holes is not: the powers of two up to
    /// `2^61`, kept in the map part, make the border `2^61`.
    pub fn entries_bound(&self) -> usize {
        self.array.len().saturating_add(self.entries.len())
    }

    /// Like [`RawTable::length`], also returning how many entries were probed to find the border,
    /// which is logarithmic in the size of the table.
    pub fn length_with_probes(&self) -> (i64, usize) {
        let probes = Cell::new(0usize);

        // Binary search for a border. Entry at max must be Nil, min must be 0 or entry at min must
        // be != Nil.
        let binary_search = |mut min: i64, mut max: i64, is_nil: &dyn Fn(i64) -> bool| -> i64 {
            while max - min > 1 {
                probes.set(probes.get() + 1);
                let mid = min + (max - min) / 2;
                if is_nil(mid) {
                    max = mid;
                } else {
                    min = mid;
                }
            }
            min
        };

        // Allocations are at most `isize::MAX` bytes.
        let array_len = i64::try_from(self.array.len()).unwrap_or(i64::MAX);

        // `i` is always in `1..=array_len` here.
        let array_is_nil = |i: i64| {
            usize::try_from(i - 1)
                .ok()
                .and_then(|i| self.array.get(i))
                .map_or(true, |v| v.is_nil())
        };

        let map_is_nil = |i: i64| {
            probes.set(probes.get() + 1);
            self.find(CanonicalKey::Integer(i))
                .and_then(|i| self.entries.get(i))
                .map_or(true, |(_, v)| v.is_nil())
        };

        let length = if self.array.last().is_some_and(|v| v.is_nil()) {
            // If the array part ends in a Nil, there must be a border inside it
            binary_search(0, array_len, &array_is_nil)
        } else if self.entries.len() == self.removed {
            // If the array part does not end in a nil but the map part is empty, then the array
            // length is a border.
            array_len
        } else {
            // Otherwise, we check the map part for a border. We need to find some nil value in the
            // map part as the max for a binary search.
            let min = array_len;
            let mut max = array_len.saturating_add(1);
            loop {
                if map_is_nil(max) {
                    // We have found a max where table[max] == nil, so we can now binary search
                    break binary_search(min, max, &map_is_nil);
                } else if max == i64::MAX {
                    // If we can't find a nil entry by doubling, then the table is pathological. We
                    // return the favor with a pathological answer: i64::MAX + 1 can't exist in the
                    // table, therefore it is Nil, so since the table contains i64::MAX, i64::MAX is
                    // a border.
                    break i64::MAX;
                } else {
                    max = max.saturating_mul(2);
                }
            }
        };
        (length, probes.get())
    }

    /// Returns the next key for this table in iteration order following the given `key`.
    ///
    /// See [`Table::next`] for a more full description of what this means.
    pub fn next(&self, key: Value<'gc>) -> NextValue<'gc> {
        self.next_with_cost(key).0
    }

    /// Like [`RawTable::next`], also returning how many slots were visited to find the next entry.
    ///
    /// This is usually small, but it is the number of removed entries skipped, so it can be up to
    /// the size of the table if most of its entries were removed.
    pub fn next_with_cost(&self, key: Value<'gc>) -> (NextValue<'gc>, usize) {
        // Where to start looking for the next non-nil entry: a (0-based) index in the array part,
        // or a position in the map part.
        enum Start {
            Array(usize),
            Map(usize),
        }

        let mut visited = 0;
        let start = if let Some(index_key) = to_array_index(key).filter(|&i| i < self.array.len()) {
            // In order to satisfy our rule that setting an entry to `Nil` is always allowed
            // during iteration, we must allow being provided a missing entry in the array portion.
            Start::Array(index_key + 1)
        } else if key.is_nil() {
            Start::Array(0)
        } else {
            let Ok(key) = CanonicalKey::new(key) else {
                return (NextValue::NotFound, 1);
            };
            match self.find(key) {
                Some(i) => Start::Map(i + 1),
                // A key removed during iteration may be one `rekey` could not index.
                None if self.unindexed => match self.entries.iter().position(|&(k, _)| k.eq(key)) {
                    Some(i) => {
                        visited = i + 1;
                        Start::Map(i + 1)
                    }
                    None => return (NextValue::NotFound, self.entries.len().max(1)),
                },
                None => return (NextValue::NotFound, 1),
            }
        };

        let map_start = match start {
            Start::Array(start) => {
                let rest = self.array.get(start..).unwrap_or_default();
                for (i, &value) in (start..).zip(rest) {
                    visited += 1;
                    if !value.is_nil() {
                        return (
                            NextValue::Found {
                                // Allocations are at most `isize::MAX` bytes.
                                key: Value::Integer(
                                    i64::try_from(i).unwrap_or(i64::MAX).wrapping_add(1),
                                ),
                                value,
                            },
                            visited,
                        );
                    }
                }
                0
            }
            Start::Map(start) => start,
        };

        for &(key, value) in self.entries.get(map_start..).unwrap_or_default() {
            visited += 1;
            // Entries with a nil value are removed, and only those can have dead keys.
            if let (false, Some(key)) = (value.is_nil(), key.live_key()) {
                return (
                    NextValue::Found {
                        key: key.to_value(),
                        value,
                    },
                    visited,
                );
            }
        }

        (NextValue::Last, visited.max(1))
    }

    /// Return the array part of the table as a slice.
    ///
    /// All integer keys that *can* fit into the array part of the table will always be stored in
    /// the array. This means that if you have an integer key `1 <= key <= array.len()`, then you
    /// can look up the value for this key with `array[key - 1]`. Such keys that are not present in
    /// the table will have a value in the array of `Value::Nil`.
    ///
    /// The length of the array part of the table and the length of the table itself are
    /// independent. There may be integral keys *outside* of the array part of the table, even for
    /// sequences.
    ///
    /// If you need to treat the table as purely array-like, and need to ensure that the entire
    /// sequence fits in the array part of the table, you can grow the array part of the table to be
    /// equal to its current length. Remember that non-sequence tables can be pathological, and can
    /// have arbitrarily large borders (numbers returned by `RawTable::length()`).
    pub fn array(&self) -> &[Value<'gc>] {
        &self.array
    }

    /// Return the array part of the table as a mutable slice.
    ///
    /// All values that *can* be stored in the array part of the table will always be stored
    /// there, so this comes with no invariants that you must uphold. Writing a value to `i` in the
    /// table is equivalent to setting the key `i + 1` in the table, and writing `Value::Nil` is
    /// equivalent to removing the key.
    pub fn array_mut(&mut self) -> &mut [Value<'gc>] {
        &mut self.array
    }

    /// Grow the array part of the table to accommodate for at *least* `additional` more elements.
    ///
    /// The array part always has a length equal to its capacity, but may have trailing nil values
    /// at the end.
    ///
    /// The map part of the table is forbidden from having keys that could be in the array part, so
    /// this may move elements from the map part of the table into the array part.
    pub fn grow_array(&mut self, additional: usize) {
        // The growth check never fails.
        let _ = self.try_grow_array(additional, |_| Ok(()));
    }

    /// Like [`RawTable::grow_array`], but if the array part has to grow, `check_growth` is first
    /// called with the work this takes, and the table is not changed if it returns an error.
    pub fn try_grow_array(
        &mut self,
        additional: usize,
        check_growth: impl FnOnce(Growth) -> Result<(), LimitError>,
    ) -> Result<(), LimitError> {
        self.grow_array_inner(additional, 0, check_growth)
    }

    // Grows the array part, `counted` is the number of entries already visited to decide to.
    fn grow_array_inner(
        &mut self,
        additional: usize,
        counted: usize,
        check_growth: impl FnOnce(Growth) -> Result<(), LimitError>,
    ) -> Result<(), LimitError> {
        let len = self.array.len();
        let capacity = self.array.capacity();
        let required = len.checked_add(additional).ok_or(LimitError::Memory)?;
        if required <= len {
            return Ok(());
        }
        // `Vec::reserve` grows to at least double the capacity.
        let new_capacity = if required > capacity {
            required.max(capacity.saturating_mul(2))
        } else {
            capacity
        };
        check_growth(Growth {
            bytes: if new_capacity > capacity {
                new_capacity.saturating_mul(mem::size_of::<Value<'gc>>())
            } else {
                0
            },
            entries: counted
                .saturating_add(new_capacity)
                .saturating_add(self.entries.len()),
        })?;
        if new_capacity > capacity {
            self.array.reserve_exact(new_capacity.saturating_sub(len));
        }
        self.array.resize(self.array.capacity(), Value::Nil);

        // We need to take any newly valid array keys from the map part, removing them there.
        let array = &mut self.array;
        for (key, value) in self.entries.iter_mut() {
            if value.is_nil() {
                continue;
            }
            if let Some(slot) = key
                .live_key()
                .and_then(|key| to_array_index(key.to_value()))
                .and_then(|i| array.get_mut(i))
            {
                *slot = mem::take(value);
                self.removed += 1;
            }
        }
        Ok(())
    }

    /// Reserve space in the map part of the table for at least `additional` more elements.
    pub fn reserve_map(&mut self, additional: usize) {
        // The growth check never fails.
        let _ = self.try_reserve_map(additional, |_| Ok(()));
    }

    /// Like [`RawTable::reserve_map`], but if the map part has to grow (or be compacted),
    /// `check_growth` is first called with the work this takes, and the table is not changed if
    /// it returns an error.
    ///
    /// Removed entries are dropped when the map part is compacted to make room, which only
    /// happens when there is not enough room.
    pub fn try_reserve_map(
        &mut self,
        additional: usize,
        check_growth: impl FnOnce(Growth) -> Result<(), LimitError>,
    ) -> Result<(), LimitError> {
        self.reserve_map_inner(additional, 0, check_growth)
    }

    // Reserves room in the map part, `counted` is the number of entries already visited to decide
    // to.
    fn reserve_map_inner(
        &mut self,
        additional: usize,
        counted: usize,
        check_growth: impl FnOnce(Growth) -> Result<(), LimitError>,
    ) -> Result<(), LimitError> {
        let len = self.entries.len();
        if additional <= self.entries.capacity() - len {
            return Ok(());
        }

        let live = len - self.removed;
        let required = live.checked_add(additional).ok_or(LimitError::Memory)?;
        let capacity = self.entries.capacity();
        // Compact in place if that leaves the map part at most half full, otherwise double it.
        let new_capacity = if required <= capacity / 2 {
            capacity
        } else {
            required.max(capacity.saturating_mul(2)).max(4)
        };

        let mut bytes = 0;
        if new_capacity > capacity {
            // The new entries vector, and the new index: hashbrown rounds its number of buckets
            // up to a power of two, with a load factor of at most 7/8, and each bucket holds an
            // index and a control byte, plus a group of control bytes.
            let buckets = if new_capacity < 8 {
                if new_capacity < 4 { 4 } else { 8 }
            } else {
                (new_capacity.saturating_mul(8) / 7)
                    .checked_next_power_of_two()
                    .ok_or(LimitError::Memory)?
            };
            let entries_size =
                new_capacity.saturating_mul(mem::size_of::<(Key<'gc>, Value<'gc>)>());
            let index_size = buckets
                .saturating_mul(mem::size_of::<usize>() + 1)
                .saturating_add(16);
            bytes = entries_size.saturating_add(index_size);
        }
        // Compacting visits every entry, and rebuilding the index every live one.
        check_growth(Growth {
            bytes,
            entries: counted.saturating_add(len).saturating_add(live),
        })?;

        // Drop the removed entries, and rebuild the index.
        if self.removed > 0 {
            self.entries.retain(|(_, v)| !v.is_nil());
            self.removed = 0;
        }
        if new_capacity > capacity {
            self.entries
                .reserve_exact(new_capacity.saturating_sub(self.entries.len()));
        }
        self.rebuild_index();
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, Eq, Collect)]
#[collect(no_drop)]
enum CanonicalKey<'gc> {
    Boolean(bool),
    Integer(i64),
    Number(u64),
    String(String<'gc>),
    Table(Table<'gc>),
    Closure(Closure<'gc>),
    Callback(Callback<'gc>),
    Thread(Thread<'gc>),
    UserData(UserData<'gc>),
}

impl<'gc> PartialEq for CanonicalKey<'gc> {
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (CanonicalKey::Boolean(a), CanonicalKey::Boolean(b)) => a == b,
            (CanonicalKey::Integer(a), CanonicalKey::Integer(b)) => a == b,
            (CanonicalKey::Number(a), CanonicalKey::Number(b)) => a == b,
            // Long string keys are only compared byte by byte if their hashes are equal.
            (CanonicalKey::String(a), CanonicalKey::String(b)) => a.eq_string(b),
            (CanonicalKey::Table(a), CanonicalKey::Table(b)) => a == b,
            (CanonicalKey::Closure(a), CanonicalKey::Closure(b)) => a == b,
            (CanonicalKey::Callback(a), CanonicalKey::Callback(b)) => a == b,
            (CanonicalKey::Thread(a), CanonicalKey::Thread(b)) => a == b,
            (CanonicalKey::UserData(a), CanonicalKey::UserData(b)) => a == b,
            _ => false,
        }
    }
}

// Consistent with `PartialEq`: strings hash by their contents (their stored hash), objects by
// their address.
impl<'gc> Hash for CanonicalKey<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        mem::discriminant(self).hash(state);
        match *self {
            CanonicalKey::Boolean(b) => b.hash(state),
            CanonicalKey::Integer(i) => i.hash(state),
            CanonicalKey::Number(n) => n.hash(state),
            CanonicalKey::String(s) => s.hash(state),
            CanonicalKey::Table(t) => t.hash(state),
            CanonicalKey::Closure(c) => c.hash(state),
            CanonicalKey::Callback(c) => c.hash(state),
            CanonicalKey::Thread(t) => t.hash(state),
            CanonicalKey::UserData(u) => u.hash(state),
        }
    }
}

impl<'gc> CanonicalKey<'gc> {
    fn new(value: Value<'gc>) -> Result<Self, InvalidTableKey> {
        Ok(match value {
            Value::Nil => {
                return Err(InvalidTableKey::IsNil);
            }
            Value::Number(n) => {
                // NaN keys are disallowed, f64 keys where their closest i64 representation is equal
                // to themselves when cast back to f64 are considered integer keys.
                if n.is_nan() {
                    return Err(InvalidTableKey::IsNaN);
                } else if let Some(i) = f64_to_i64(n) {
                    CanonicalKey::Integer(i)
                } else {
                    CanonicalKey::Number(canonical_float_bytes(n))
                }
            }
            Value::Boolean(b) => CanonicalKey::Boolean(b),
            Value::Integer(i) => CanonicalKey::Integer(i),
            Value::String(s) => CanonicalKey::String(s),
            Value::Table(t) => CanonicalKey::Table(t),
            Value::Function(Function::Closure(c)) => CanonicalKey::Closure(c),
            Value::Function(Function::Callback(c)) => CanonicalKey::Callback(c),
            Value::Thread(t) => CanonicalKey::Thread(t),
            Value::UserData(u) => CanonicalKey::UserData(u),
        })
    }

    fn to_value(self) -> Value<'gc> {
        match self {
            CanonicalKey::Boolean(b) => b.into(),
            CanonicalKey::Integer(i) => i.into(),
            CanonicalKey::Number(n) => f64::from_bits(n).into(),
            CanonicalKey::String(s) => s.into(),
            CanonicalKey::Table(t) => t.into(),
            CanonicalKey::Closure(c) => c.into(),
            CanonicalKey::Callback(c) => c.into(),
            CanonicalKey::Thread(t) => t.into(),
            CanonicalKey::UserData(u) => u.into(),
        }
    }
}

// A table key which may be "live" or "dead".
//
// "Removed" keys in tables do not actually have their entry removed, instead the value is set to
// Nil and the key is set to a dead key if it is a GC object.
//
// This is done to make iteration predictable in the presence of any table mutation that does not
// cause the table to grow.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
enum Key<'gc> {
    Live(CanonicalKey<'gc>),
    Dead(#[collect(require_static)] *const ()),
}

impl<'gc> Key<'gc> {
    // Only a GC object (`Value::as_ptr`) has a dead form.
    fn kill(self) -> Option<Key<'gc>> {
        match self {
            Key::Live(v) => v.to_value().as_ptr().map(Key::Dead),
            Key::Dead(_) => Some(self),
        }
    }

    fn live_key(self) -> Option<CanonicalKey<'gc>> {
        match self {
            Key::Live(v) => Some(v),
            Key::Dead(_) => None,
        }
    }

    fn eq(self, key: CanonicalKey<'gc>) -> bool {
        match (self, key) {
            (Key::Live(a), b) => a == b,
            (Key::Dead(a), b) => b.to_value().as_ptr() == Some(a),
        }
    }
}

// Parameter must not be NaN, should return a bit-pattern which is always equal when the
// corresponding f64s are equal (-0.0 and 0.0 return the same bit pattern).
fn canonical_float_bytes(f: f64) -> u64 {
    debug_assert!(!f.is_nan());
    if f == 0.0 {
        0.0f64.to_bits()
    } else {
        f.to_bits()
    }
}

// Hashes the key of the entry at an index of `entries`, for the index of the map part.
fn entry_hasher<'a, 'gc>(
    entries: &'a [(Key<'gc>, Value<'gc>)],
    hash_builder: &'a ahash::random_state::RandomState,
) -> impl Fn(&usize) -> u64 + use<'a, 'gc> {
    move |&i| {
        entries
            .get(i)
            .and_then(|(k, _)| k.live_key())
            .map_or(0, |k| hash_builder.hash_one(k))
    }
}

// If the given key can live in the array part of the table (integral value between 1 and
// usize::MAX), returns the associated array index.
fn to_array_index<'gc>(key: Value<'gc>) -> Option<usize> {
    let i = match key {
        Value::Integer(i) => i,
        Value::Number(f) => f64_to_i64(f)?,
        _ => return None,
    };

    if i > 0 {
        Some(usize::try_from(i).ok()? - 1)
    } else {
        None
    }
}

// Returns the place of the highest set bit in the given i, i = 0 returns 0, i = 1 returns 1, i = 2
// returns 2, i = 3 returns 2, and so on.
fn highest_bit(i: usize) -> usize {
    i.checked_ilog2().map(|i| i + 1).unwrap_or(0) as usize
}
