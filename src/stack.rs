use std::{
    iter, mem,
    ops::{Bound, Index, IndexMut, RangeBounds},
    slice::{self, SliceIndex},
};

use allocator_api2::vec;
use gc_arena::allocator_api::MetricsAlloc;

use crate::{
    table::Growth,
    thread::{VMError, MAX_STACK_SIZE},
    Context, FromMultiValue, FromValue, IntoMultiValue, IntoValue, LimitError, TypeError, Value,
};

/// The mechanism through which all callbacks receive parameters and return values.
///
/// Each [`Thread`](crate::Thread) has its own internal stack of [`Value`]s, and this stack is
/// shared for all running Lua functions *and* callbacks.
///
/// The `Stack` is actually a mutable reference to the *top* of the internal stack inside a
/// `Thread`. In this way, we avoid needing to constantly allocate space for callback arguments
/// and returns.
pub struct Stack<'gc, 'a> {
    ctx: Context<'gc>,
    values: &'a mut vec::Vec<Value<'gc>, MetricsAlloc<'gc>>,
    bottom: usize,
}

impl<'gc, 'a> Stack<'gc, 'a> {
    /// Create a `Stack` view of all `values` above `bottom`.
    ///
    /// `bottom` must not be greater than `values.len()`, it is clamped to it if it is.
    pub fn new(
        ctx: Context<'gc>,
        values: &'a mut vec::Vec<Value<'gc>, MetricsAlloc<'gc>>,
        bottom: usize,
    ) -> Self {
        debug_assert!(values.len() >= bottom);
        let bottom = bottom.min(values.len());
        Self {
            ctx,
            values,
            bottom,
        }
    }

    pub fn reborrow(&mut self) -> Stack<'gc, '_> {
        Stack {
            ctx: self.ctx,
            values: self.values,
            bottom: self.bottom,
        }
    }

    /// A view of the stack above `bottom`, which is clamped to the length of the stack.
    pub fn sub_stack(&mut self, bottom: usize) -> Stack<'gc, '_> {
        let bottom = self.bottom.saturating_add(bottom).min(self.values.len());
        Stack {
            ctx: self.ctx,
            values: self.values,
            bottom,
        }
    }

    // The invariant `self.bottom <= self.values.len()` holds for every `Stack`, so these never
    // return an empty slice in place of the real contents.

    /// The values on the stack as a slice.
    pub fn as_slice(&self) -> &[Value<'gc>] {
        self.values.get(self.bottom..).unwrap_or_default()
    }

    /// The values on the stack as a mutable slice.
    pub fn as_mut_slice(&mut self) -> &mut [Value<'gc>] {
        self.values.get_mut(self.bottom..).unwrap_or_default()
    }

    pub fn get(&self, i: usize) -> Value<'gc> {
        self.values
            .get(self.bottom + i)
            .copied()
            .unwrap_or_default()
    }

    // Makes room for `additional` more values, checking the maximum stack size and the memory
    // limit (see [`crate::limits`]). A refusal marks the limit as exceeded, which stops the running
    // `Executor` at the end of its step, and the values are not pushed: the caller cannot raise
    // the error itself. Callbacks pushing many values should use [`Stack::try_reserve`] first,
    // whose error can be returned.
    fn grow(&mut self, additional: usize) -> bool {
        let required = self.values.len().saturating_add(additional);
        if required <= self.values.capacity() && required <= MAX_STACK_SIZE {
            return true;
        }
        match reserve_values(self.ctx, self.values, required) {
            Ok(()) => true,
            // The memory limit marked itself as exceeded.
            Err(VMError::Limit(_)) => false,
            // The stack size limit (`MAX_STACK_SIZE` values, 16 MB) counts as memory here.
            Err(_) => {
                self.ctx.exceed_limit(LimitError::Memory);
                false
            }
        }
    }

    pub fn push_back(&mut self, value: Value<'gc>) {
        if self.grow(1) {
            self.values.push(value);
        }
    }

    pub fn push_front(&mut self, value: Value<'gc>) {
        if self.grow(1) {
            self.values.insert(self.bottom, value);
        }
    }

    pub fn pop_back(&mut self) -> Option<Value<'gc>> {
        if self.values.len() > self.bottom {
            self.values.pop()
        } else {
            None
        }
    }

    pub fn pop_front(&mut self) -> Option<Value<'gc>> {
        if self.values.len() > self.bottom {
            Some(self.values.remove(self.bottom))
        } else {
            None
        }
    }

    pub fn remove(&mut self, i: usize) -> Option<Value<'gc>> {
        let index = self.bottom + i;
        if index < self.values.len() {
            Some(self.values.remove(index))
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.values.len() - self.bottom
    }

    pub fn is_empty(&self) -> bool {
        self.values.len() == self.bottom
    }

    pub fn clear(&mut self) {
        self.values.truncate(self.bottom);
    }

    pub fn resize(&mut self, size: usize) {
        let len = self.bottom.saturating_add(size);
        if len <= self.values.len() || self.grow(len - self.values.len()) {
            self.values.resize(len, Value::Nil);
        }
    }

    /// Reserve room for `additional` more values, as [`Stack::try_reserve`] does but marking a
    /// limit it exceeds instead of returning the error.
    pub fn reserve(&mut self, additional: usize) {
        let _ = self.grow(additional);
    }

    /// Reserve room for `additional` more values, checking the maximum stack size and the memory
    /// limit (see [`crate::limits`]) first.
    pub fn try_reserve(&mut self, ctx: Context<'gc>, additional: usize) -> Result<(), VMError> {
        reserve_values(
            ctx,
            self.values,
            self.values.len().saturating_add(additional),
        )
    }

    pub fn capacity(&self) -> usize {
        self.values.capacity() - self.bottom
    }

    /// Removes the given range of values from the stack, returning them.
    ///
    /// Unlike `Vec::drain`, the range is clamped to the values on the stack instead of panicking.
    pub fn drain<R: RangeBounds<usize>>(
        &mut self,
        range: R,
    ) -> vec::Drain<'_, Value<'gc>, MetricsAlloc<'gc>> {
        let len = self.len();
        let start = match range.start_bound().cloned() {
            Bound::Included(r) => r,
            Bound::Excluded(r) => r.saturating_add(1),
            Bound::Unbounded => 0,
        }
        .min(len);
        let end = match range.end_bound().cloned() {
            Bound::Included(r) => r.saturating_add(1),
            Bound::Excluded(r) => r,
            Bound::Unbounded => len,
        }
        .clamp(start, len);
        self.values.drain(self.bottom + start..self.bottom + end)
    }

    pub fn into_back(&mut self, ctx: Context<'gc>, v: impl IntoMultiValue<'gc>) {
        for v in v.into_multi_value(ctx) {
            self.push_back(v.into_value(ctx));
        }
    }

    pub fn into_front(&mut self, ctx: Context<'gc>, v: impl IntoMultiValue<'gc>) {
        let len = self.len();
        for v in v.into_multi_value(ctx) {
            self.push_back(v.into_value(ctx));
        }
        let pushed = self.len() - len;
        self.as_mut_slice().rotate_right(pushed);
    }

    pub fn from_back<V: FromValue<'gc>>(&mut self, ctx: Context<'gc>) -> Result<V, TypeError> {
        V::from_value(ctx, self.pop_back().unwrap_or_default())
    }

    pub fn from_front<V: FromValue<'gc>>(&mut self, ctx: Context<'gc>) -> Result<V, TypeError> {
        V::from_value(ctx, self.pop_front().unwrap_or_default())
    }

    pub fn replace(&mut self, ctx: Context<'gc>, v: impl IntoMultiValue<'gc>) {
        self.clear();
        self.extend(v.into_multi_value(ctx));
    }

    pub fn consume<V: FromMultiValue<'gc>>(&mut self, ctx: Context<'gc>) -> Result<V, TypeError> {
        V::from_multi_value(ctx, self.drain(..))
    }
}

impl<'gc: 'b, 'a, 'b> IntoIterator for &'b Stack<'gc, 'a> {
    type Item = Value<'gc>;
    type IntoIter = iter::Copied<slice::Iter<'b, Value<'gc>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter().copied()
    }
}

impl<'gc, 'a> Extend<Value<'gc>> for Stack<'gc, 'a> {
    fn extend<T: IntoIterator<Item = Value<'gc>>>(&mut self, iter: T) {
        let iter = iter.into_iter();
        if self.grow(iter.size_hint().0) {
            for v in iter {
                self.push_back(v);
            }
        }
    }
}

impl<'gc, 'a, 'b> Extend<Value<'gc>> for &'b mut Stack<'gc, 'a> {
    fn extend<T: IntoIterator<Item = Value<'gc>>>(&mut self, iter: T) {
        Stack::extend(self, iter);
    }
}

impl<'gc, 'a> Extend<&'a Value<'gc>> for Stack<'gc, 'a> {
    fn extend<T: IntoIterator<Item = &'a Value<'gc>>>(&mut self, iter: T) {
        Stack::extend(self, iter.into_iter().copied());
    }
}

impl<'gc: 'b, 'a, 'b, 'c> Extend<&'b Value<'gc>> for &'c mut Stack<'gc, 'a> {
    fn extend<T: IntoIterator<Item = &'b Value<'gc>>>(&mut self, iter: T) {
        Stack::extend(self, iter.into_iter().copied());
    }
}

impl<'gc, 'a, I: SliceIndex<[Value<'gc>]>> Index<I> for Stack<'gc, 'a> {
    type Output = <Vec<Value<'gc>> as Index<I>>::Output;

    #[expect(
        clippy::indexing_slicing,
        reason = "`Index` panics out of range like slices do, the library itself does not index \
                  stacks"
    )]
    fn index(&self, index: I) -> &Self::Output {
        &self.as_slice()[index]
    }
}

impl<'gc, 'a, I: SliceIndex<[Value<'gc>]>> IndexMut<I> for Stack<'gc, 'a> {
    #[expect(
        clippy::indexing_slicing,
        reason = "`IndexMut` panics out of range like slices do, the library itself does not \
                  index stacks"
    )]
    fn index_mut(&mut self, index: I) -> &mut Self::Output {
        &mut self.as_mut_slice()[index]
    }
}

/// Makes room for `values` to grow to `required` values, checking the maximum stack size and the
/// memory limit first.
pub(crate) fn reserve_values<'gc>(
    ctx: Context<'gc>,
    values: &mut vec::Vec<Value<'gc>, MetricsAlloc<'gc>>,
    required: usize,
) -> Result<(), VMError> {
    if required > MAX_STACK_SIZE {
        return Err(VMError::StackOverflow);
    }
    let capacity = values.capacity();
    if required > capacity {
        // Grow by half rather than doubling: the blocks freed by earlier growths then add up to
        // the next one after a few, so that an allocator that cannot return memory (as in wasm)
        // reuses them instead of growing its heap by the sum of all of them.
        let new_capacity = required
            .max(capacity.saturating_add(capacity / 2))
            .min(MAX_STACK_SIZE);
        ctx.check_growth(Growth {
            bytes: new_capacity.saturating_mul(mem::size_of::<Value<'gc>>()),
            entries: 0,
        })?;
        values.reserve_exact(new_capacity.saturating_sub(values.len()));
    }
    Ok(())
}
