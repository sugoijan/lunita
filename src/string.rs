use std::{
    alloc, fmt,
    hash::{BuildHasherDefault, Hash, Hasher},
    mem, ops, slice,
    str::{self, Utf8Error},
};

use ahash::AHasher;
use gc_arena::{
    Collect, Gc, GcWeak, Mutation, Static, barrier::Unlock, collect::Trace, lock::RefLock,
};
use hashbrown::{HashMap, hash_map};

use crate::{
    compiler::string_utils::{debug_utf8_lossy, display_utf8_lossy},
    hash::{HashKeys, HashState},
    memory::{Boxed, TrackedTable, count_bytes, uncount_bytes},
    table::Growth,
};

/// The Lua string type.
///
/// Unlike Rust strings, Lua strings may contain *arbitrary bytes*, and as such are not necessarily
/// UTF-8.
#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct String<'gc>(Gc<'gc, StringInner>);

// We represent `String` as either a pointer to an external / owned slice pointer or a size prefixed
// inline array.
#[derive(Copy, Clone, Collect)]
#[collect(require_static)]
pub struct StringInner {
    hash: u64,
    buffer: Buffer,
}

#[derive(Copy, Clone)]
enum Buffer {
    Indirect(*const [u8]),
    Inline(usize),
}

impl<'gc> String<'gc> {
    /// Creates a string, which is not interned, owning the buffer.
    ///
    /// Strings are hashed when they are created and compared by their hashes first: `keys` must
    /// be the keys of the instance the string is used in, `ctx.interned_strings().hash_keys()`.
    pub fn from_buffer(mc: &Mutation<'gc>, keys: &HashKeys, s: Box<[u8]>) -> String<'gc> {
        let hash = keys.hash_bytes(&s);
        Self::from_buffer_hashed(mc, s, hash)
    }

    /// Creates a string, which is not interned, copying the bytes. See [`String::from_buffer`] for
    /// `keys`.
    pub fn from_slice(mc: &Mutation<'gc>, keys: &HashKeys, s: impl AsRef<[u8]>) -> String<'gc> {
        let s = s.as_ref();
        Self::from_slice_hashed(mc, s, keys.hash_bytes(s))
    }

    /// Creates a string, which is not interned, from static bytes. See [`String::from_buffer`] for
    /// `keys`.
    pub fn from_static<S: ?Sized + AsRef<[u8]>>(
        mc: &Mutation<'gc>,
        keys: &HashKeys,
        s: &'static S,
    ) -> String<'gc> {
        let s = s.as_ref();
        Self::from_static_hashed(mc, s, keys.hash_bytes(s))
    }

    // A string with the given hash, which is its hash with the keys of its instance.
    fn from_buffer_hashed(mc: &Mutation<'gc>, s: Box<[u8]>, hash: u64) -> String<'gc> {
        #[derive(Collect)]
        #[collect(require_static)]
        #[repr(C)]
        struct Owned {
            header: StringInner,
            // Counts the heap block; the buffer is counted by hand, as its length is only known
            // through the pointer.
            boxed: Boxed<Owned>,
        }

        impl Drop for Owned {
            fn drop(&mut self) {
                match self.header.buffer {
                    Buffer::Indirect(ptr) => unsafe {
                        uncount_bytes(ptr.len());
                        drop(Box::from_raw(ptr as *mut [u8]));
                    },
                    // `Owned` strings are always indirect.
                    Buffer::Inline(_) => {}
                }
            }
        }

        count_bytes(s.len());
        let owned = Owned {
            header: StringInner {
                hash,
                buffer: Buffer::Indirect(Box::into_raw(s)),
            },
            boxed: Boxed::new(),
        };
        // SAFETY: We know we can cast to `StringInner` because `Owned` is `#[repr(C)]`
        String(unsafe { Gc::cast::<StringInner>(Gc::new(mc, owned)) })
    }

    fn from_slice_hashed(mc: &Mutation<'gc>, s: &[u8], hash: u64) -> String<'gc> {
        // TODO: This is an extremely silly way to allocate a dynamically sized, inline string.
        // Since gc-arena does not support variable sized allocations, we try a set of static
        // sizes to inline small strings. All larger strings are instead allocated with an indirect
        // buffer. This can be improved when gc-arena learns to allocate variable sizes.

        fn create<'gc, const N: usize>(mc: &Mutation<'gc>, s: &[u8], hash: u64) -> String<'gc> {
            #[derive(Collect)]
            #[collect(require_static)]
            #[repr(C)]
            struct InlineString<const N: usize> {
                header: StringInner,
                array: [u8; N],
                // Counts the heap block (zero-sized, so `String::as_bytes` finds `array` where
                // it expects it).
                boxed: Boxed<InlineString<N>>,
            }

            assert!(s.len() <= N);
            let mut string = InlineString {
                header: StringInner {
                    hash,
                    buffer: Buffer::Inline(s.len()),
                },
                array: [0; N],
                boxed: Boxed::new(),
            };
            string.array[0..s.len()].copy_from_slice(s);

            let string = Gc::new(mc, string);
            // SAFETY: We know we can cast to `StringInner` because `InlineString` is `#[repr(C)]`
            // and `header` is the first field.
            unsafe { String(Gc::cast::<StringInner>(string)) }
        }

        macro_rules! try_sizes {
            ($($size:expr),*) => {
                $(if s.len() <= $size {
                    return create::<$size>(mc, s, hash);
                })*
            };
        }
        try_sizes!(0, 2, 4, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256);
        const _: () = assert!(MAX_INLINE_LEN == 256);

        Self::from_buffer_hashed(mc, s.into(), hash)
    }

    fn from_static_hashed(mc: &Mutation<'gc>, s: &'static [u8], hash: u64) -> String<'gc> {
        String(Gc::new(
            mc,
            StringInner {
                hash,
                buffer: Buffer::Indirect(s),
            },
        ))
    }

    pub fn from_inner(inner: Gc<'gc, StringInner>) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> Gc<'gc, StringInner> {
        self.0
    }

    pub fn stored_hash(self) -> u64 {
        self.0.hash
    }

    #[expect(
        clippy::unwrap_used,
        reason = "inline strings are at most 256 bytes, so their layout cannot overflow"
    )]
    pub fn as_bytes(self) -> &'gc [u8] {
        // SAFETY: `&'gc [u8]` has the correct lifetime because `Gc::as_ref` also returns `&'gc T`.
        unsafe {
            match self.0.buffer {
                Buffer::Indirect(p) => &(*p),
                Buffer::Inline(len) => {
                    let layout = alloc::Layout::new::<StringInner>();
                    let (_, offset) = layout
                        .extend(alloc::Layout::array::<u8>(len).unwrap())
                        .unwrap();
                    let data =
                        (Gc::as_ptr(self.0) as *const u8).offset(offset as isize) as *const u8;
                    slice::from_raw_parts(data, len)
                }
            }
        }
    }
}

// The longest string stored inline in its allocation by `String::from_slice`.
const MAX_INLINE_LEN: usize = 256;

impl<'gc> fmt::Debug for String<'gc> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "String({:?})", self.debug_lossy())
    }
}

impl<'gc> String<'gc> {
    pub fn len(self) -> i64 {
        // Allocations are at most `isize::MAX` bytes.
        self.as_bytes().len().try_into().unwrap_or(i64::MAX)
    }

    pub fn to_str(self) -> Result<&'gc str, Utf8Error> {
        str::from_utf8(self.as_bytes())
    }

    /// Display a potentially non-utf8 `String` in a lossy way.
    pub fn display_lossy(self) -> impl fmt::Display + 'gc {
        display_utf8_lossy(self.as_bytes())
    }

    /// Debug a potentially non-utf8 `String` in a lossy way.
    pub fn debug_lossy(self) -> impl fmt::Debug + 'gc {
        debug_utf8_lossy(self.as_bytes())
    }
}

impl<'gc> ops::Deref for String<'gc> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<'gc> AsRef<[u8]> for String<'gc> {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl<'gc, T> PartialEq<T> for String<'gc>
where
    T: ?Sized + AsRef<[u8]>,
{
    fn eq(&self, other: &T) -> bool {
        let (a, b) = (self.as_bytes(), other.as_ref());
        // The same bytes are equal without comparing them.
        std::ptr::eq(a, b) || a == b
    }
}

impl<'gc> String<'gc> {
    /// Compares two strings for equality, comparing their stored hashes before their contents.
    ///
    /// Strings created through [`crate::Context`] are interned, so equal strings are usually the
    /// same string, and different strings almost always have different hashes: this only compares
    /// the contents of two strings that are equal or have colliding hashes.
    pub fn eq_string(self, other: String<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
            || (self.stored_hash() == other.stored_hash() && self.as_bytes() == other.as_bytes())
    }
}

impl<'gc> Eq for String<'gc> {}

impl<'gc> Hash for String<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.stored_hash())
    }
}

struct InternedDynStringsInner<'gc>(RefLock<TrackedTable<(GcWeak<'gc, StringInner>, u64)>>);

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
struct InternedDynStrings<'gc>(Gc<'gc, InternedDynStringsInner<'gc>>);

unsafe impl<'gc> Collect<'gc> for InternedDynStringsInner<'gc> {
    fn trace<T: Trace<'gc>>(&self, cc: &mut T) {
        // SAFETY: No new Gc pointers are adopted or reparented.
        let mut dyn_strings = unsafe { self.0.unlock_unchecked() }.borrow_mut();
        // The entries of strings the last cycle freed are dropped here, as they are found.
        dyn_strings.update(|table| table.retain(|(s, _)| !s.is_dropped()));
        for (s, _) in dyn_strings.iter() {
            cc.trace(s);
        }
    }
}

impl<'gc> InternedDynStrings<'gc> {
    fn new(mc: &Mutation<'gc>) -> Self {
        Self(Gc::new(
            mc,
            InternedDynStringsInner(RefLock::new(TrackedTable::with_capacity(0))),
        ))
    }

    // Returns the interned string equal to `s`, whose hash is `hash`, or interns the string created
    // from `s` by `create`, which must be equal to `s` and have the hash `hash` (only created if
    // there is no equal string).
    //
    // If the set of interned strings has to grow, `can_grow` is first called with the work this
    // takes, and if it returns false the new string is returned without being interned.
    // DELIBERATE(cost): hash probes are not charged one by one: with the host's seed, long chains
    // are chance (the maintainer's decision; see docs/hardening-notes.md).
    fn intern_with<S: AsRef<[u8]>>(
        self,
        mc: &Mutation<'gc>,
        s: S,
        hash: u64,
        create: impl FnOnce(&Mutation<'gc>, S) -> String<'gc>,
        can_grow: impl FnOnce(Growth) -> bool,
    ) -> String<'gc> {
        // SAFETY: If a new string is added, we call the write barrier.
        let mut dyn_strings = unsafe { self.0.0.unlock_unchecked() }.borrow_mut();

        // The entries of freed strings are skipped here and dropped at the next collection.
        if let Some(&(key, _)) = dyn_strings.find(hash, |&(key, _)| {
            key.upgrade(mc)
                .is_some_and(|st| String::from_inner(st) == s.as_ref())
        }) {
            if let Some(st) = key.upgrade(mc) {
                return String::from_inner(st);
            }
        }

        let len = dyn_strings.len();
        if len == dyn_strings.capacity() {
            // The table will grow (to at least double the buckets, rounded up to a power of two
            // with a load factor of 7/8) and rehash its entries, or rehash in place after
            // removals; the growth is charged as the worst case.
            let buckets = (dyn_strings.capacity().saturating_mul(2).max(4))
                .saturating_mul(8)
                .checked_div(7)
                .and_then(usize::checked_next_power_of_two)
                .unwrap_or(usize::MAX);
            let growth = Growth {
                bytes: buckets
                    .saturating_mul(mem::size_of::<(GcWeak<'gc, StringInner>, u64)>() + 1)
                    .saturating_add(16),
                entries: len,
            };
            if !can_grow(growth) {
                return create(mc, s);
            }
        }

        // SAFETY: We are going to modify the dyn_strings table, so call the write barrier.
        Gc::write(mc, self.0);

        let s = create(mc, s);
        dyn_strings.update(|table| {
            table.insert_unique(
                s.stored_hash(),
                (Gc::downgrade(s.into_inner()), s.stored_hash()),
                |(_, hash)| *hash,
            );
        });

        s
    }
}

#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
struct InternedStaticStrings<'gc>(
    Gc<'gc, RefLock<HashMap<Static<*const [u8]>, String<'gc>, BuildHasherDefault<AHasher>>>>,
);

impl<'gc> InternedStaticStrings<'gc> {
    fn new(mc: &Mutation<'gc>) -> Self {
        Self(Gc::new(
            mc,
            RefLock::new(HashMap::with_hasher(BuildHasherDefault::default())),
        ))
    }

    fn intern(self, mc: &Mutation<'gc>, hash: &HashState, s: &'static [u8]) -> String<'gc> {
        let key = Static(s as *const _);

        // SAFETY: If a new string is added, we call the write barrier.
        let mut static_strings = unsafe { self.0.unlock_unchecked() }.borrow_mut();

        match static_strings.entry(key) {
            hash_map::Entry::Occupied(occupied) => *occupied.get(),
            hash_map::Entry::Vacant(vacant) => {
                // SAFETY: We are modifying the static_strings table, so we call the write barrier.
                Gc::write(mc, self.0);
                *vacant.insert(String::from_static_hashed(mc, s, hash.hash_bytes(s)))
            }
        }
    }
}

/// A set of shared, immutable `String` values that are de-duplicated to save space.
///
/// If the given string is the same as a previously interned string, and that interned string is
/// still "live", then a pointer to the previous string is returned instead of a newly allocated
/// string.
///
/// If there is no matching existing live interned string, then a new string is allocated.
#[derive(Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct InternedStringSet<'gc> {
    dyn_strings: InternedDynStrings<'gc>,
    static_strings: InternedStaticStrings<'gc>,
    hash: Gc<'gc, HashState>,
}

impl<'gc> InternedStringSet<'gc> {
    pub fn new(mc: &Mutation<'gc>) -> InternedStringSet<'gc> {
        InternedStringSet {
            dyn_strings: InternedDynStrings::new(mc),
            static_strings: InternedStaticStrings::new(mc),
            hash: Gc::new(mc, HashState::default()),
        }
    }

    /// The keys strings are hashed with (see [`crate::hash`]). Once they have been asked for, or a
    /// string has been made, they no longer change.
    pub fn hash_keys(self) -> HashKeys {
        self.hash.string_keys()
    }

    /// Derives the hash keys from `seed`, unless a string has already been made (or the keys asked
    /// for), returning whether it did.
    pub fn set_hash_seed(self, seed: u64) -> bool {
        self.hash.set_seed(seed)
    }

    // The keys for tables and the compiler, and their generation, which changes with the seed.
    pub(crate) fn table_hash_keys(self) -> HashKeys {
        self.hash.keys()
    }

    pub(crate) fn hash_generation(self) -> u64 {
        self.hash.generation()
    }

    // DELIBERATE(puc): long strings are interned like short ones, so equal strings are one object
    // (PUC's `%p` differs for equal long strings; see COMPATIBILITY.md).
    pub fn intern(self, mc: &Mutation<'gc>, s: &[u8]) -> String<'gc> {
        self.intern_checked(mc, s, |_| true)
    }

    /// Like [`InternedStringSet::intern`], but if the set has to grow, `can_grow` is first called
    /// with the work this takes, and if it returns false the new string is returned without being
    /// interned.
    pub fn intern_checked(
        self,
        mc: &Mutation<'gc>,
        s: &[u8],
        can_grow: impl FnOnce(Growth) -> bool,
    ) -> String<'gc> {
        let hash = self.hash.hash_bytes(s);
        self.dyn_strings.intern_with(
            mc,
            s,
            hash,
            |mc, s| String::from_slice_hashed(mc, s, hash),
            can_grow,
        )
    }

    /// Like [`InternedStringSet::intern_checked`], but takes ownership of the buffer instead of
    /// copying it if there is no equal interned string.
    pub fn intern_buffer(
        self,
        mc: &Mutation<'gc>,
        s: Box<[u8]>,
        can_grow: impl FnOnce(Growth) -> bool,
    ) -> String<'gc> {
        // Short strings are stored inline in their allocation.
        if s.len() <= MAX_INLINE_LEN {
            return self.intern_checked(mc, &s, can_grow);
        }
        let hash = self.hash.hash_bytes(&s);
        self.dyn_strings.intern_with(
            mc,
            s,
            hash,
            |mc, s| String::from_buffer_hashed(mc, s, hash),
            can_grow,
        )
    }

    pub fn intern_static(self, mc: &Mutation<'gc>, s: &'static [u8]) -> String<'gc> {
        self.static_strings.intern(mc, &self.hash, s)
    }
}

#[cfg(test)]
mod tests {
    use gc_arena::arena::rootless_mutate;

    use super::*;

    #[test]
    fn test_string_header() {
        rootless_mutate(|mc| {
            let keys = &HashKeys::default();
            let test1 = String::from_buffer(mc, keys, Box::from(b"test 1".as_slice()));
            let test2 = String::from_buffer(mc, keys, Box::from(b"test 2".as_slice()));

            let test3 = String::from_slice(mc, keys, b"test 3");
            let test4 = String::from_slice(mc, keys, b"test 4444 4444 4444 4444");

            let test5 = String::from_static(mc, keys, b"test 55555 55555 55555 55555 55555");
            let test6 = String::from_static(mc, keys, b"test 666666");

            assert_eq!(test1.as_bytes(), b"test 1");
            assert_eq!(test2.as_bytes(), b"test 2");
            assert_eq!(test3.as_bytes(), b"test 3");
            assert_eq!(test4.as_bytes(), b"test 4444 4444 4444 4444");
            assert_eq!(test5.as_bytes(), b"test 55555 55555 55555 55555 55555");
            assert_eq!(test6.as_bytes(), b"test 666666");
        });
    }
}
