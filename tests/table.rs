use std::cmp::Ordering;

use piccolo::{table::NextValue, Lua, Table, Value};

#[test]
fn test_table_iter() {
    let mut lua = Lua::core();

    lua.enter(|ctx| {
        let table = Table::new(&ctx);

        table.set(ctx, 1, "1").unwrap();
        table.set(ctx, 2, "2").unwrap();
        table.set(ctx, 3, "3").unwrap();
        table.set(ctx, "1", 1).unwrap();
        table.set(ctx, "2", 2).unwrap();
        table.set(ctx, "3", 3).unwrap();

        let mut pairs = table.iter().collect::<Vec<_>>();
        pairs.sort_by(|&(ak, _), &(bk, _)| match (ak, bk) {
            (piccolo::Value::Integer(a), piccolo::Value::Integer(b)) => a.cmp(&b),
            (piccolo::Value::Integer(_), piccolo::Value::String(_)) => Ordering::Less,
            (piccolo::Value::String(_), piccolo::Value::Integer(_)) => Ordering::Greater,
            (piccolo::Value::String(a), piccolo::Value::String(b)) => a.cmp(&b),
            _ => unreachable!(),
        });

        assert_eq!(pairs.len(), 6);
        assert!(matches!(pairs[0], (Value::Integer(1), Value::String(s)) if s == "1" ));
        assert!(matches!(pairs[1], (Value::Integer(2), Value::String(s)) if s == "2" ));
        assert!(matches!(pairs[2], (Value::Integer(3), Value::String(s)) if s == "3" ));
        assert!(matches!(pairs[3], (Value::String(s), Value::Integer(1)) if s == "1" ));
        assert!(matches!(pairs[4], (Value::String(s), Value::Integer(2)) if s == "2" ));
        assert!(matches!(pairs[5], (Value::String(s), Value::Integer(3)) if s == "3" ));

        for (k, _) in table.iter() {
            table.set(ctx, k, Value::Nil).unwrap();
        }

        assert!(table.get_value(ctx, 1).is_nil());
        assert!(table.get_value(ctx, 2).is_nil());
        assert!(table.get_value(ctx, 3).is_nil());
        assert!(table.get_value(ctx, "1").is_nil());
        assert!(table.get_value(ctx, "2").is_nil());
        assert!(table.get_value(ctx, "3").is_nil());
    });
}

#[test]
fn test_table_insertion_order() {
    let mut lua = Lua::core();

    lua.enter(|ctx| {
        let table = Table::new(&ctx);
        let keys = ["z", "a", "m", "b", "y", "c"];
        for (i, k) in keys.iter().enumerate() {
            table.set(ctx, *k, i as i64).unwrap();
        }
        // Removing and re-adding a key keeps its position until the table is compacted.
        table.set(ctx, "m", Value::Nil).unwrap();
        table.set(ctx, "m", 2).unwrap();

        let order = table
            .iter()
            .map(|(k, _)| match k {
                Value::String(s) => s.to_str().unwrap().to_owned(),
                _ => panic!("unexpected key"),
            })
            .collect::<Vec<_>>();
        assert_eq!(order, keys);
    });
}

#[test]
fn next_finds_keys_removed_before_a_rekey() {
    let mut lua = Lua::core();

    lua.enter(|ctx| {
        // Made without a `Context`, the table takes the instance's hash keys the first time it is
        // set through one, after an entry was removed during iteration.
        let table = Table::new(&ctx);
        let keys = [Table::new(&ctx), Table::new(&ctx), Table::new(&ctx)];
        for (i, &k) in keys.iter().enumerate() {
            table
                .set_raw(&ctx, k.into(), Value::Integer(i as i64))
                .unwrap();
        }

        let NextValue::Found { key: first, .. } = table.next(Value::Nil) else {
            panic!("no first entry");
        };
        table.set_raw(&ctx, first, Value::Nil).unwrap();
        table.set(ctx, keys[1], 10).unwrap();

        let NextValue::Found { key, value } = table.next(first) else {
            panic!("the removed key was not found");
        };
        assert!(matches!((key, value), (Value::Table(k), Value::Integer(10)) if k == keys[1]));
        assert!(matches!(
            table.next(Value::Table(Table::new(&ctx))),
            NextValue::NotFound
        ));

        // Setting the removed key again appends it, the others are still found.
        table.set(ctx, first, 20).unwrap();
        assert!(matches!(table.get_value(ctx, first), Value::Integer(20)));
        assert!(matches!(table.get_value(ctx, keys[2]), Value::Integer(2)));
        assert_eq!(table.iter().count(), 3);
    });
}

#[test]
fn rekey_allocates_nothing() {
    let mut lua = Lua::core();

    lua.enter(|ctx| {
        // A table made without a `Context` takes the instance's hash keys the first time it is
        // set through one, rebuilding its index: the index already has room for every entry.
        let table = Table::new(&ctx);
        for i in 0..100_000 {
            let key = ctx.intern(format!("key{i}").as_bytes());
            table.set_raw(&ctx, key.into(), Value::Integer(i)).unwrap();
        }
        let before = ctx.metrics().total_allocation();
        table.set(ctx, "x", 1).unwrap();
        let after = ctx.metrics().total_allocation();
        assert!(
            after <= before + 64,
            "rekey allocated {} bytes",
            after - before
        );
        assert!(matches!(
            table.get_value(ctx, "key99999"),
            Value::Integer(99_999)
        ));
    });
}
