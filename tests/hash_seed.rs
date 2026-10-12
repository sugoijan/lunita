//! Strings and table keys are hashed with keys derived from the seed set by `Lua::set_hash_seed`,
//! so that strings whose hashes collide cannot be computed without the seed.

use std::hash::Hasher;

use piccolo::{hash::HashKeys, Closure, Executor, Lua, Table, Value};

/// The numbers `n` for which the low 16 bits of `AHasher::default()`'s hash of `"k<n>"` are zero,
/// found in review (with ahash 0.8.11's AES hasher, on aarch64): piccolo hashed strings with
/// `AHasher::default()`, whose keys are fixed, before the hash seed.
fn review_collisions() -> Vec<u64> {
    let mut n = 0;
    include_str!("data/r2-collisions.txt")
        .split(|c: char| !c.is_ascii_digit())
        .filter(|d| !d.is_empty())
        .map(|d| {
            n += d.parse::<u64>().unwrap();
            n
        })
        .collect()
}

/// The hashes piccolo gives `strings` in an instance seeded with `seed` (unseeded if `None`).
fn hashes(seed: Option<u64>, strings: &[String]) -> Vec<u64> {
    let mut lua = Lua::empty();
    if let Some(seed) = seed {
        assert!(lua.set_hash_seed(seed));
    }
    lua.enter(|ctx| {
        strings
            .iter()
            .map(|s| ctx.intern(s.as_bytes()).stored_hash())
            .collect()
    })
}

/// The size of the largest group of hashes with the same `bits` low bits.
fn largest_group(hashes: &[u64], bits: u32) -> usize {
    let mut counts = vec![0usize; 1 << bits];
    for h in hashes {
        counts[(h & ((1 << bits) - 1)) as usize] += 1;
    }
    counts.into_iter().max().unwrap()
}

#[test]
fn seed_before_strings() {
    let mut lua = Lua::empty();
    assert!(lua.set_hash_seed(1));
    // Still no string made.
    assert!(lua.set_hash_seed(2));
    lua.load_core().unwrap();
    assert!(!lua.set_hash_seed(3));
    assert!(!Lua::core().set_hash_seed(1));

    // An instance's strings are hashed with the keys of its seed.
    let hash = hashes(Some(2), &["hello".to_owned()])[0];
    assert_eq!(hash, HashKeys::new(2).hash_bytes(b"hello"));
    assert_eq!(
        hashes(None, &["hello".to_owned()])[0],
        HashKeys::default().hash_bytes(b"hello")
    );
    assert_ne!(hash, HashKeys::new(3).hash_bytes(b"hello"));
}

#[test]
fn review_collisions_spread() {
    let strings: Vec<String> = review_collisions()
        .into_iter()
        .map(|n| format!("k{n}"))
        .collect();
    assert_eq!(strings.len(), 65536);

    let old = |s: &str| {
        let mut hasher = ahash::AHasher::default();
        hasher.write(s.as_bytes());
        hasher.finish()
    };
    let colliding = strings.iter().filter(|s| old(s) & 0xffff == 0).count();
    // Other targets may use ahash's fallback hasher, under which these do not collide.
    if cfg!(all(target_arch = "aarch64", target_feature = "aes")) {
        assert_eq!(colliding, strings.len());
    }

    // With piccolo's keys, seeded or not, they spread over the low 16 bits like any 65,536
    // strings (the largest of 65,536 groups is about 8 then).
    for seed in [None, Some(1), Some(0x5eed_5eed_5eed_5eed)] {
        let largest = largest_group(&hashes(seed, &strings), 16);
        assert!(
            largest <= 16,
            "seed {seed:?}: {largest} strings in one group"
        );
    }
}

#[test]
fn collisions_under_one_seed_spread_under_another() {
    // Strings colliding in the low 12 bits under the keys of seed 7, as someone knowing the seed
    // could compute them.
    let keys = HashKeys::new(7);
    let strings: Vec<String> = (0u64..)
        .map(|n| format!("k{n}"))
        .filter(|s| keys.hash_bytes(s.as_bytes()) & 0xfff == 0)
        .take(256)
        .collect();
    assert_eq!(largest_group(&hashes(Some(7), &strings), 12), 256);
    for seed in [None, Some(8), Some(u64::MAX)] {
        let largest = largest_group(&hashes(seed, &strings), 12);
        assert!(
            largest <= 6,
            "seed {seed:?}: {largest} strings in one group"
        );
    }
}

#[test]
fn tables_take_the_seed() {
    // A table made and filled before the seed is set rehashes with the new keys the first time
    // it is set through a `Context`, and keeps its entries.
    let mut lua = Lua::empty();
    let table = lua.enter(|ctx| {
        let table = Table::new(&ctx);
        for i in 1..=100i64 {
            table.set(ctx, i * 1000, i).unwrap();
        }
        ctx.stash(table)
    });
    assert!(lua.set_hash_seed(42));
    lua.enter(|ctx| {
        let table = ctx.fetch(&table);
        table.set(ctx, "key", 1).unwrap();
        for i in 1..=100i64 {
            assert!(matches!(table.get_value(ctx, i * 1000), Value::Integer(v) if v == i));
        }
        assert!(matches!(table.get_value(ctx, "key"), Value::Integer(1)));
    });

    // And scripts work as usual in a seeded instance.
    let mut lua = Lua::empty();
    assert!(lua.set_hash_seed(42));
    lua.load_core().unwrap();
    let executor = lua
        .try_enter(|ctx| {
            let closure = Closure::load(
                ctx,
                None,
                &b"local t = {} for i = 1, 1000 do t['k' .. i] = i t[i * 7] = i end \
                   local n = 0 for k, v in pairs(t) do n = n + v end \
                   return n, t.k500, t[7000], string.rep('x', 3)"[..],
            )?;
            Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
        })
        .unwrap();
    let (n, k, i, s) = lua
        .execute::<(i64, i64, i64, std::string::String)>(&executor)
        .unwrap();
    assert_eq!((n, k, i, s.as_str()), (1001000, 500, 1000, "xxx"));
}

#[test]
fn with_hash_seed_constructor() {
    let mut lua = Lua::with_hash_seed(2);
    lua.load_core().unwrap();
    // The seed took, and no longer changes.
    assert!(!lua.set_hash_seed(3));
    let hash = lua.enter(|ctx| ctx.intern(b"hello").stored_hash());
    assert_eq!(hash, HashKeys::new(2).hash_bytes(b"hello"));
    assert_eq!(hash, hashes(Some(2), &["hello".to_owned()])[0]);
}
