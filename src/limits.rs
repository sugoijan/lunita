//! Resource limits for running untrusted Lua code.
//!
//! A [`Lua`](crate::Lua) instance can be given [`Limits`] with
//! [`Lua::set_limits`](crate::Lua::set_limits). Every allocation whose size a script controls is
//! checked against them: stacks, table growth and buffers *before* they are made, and a refused
//! allocation raises a [`LimitError`]; a string past a limit (or a table preallocated past it) is
//! made anyway, and the limit is marked as exceeded so that the executor stops.
//!
//! A limit error cannot be caught by Lua: once one is raised, the [`Executor`](crate::Executor)
//! that was running stops at the end of the current operation, unwinding all of its threads, and
//! its result is the `LimitError` (reachable with [`RuntimeError::downcast`] or
//! [`Error::limit_error`](crate::Error::limit_error)). `pcall`, `xpcall` and coroutines never see
//! it, and the instance stays usable. Host callbacks can stop a script the same way with
//! [`Context::exceed_limit`](crate::Context::exceed_limit).
//!
//! # What the memory limit counts
//!
//! The heap blocks of all the values a `Gc` points to (garbage included until it is collected)
//! and the storage they own (stacks, the parts of tables, string bytes, buffers held across calls
//! into Lua), which the instance counts itself (see [`crate::memory`]) and
//! [`Lua::total_memory`](crate::Lua::total_memory) reports. Static strings and the lists of
//! composed functions, which only the host makes, are not counted. Allocations are refused once
//! they would take the limit less a reserve for gc-arena's
//! mark queues ([`Lua::mark_queue_reserve`](crate::Lua::mark_queue_reserve)) and less 1/16 of it
//! for the allocations lunita makes without tracking, so that the real heap stays within the
//! limit (see [`Limits::memory`]). Garbage counts until it is collected: past a quarter of the
//! limit the instance paces gc-arena's collector to run without pause and four times as fast,
//! and past three quarters [`Lua::enter`](crate::Lua::enter) first finishes a collection, charged
//! to the next `Executor::step`. [`Context::memory_room`](crate::Context::memory_room) is what may
//! still be allocated; [`Context::check_memory`](crate::Context::check_memory) and
//! [`Context::check_string`](crate::Context::check_string) check a size against the limits.
//!
//! # Fixed caps
//!
//! Not configurable: the Lua stack holds at most `MAX_STACK_SIZE` (1,000,000) values and a
//! thread at most `MAX_FRAMES` (200,000; 20,000 with `Compat::Lua51`) frames, a catchable "stack
//! overflow" past them; the parser nests at most 200 levels; the pattern matcher takes at most
//! `MAX_MATCH_STEPS` (10^5) steps per match attempt and 200 nested calls ("pattern too complex");
//! `load` from Lua compiles at most `LOAD_MAX_WORK` (20,000) units of compiler work, about 20,000
//! tokens or 320 KiB of source (past it `load` returns nil and "chunk is too complex to
//! compile"); and the compiler charges `CompileBudget::BYTES_PER_TOKEN` (384) bytes per token
//! against the memory limit.
//!
//! [`RuntimeError::downcast`]: crate::RuntimeError::downcast

use std::{cell::Cell, mem, rc::Rc};

use gc_arena::Collect;
use thiserror::Error;

use crate::{
    Context,
    fuel::{cost, fuel_for},
    memory::{Accounting, TrackedBytes},
};

/// Resource limits of a [`Lua`](crate::Lua) instance, `None` meaning unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// Bytes the instance may hold: the heap blocks of its values and the storage they own
    /// (stacks, table storage, string bytes, buffers), which it counts itself (see
    /// [`crate::memory`]).
    ///
    /// Allocations are refused once they would take this, plus a reserve for gc-arena's mark
    /// queues, past `memory - memory / 16`, so that the real heap stays within `memory`.
    ///
    /// gc-arena keeps the objects it has yet to trace in two queues it does not track, which never
    /// shrink. Each object that may contain pointers is at most once in each queue at a time, and a
    /// queue's capacity is at most twice the longest it has been, so the queues hold at most two
    /// pointers for each such object that is ever live at once, and two more for each such object
    /// that can be written to (tables, upvalues, threads, executors and `gmatch` states). The
    /// reserve is that, for the most of them ever live at once (see [`Lua::mark_queue_reserve`]):
    /// 16 to 32 bytes for each table or closure on 64-bit targets, half that on 32-bit ones, and
    /// nothing for strings. Userdata, callbacks made with [`Callback::new`] rather than
    /// [`Callback::from_fn_with`] and the function list a composed function holds are not
    /// counted: only the host makes them. The remaining 1/16 is headroom for the allocations
    /// lunita makes without tracking, such as
    /// the buffer a concatenation builds its result in (up to the string length limit).
    ///
    /// [`Lua::mark_queue_reserve`]: crate::Lua::mark_queue_reserve
    /// [`Callback::new`]: crate::Callback::new
    /// [`Callback::from_fn_with`]: crate::Callback::from_fn_with
    pub memory: Option<usize>,
    /// Longest Lua string, in bytes (a string of exactly this length is allowed).
    pub string_len: Option<usize>,
}

/// A resource limit was exceeded.
///
/// This error cannot be caught by Lua code, it always stops the running
/// [`Executor`](crate::Executor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LimitError {
    #[error("memory limit exceeded")]
    Memory,
    #[error("string length limit exceeded")]
    StringLength,
}

#[derive(Debug, Default, Collect)]
#[collect(require_static)]
pub(crate) struct LimitState {
    limits: Cell<Limits>,
    // The objects counted for the mark queue reserve, shared with the `Lua` instance.
    objects: Rc<Accounting>,
    // The first limit exceeded since this was last cleared.
    exceeded: Cell<Option<LimitError>>,
    // Fuel charged by code that does not have access to the running executor's `Fuel`.
    fuel_debt: Cell<i32>,
    // Fuel charged for a collection the instance finished between steps, paid by the next step.
    collection_fuel: Cell<i32>,
}

impl LimitState {
    pub(crate) fn new(objects: Rc<Accounting>) -> Self {
        LimitState {
            objects,
            ..LimitState::default()
        }
    }

    pub(crate) fn limits(&self) -> Limits {
        self.limits.get()
    }

    pub(crate) fn accounting(&self) -> &Accounting {
        &self.objects
    }

    pub(crate) fn set_limits(&self, limits: Limits) {
        self.limits.set(limits);
    }

    pub(crate) fn exceeded(&self) -> Option<LimitError> {
        self.exceeded.get()
    }

    pub(crate) fn exceed(&self, error: LimitError) -> LimitError {
        if self.exceeded.get().is_none() {
            self.exceeded.set(Some(error));
        }
        error
    }

    pub(crate) fn take_exceeded(&self) -> Option<LimitError> {
        self.exceeded.take()
    }

    pub(crate) fn charge_fuel(&self, fuel: i32) {
        self.fuel_debt
            .set(self.fuel_debt.get().saturating_add(fuel.max(0)));
    }

    pub(crate) fn fuel_debt(&self) -> i32 {
        self.fuel_debt.get()
    }

    pub(crate) fn take_fuel_debt(&self) -> i32 {
        self.fuel_debt.take()
    }

    pub(crate) fn charge_collection_fuel(&self, fuel: i32) {
        self.collection_fuel
            .set(self.collection_fuel.get().saturating_add(fuel.max(0)));
    }

    pub(crate) fn take_collection_fuel(&self) -> i32 {
        self.collection_fuel.take()
    }

    /// The bytes that may still be allocated when `total` are already allocated, if there is a
    /// memory limit (see [`Limits::memory`]).
    pub(crate) fn memory_room(&self, total: usize) -> Option<usize> {
        self.limits.get().memory.map(|memory| {
            refused_at(memory).saturating_sub(total.saturating_add(self.objects.reserve()))
        })
    }

    /// Checks that `additional` more bytes may be allocated when `total` are already allocated
    /// (see [`Limits::memory`]).
    pub(crate) fn check_memory(&self, total: usize, additional: usize) -> Result<(), LimitError> {
        match self.limits.get().memory {
            Some(memory)
                if total
                    .saturating_add(self.objects.reserve())
                    .saturating_add(additional)
                    > refused_at(memory) =>
            {
                Err(self.exceed(LimitError::Memory))
            }
            _ => Ok(()),
        }
    }

    /// Checks that a string of `len` bytes may be created.
    pub(crate) fn check_string_len(&self, len: usize) -> Result<(), LimitError> {
        match self.limits.get().string_len {
            Some(string_len) if len > string_len => Err(self.exceed(LimitError::StringLength)),
            _ => Ok(()),
        }
    }
}

/// The memory counted (with the reserve for the mark queues) past which allocations are refused
/// under a limit of `memory` bytes, leaving 1/16 of it as headroom (see [`Limits::memory`]).
pub(crate) fn refused_at(memory: usize) -> usize {
    memory - memory / 16
}

/// A byte buffer kept across steps or calls into Lua (such as the result of `gsub` while a
/// replacement function runs), whose capacity counts toward the memory limit.
pub(crate) struct ByteBuffer {
    bytes: Vec<u8>,
    // The capacity.
    marked: TrackedBytes,
}

impl ByteBuffer {
    pub(crate) fn new() -> Self {
        ByteBuffer {
            bytes: Vec::new(),
            marked: TrackedBytes::new(0),
        }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Appends `bytes`, checking the string length and memory limits before growing.
    pub(crate) fn extend(&mut self, ctx: Context<'_>, bytes: &[u8]) -> Result<(), LimitError> {
        let len = self.bytes.len().saturating_add(bytes.len());
        let string_len = ctx.limits().string_len;
        if string_len.is_some_and(|max| len > max) {
            return Err(ctx.exceed_limit(LimitError::StringLength));
        }
        if len > self.bytes.capacity() {
            let mut capacity = len.max(self.bytes.capacity().saturating_mul(2));
            if let Some(max) = string_len {
                capacity = capacity.min(max).max(len);
            }
            ctx.check_memory(capacity.saturating_sub(self.marked.bytes()))?;
            ctx.charge_fuel(fuel_for(capacity, cost::BYTES_COPIED));
            self.bytes
                .reserve_exact(capacity.saturating_sub(self.bytes.len()));
            self.marked.set(self.bytes.capacity());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    /// Takes the bytes out, to make a string of them (which tracks its own allocation).
    pub(crate) fn take(&mut self) -> Vec<u8> {
        self.marked.set(0);
        mem::take(&mut self.bytes)
    }
}
