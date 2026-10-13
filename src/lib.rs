// The library must not panic on any input: these are denied outside of tests, and the few sites
// that cannot fire are marked with `#[expect]` and a reason.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
    )
)]

pub mod any;
pub mod async_callback;
pub mod callback;
pub mod closure;
pub mod compat;
pub mod compiler;
pub mod constant;
pub mod conversion;
pub mod error;
pub mod finalizers;
pub mod fuel;
pub mod function;
pub mod hash;
pub mod io;
pub mod limits;
pub mod lua;
pub mod memory;
pub mod meta_ops;
pub mod opcode;
pub mod registry;
pub mod stack;
pub mod stash;
pub mod stdlib;
pub mod string;
pub mod table;
pub mod thread;
pub mod types;
pub mod userdata;
pub mod value;

pub use self::{
    async_callback::{SequenceReturn, async_sequence},
    callback::{BoxSequence, Callback, CallbackFn, CallbackReturn, Sequence, SequencePoll},
    closure::{Closure, CompilerError, FunctionPrototype},
    compat::Compat,
    constant::Constant,
    conversion::{FromMultiValue, FromValue, IntoMultiValue, IntoValue, Variadic},
    error::{Error, ExternError, RuntimeError, TypeError},
    fuel::Fuel,
    function::Function,
    limits::{LimitError, Limits},
    lua::{Context, Lua},
    meta_ops::MetaMethod,
    registry::{Registry, Singleton},
    stack::Stack,
    stash::{
        StashedCallback, StashedClosure, StashedError, StashedExecutor, StashedFunction,
        StashedString, StashedTable, StashedThread, StashedUserData, StashedValue,
    },
    string::String,
    table::Table,
    thread::{Execution, Executor, ExecutorMode, Thread, ThreadMode},
    userdata::UserData,
    value::Value,
};
