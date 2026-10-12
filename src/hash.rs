//! The keys a [`Lua`](crate::Lua) instance hashes strings and table keys with.
//!
//! The hash of a string is computed once, when the string is made, and tables and the string
//! interner place their entries by hash: whoever knows the keys can compute offline many strings
//! (or numbers) whose hashes collide, and make every lookup among them scan all of them. The keys
//! are derived from a seed the host sets with [`Lua::set_hash_seed`](crate::Lua::set_hash_seed),
//! so that collisions cannot be computed without it. Without a seed, an instance uses the keys of
//! seed 0, which are in the source.

use std::{
    cell::{Cell, RefCell},
    hash::{BuildHasher, Hasher},
};

use gc_arena::Collect;

/// Keys for hashing strings and table keys, derived from a 64-bit seed.
#[derive(Clone, Debug)]
pub struct HashKeys(ahash::RandomState);

impl HashKeys {
    pub fn new(seed: u64) -> HashKeys {
        // SplitMix64, so that every bit of the seed affects every key.
        let mut state = seed;
        let mut next = || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        HashKeys(ahash::RandomState::with_seeds(
            next(),
            next(),
            next(),
            next(),
        ))
    }

    /// The hash of a string with these keys.
    pub fn hash_bytes(&self, bytes: &[u8]) -> u64 {
        let mut hasher = self.0.build_hasher();
        hasher.write(bytes);
        hasher.finish()
    }

    pub(crate) fn build_hasher(&self) -> ahash::RandomState {
        self.0.clone()
    }
}

impl Default for HashKeys {
    /// The keys of seed 0.
    fn default() -> Self {
        HashKeys::new(0)
    }
}

/// The hash keys of an instance, and whether any string has been hashed with them yet.
#[derive(Collect)]
#[collect(require_static)]
pub(crate) struct HashState {
    keys: RefCell<HashKeys>,
    // Incremented whenever the keys change, so that tables keyed with older keys rehash.
    generation: Cell<u64>,
    used: Cell<bool>,
}

impl Default for HashState {
    fn default() -> Self {
        HashState {
            keys: RefCell::new(HashKeys::default()),
            generation: Cell::new(1),
            used: Cell::new(false),
        }
    }
}

impl HashState {
    /// The keys, for tables and the compiler, which rehash when the keys change (see
    /// [`HashState::generation`]).
    pub(crate) fn keys(&self) -> HashKeys {
        self.keys.borrow().clone()
    }

    /// The keys, for hashing strings with: they may not change from now on.
    pub(crate) fn string_keys(&self) -> HashKeys {
        self.used.set(true);
        self.keys.borrow().clone()
    }

    /// Hashes a string with the keys, which may not change from now on.
    pub(crate) fn hash_bytes(&self, bytes: &[u8]) -> u64 {
        self.used.set(true);
        self.keys.borrow().hash_bytes(bytes)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.get()
    }

    /// Sets the keys from a seed, unless a string has already been hashed with the current keys.
    pub(crate) fn set_seed(&self, seed: u64) -> bool {
        if self.used.get() {
            return false;
        }
        *self.keys.borrow_mut() = HashKeys::new(seed);
        self.generation.set(self.generation.get().wrapping_add(1));
        true
    }
}
