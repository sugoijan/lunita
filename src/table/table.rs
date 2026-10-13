use std::{
    fmt,
    hash::{Hash, Hasher},
    mem,
};

use gc_arena::{lock::RefLock, Collect, Gc, Mutation};

use crate::{
    fuel::{cost, fuel_for},
    memory::Written,
    table::Growth,
    Context, FromValue, IntoValue, LimitError, TypeError, Value,
};

use super::raw::{InvalidTableKey, NextValue, RawTable, TableError};

pub type TableInner<'gc> = RefLock<TableState<'gc>>;

/// The primary Lua data structure.
///
/// A `Table` is a combination of an array and a map. It a map of [`Value`]s to other `Value`s, but
/// as an optimization, when keys are integral and start from 1, it stores them within an internal
/// array.
///
/// Entries with values of [`Value::Nil`] are transparently removed from the table.
///
/// When a `Table` has only integral keys starting from 1, and every value is non-nil, then it is
/// also known as a "sequence" and acts similar to how an array would in other languages.
///
/// All Lua tables can have another associated table called a "metatable" which governs how they
/// act in Lua code. In Lua code, operations on a table can trigger special "metamethods" in the
/// metatable (if they are present).
///
/// On the Rust side, all methods on `Table` are "raw", which in Lua jargon means that they
/// never trigger metamethods. This MUST be true, because `lunita` does not (and cannot)
/// silently trigger running Lua code. In order to trigger metamethods, you must use the
/// [`meta_ops`](crate::meta_ops) module and manually run any triggered code on an
/// [`Executor`](crate::Executor).
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Table<'gc>(Gc<'gc, TableInner<'gc>>);

impl<'gc> PartialEq for Table<'gc> {
    fn eq(&self, other: &Table<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl<'gc> Eq for Table<'gc> {}

impl<'gc> Hash for Table<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Gc::as_ptr(self.0).hash(state)
    }
}

impl<'gc> Table<'gc> {
    pub fn new(mc: &Mutation<'gc>) -> Table<'gc> {
        Self::from_parts(mc, RawTable::new(mc), None)
    }

    /// A table with room for `array_capacity` and `map_capacity` elements, keyed with the hash
    /// keys of the instance.
    pub fn with_capacity(
        ctx: Context<'gc>,
        array_capacity: usize,
        map_capacity: usize,
    ) -> Table<'gc> {
        match Self::try_with_capacity(ctx, array_capacity, map_capacity) {
            Ok(table) => table,
            // Marked as exceeded by the check; the table is made small.
            Err(_) => Self::new(&ctx),
        }
    }

    /// Like [`Table::with_capacity`], checking the memory limit for the room and charging fuel
    /// for it (`Context::check_growth`); a refusal makes no table.
    pub fn try_with_capacity(
        ctx: Context<'gc>,
        array_capacity: usize,
        map_capacity: usize,
    ) -> Result<Table<'gc>, LimitError> {
        if array_capacity > 0 || map_capacity > 0 {
            ctx.check_growth(Growth {
                bytes: RawTable::capacity_bytes(array_capacity, map_capacity),
                entries: array_capacity.saturating_add(map_capacity),
            })?;
        }
        Ok(Self::with_capacity_unchecked(
            ctx,
            array_capacity,
            map_capacity,
        ))
    }

    fn with_capacity_unchecked(
        ctx: Context<'gc>,
        array_capacity: usize,
        map_capacity: usize,
    ) -> Table<'gc> {
        let strings = ctx.interned_strings();
        let raw_table = RawTable::with_keys(
            &ctx,
            array_capacity,
            map_capacity,
            &strings.table_hash_keys(),
            strings.hash_generation(),
        );
        Self::from_parts(&ctx, raw_table, None)
    }

    pub fn from_parts(
        mc: &Mutation<'gc>,
        raw_table: RawTable<'gc>,
        metatable: Option<Table<'gc>>,
    ) -> Table<'gc> {
        Self(Gc::new(
            mc,
            RefLock::new(TableState {
                raw_table,
                metatable,
                queued: Written::new(),
            }),
        ))
    }

    pub fn from_inner(inner: Gc<'gc, TableInner<'gc>>) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> Gc<'gc, TableInner<'gc>> {
        self.0
    }

    pub fn get<K: IntoValue<'gc>, V: FromValue<'gc>>(
        self,
        ctx: Context<'gc>,
        key: K,
    ) -> Result<V, TypeError> {
        V::from_value(ctx, self.get_value(ctx, key))
    }

    /// Set a value in this table, returning the previous value.
    ///
    /// If the table has to grow, this first checks the memory limit of the Lua instance (see
    /// [`crate::limits`]), and does not set the value if it would be exceeded.
    pub fn set<K: IntoValue<'gc>, V: IntoValue<'gc>>(
        self,
        ctx: Context<'gc>,
        key: K,
        value: V,
    ) -> Result<Value<'gc>, TableError> {
        let (key, value) = (key.into_value(ctx), value.into_value(ctx));
        let mut state = self.0.borrow_mut(&ctx);
        // A table made without a `Context` (or before the hash seed was set) takes the instance's
        // hash keys the first time it is set through one.
        let strings = ctx.interned_strings();
        if state.raw_table.hash_generation() != strings.hash_generation() {
            let rehashed = state
                .raw_table
                .rekey(&strings.table_hash_keys(), strings.hash_generation());
            ctx.charge_fuel(fuel_for(rehashed, cost::TABLE_SLOTS));
        }
        state
            .raw_table
            .try_set(key, value, |growth| ctx.check_growth(growth))
    }

    pub fn get_value<K: IntoValue<'gc>>(self, ctx: Context<'gc>, key: K) -> Value<'gc> {
        self.get_raw(key.into_value(ctx))
    }

    /// A convenience method over [`Table::set`] for setting a string field of a table.
    ///
    /// It behaves exactly the same as [`Table::set`], except since this only accepts string keys,
    /// the only error left is the memory limit refusing to grow the table (see
    /// [`crate::limits`]), in which case the field is not set.
    pub fn set_field<V: IntoValue<'gc>>(
        self,
        ctx: Context<'gc>,
        key: &'static str,
        value: V,
    ) -> Result<Value<'gc>, LimitError> {
        match self.set(ctx, key, value) {
            Ok(previous) => Ok(previous),
            Err(TableError::Limit(err)) => Err(err),
            // A string key is always a valid key.
            Err(TableError::InvalidKey(_)) => Ok(Value::Nil),
        }
    }

    /// Get a value from this table without any automatic type conversion.
    pub fn get_raw(self, key: Value<'gc>) -> Value<'gc> {
        self.0.borrow().raw_table.get(key)
    }

    /// Grows the array part of this table to fit at least `additional` more elements (see
    /// [`RawTable::grow_array`]), checking the memory limit and charging fuel for the work first.
    pub fn grow_array(self, ctx: Context<'gc>, additional: usize) -> Result<(), LimitError> {
        self.0
            .borrow_mut(&ctx)
            .raw_table
            .try_grow_array(additional, |growth| ctx.check_growth(growth))
    }

    /// Set a value in this table without any automatic type conversion.
    ///
    /// Unlike [`Table::set`], this does not check the memory limit.
    pub fn set_raw(
        self,
        mc: &Mutation<'gc>,
        key: Value<'gc>,
        value: Value<'gc>,
    ) -> Result<Value<'gc>, InvalidTableKey> {
        self.0.borrow_mut(&mc).raw_table.set(key, value)
    }

    /// Returns a 'border' for this table.
    ///
    /// A 'border' for a table is any i >= 0 where:
    /// `(i == 0 or table[i] ~= nil) and table[i + 1] == nil`
    ///
    /// If a table has exactly one border, it is called a 'sequence', and this border is the table's
    /// length.
    pub fn length(self) -> i64 {
        self.0.borrow().raw_table.length()
    }

    /// Returns a border for this table like [`Table::length`], charging fuel for the search to
    /// the running `Executor` (see [`Context::charge_fuel`]).
    pub fn length_charged(self, ctx: Context<'gc>) -> i64 {
        let (length, probes) = self.0.borrow().raw_table.length_with_probes();
        ctx.charge_fuel(fuel_for(probes, cost::TABLE_LOOKUPS));
        length
    }

    /// Returns the next value after this key in the table's iteration order: the array part in
    /// index order, then the map part in insertion order.
    ///
    /// The order is only guaranteed to be stable if there are no inserts into the table, so
    /// iterating using this method while simultaneously inserting may result in unspecified (but
    /// never unsafe) behavior. As a special case, removing from the table (setting a value to
    /// [`Value::Nil`]) is *always* allowed, even for the currently returned key.
    ///
    /// If given `Nil`, it will return the first pair in the table. If given a key that is present
    /// in the table, it will return the next pair in iteration order. If given a key that is not
    /// present in the table, it returns [`NextValue::NotFound`].
    pub fn next(self, key: Value<'gc>) -> NextValue<'gc> {
        self.0.borrow().raw_table.next(key)
    }

    /// Like [`Table::next`], charging fuel to the running `Executor` for the removed entries it
    /// skips (see [`Context::charge_fuel`]).
    pub fn next_charged(self, ctx: Context<'gc>, key: Value<'gc>) -> NextValue<'gc> {
        let (next, visited) = self.0.borrow().raw_table.next_with_cost(key);
        ctx.charge_fuel(fuel_for(visited, cost::TABLE_SLOTS));
        next
    }

    /// Iterate over the key-value pairs of the table.
    ///
    /// Internally uses the `Table::next` method and thus matches the behavior of Lua.
    pub fn iter(self) -> Iter<'gc> {
        Iter::new(self)
    }

    pub fn metatable(self) -> Option<Table<'gc>> {
        self.0.borrow().metatable
    }

    pub fn set_metatable(
        self,
        mc: &Mutation<'gc>,
        metatable: Option<Table<'gc>>,
    ) -> Option<Table<'gc>> {
        mem::replace(&mut self.0.borrow_mut(mc).metatable, metatable)
    }
}

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Iter<'gc> {
    table: Table<'gc>,
    prev: Option<Value<'gc>>,
}

impl<'gc> Iter<'gc> {
    pub fn new(table: Table<'gc>) -> Self {
        Self {
            table,
            prev: Some(Value::Nil),
        }
    }
}

impl<'gc> Iterator for Iter<'gc> {
    type Item = (Value<'gc>, Value<'gc>);

    fn next(&mut self) -> Option<Self::Item> {
        match self.table.next(self.prev.take()?) {
            NextValue::Found { key, value } => {
                self.prev = Some(key);
                Some((key, value))
            }
            _ => None,
        }
    }
}

impl<'gc> IntoIterator for Table<'gc> {
    type Item = (Value<'gc>, Value<'gc>);
    type IntoIter = Iter<'gc>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[derive(Collect)]
#[collect(no_drop)]
pub struct TableState<'gc> {
    pub raw_table: RawTable<'gc>,
    pub metatable: Option<Table<'gc>>,
    queued: Written<RefLock<TableState<'static>>>,
}

impl<'gc> fmt::Debug for TableState<'gc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct ShallowTableDebug<'gc>(Table<'gc>);

        impl<'gc> fmt::Debug for ShallowTableDebug<'gc> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let t = self.0.into_inner().borrow();
                match t.metatable {
                    Some(meta) => f
                        .debug_struct("TableState")
                        .field("raw_table", &t.raw_table)
                        .field(
                            "metatable",
                            &Some(format_args!("Table({:p})", Gc::as_ptr(meta.into_inner()))),
                        )
                        .finish(),
                    None => f
                        .debug_struct("TableState")
                        .field("raw_table", &t.raw_table)
                        .field("metatable", &None::<()>)
                        .finish(),
                }
            }
        }

        f.debug_struct("TableState")
            .field("raw_table", &self.raw_table)
            .field(
                "metatable",
                &self.metatable.as_ref().map(|t| ShallowTableDebug(*t)),
            )
            .finish()
    }
}
