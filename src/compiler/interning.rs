use std::rc::Rc;

use ahash::HashSet;

use crate::hash::HashKeys;

pub trait StringInterner {
    type String: AsRef<[u8]> + Clone;

    fn intern(&mut self, s: &[u8]) -> Self::String;

    /// The keys the compiler hashes constants with: those of the instance the chunk is compiled
    /// for, so that a chunk cannot be made of constants whose hashes collide (see
    /// [`crate::hash`]).
    fn hash_keys(&self) -> HashKeys {
        HashKeys::default()
    }
}

impl<'a, S: StringInterner> StringInterner for &'a mut S {
    type String = S::String;

    fn intern(&mut self, s: &[u8]) -> Self::String {
        S::intern(self, s)
    }

    fn hash_keys(&self) -> HashKeys {
        S::hash_keys(self)
    }
}

pub struct BasicInterner(HashSet<Rc<[u8]>>);

impl Default for BasicInterner {
    fn default() -> Self {
        BasicInterner(HashSet::with_hasher(ahash::RandomState::new()))
    }
}

impl StringInterner for BasicInterner {
    type String = Rc<[u8]>;

    fn intern(&mut self, string: &[u8]) -> Self::String {
        if let Some(s) = self.0.get(string) {
            s.clone()
        } else {
            let s = Rc::from(Box::from(string));
            self.0.insert(Rc::clone(&s));
            s
        }
    }
}
