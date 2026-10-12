//! A singleton may ask the registry for another one while it is created.

use gc_arena::{Collect, Rootable};
use piccolo::{Context, Lua, Singleton};

#[derive(Collect)]
#[collect(require_static)]
struct Inner(u32);

impl<'gc> Singleton<'gc> for Inner {
    fn create(_: Context<'gc>) -> Self {
        Inner(7)
    }
}

#[derive(Collect)]
#[collect(require_static)]
struct Outer(u32);

impl<'gc> Singleton<'gc> for Outer {
    fn create(ctx: Context<'gc>) -> Self {
        Outer(ctx.singleton::<Rootable![Inner]>().0 + 1)
    }
}

#[test]
fn singletons_may_create_singletons() {
    let mut lua = Lua::core();
    lua.enter(|ctx| {
        assert_eq!(ctx.singleton::<Rootable![Outer]>().0, 8);
        assert_eq!(ctx.singleton::<Rootable![Inner]>().0, 7);
        assert_eq!(ctx.singleton::<Rootable![Outer]>().0, 8);
    });
}
