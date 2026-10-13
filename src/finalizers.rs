use gc_arena::{Collect, Finalization, Gc, GcWeak, Mutation, lock::RefLock};

use crate::{Thread, memory::TrackedVec, thread::ThreadInner};

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Finalizers<'gc>(Gc<'gc, RefLock<FinalizersState<'gc>>>);

impl<'gc> Finalizers<'gc> {
    const THREAD_ERR: &'static str = "thread finalization was missed";

    pub(crate) fn new(mc: &Mutation<'gc>) -> Self {
        Finalizers(Gc::new(
            mc,
            RefLock::new(FinalizersState {
                threads: TrackedVec::new(),
            }),
        ))
    }

    pub(crate) fn register_thread(&self, mc: &Mutation<'gc>, ptr: Gc<'gc, ThreadInner<'gc>>) {
        self.0.borrow_mut(mc).threads.push(Gc::downgrade(ptr));
    }

    /// First stage of two-stage finalization.
    ///
    /// This stage can cause resurrection, so the arena must be *fully re-marked* before stage two
    /// (`Finalizers::finalize`).
    #[expect(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "registered threads are only removed in `finalize`, and no thread can be \
                  borrowed outside of `Lua::enter`, where finalization happens"
    )]
    pub(crate) fn prepare(&self, fc: &Finalization<'gc>) {
        let state = self.0.borrow();
        for &ptr in &state.threads {
            let thread = Thread::from_inner(ptr.upgrade(fc).expect(Self::THREAD_ERR));
            thread.resurrect_live_upvalues(fc).unwrap();
        }
    }

    /// Second stage of two-stage finalization.
    ///
    /// Assuming stage one was called (`Finalizers::prepare`) and the arena fully re-marked, this
    /// method will *not* cause any resurrection.
    ///
    /// The arena must *immediately* transition to `CollectionPhase::Sweeping` afterwards to not
    /// miss any finalizers.
    #[expect(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "registered threads are only removed here, and no thread can be borrowed \
                  outside of `Lua::enter`, where finalization happens"
    )]
    pub(crate) fn finalize(&self, fc: &Finalization<'gc>) {
        let mut state = self.0.borrow_mut(fc);
        state.threads.retain(|&ptr| {
            let ptr = ptr.upgrade(fc).expect(Self::THREAD_ERR);
            if Gc::is_dead(fc, ptr) {
                Thread::from_inner(ptr).reset(fc).unwrap();
                false
            } else {
                true
            }
        });
    }
}

#[derive(Collect)]
#[collect(no_drop)]
struct FinalizersState<'gc> {
    threads: TrackedVec<GcWeak<'gc, ThreadInner<'gc>>>,
}
