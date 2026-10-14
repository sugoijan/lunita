use std::fmt;

use gc_arena::{DynamicRoot, DynamicRootSet, Mutation, Rootable};

use crate::{
    Callback, Closure, Error, Executor, Function, RuntimeError, String, Table, Thread, UserData,
    Value,
    callback::CallbackInner,
    closure::ClosureInner,
    string::StringInner,
    table::TableInner,
    thread::{ExecutorInner, ThreadInner},
    userdata::UserDataInner,
};

/// A trait for types that can be stashed into a [`DynamicRootSet`].
///
/// This trait is simpler to work with than having to manually specify `Rootable` projections and
/// can work with more types than just those that wrap a single `Gc` pointer.
///
/// It is implemented for all common `lunita` types, allowing you to stash those types in the
/// registry or inside of async sequences.
pub trait Stashable<'gc> {
    type Stashed;

    fn stash(self, mc: &Mutation<'gc>, roots: DynamicRootSet<'gc>) -> Self::Stashed;
}

/// A trait for types that can be fetched from a [`DynamicRootSet`].
pub trait Fetchable {
    type Fetched<'gc>;

    fn fetch<'gc>(&self, roots: DynamicRootSet<'gc>) -> Self::Fetched<'gc>;
}

/// Defines a stashed handle to a `lunita` type: a newtype over a `DynamicRoot` of the type's
/// inner `Gc` pointer, `Debug` by pointer, and the `Stashable` and `Fetchable` conversions.
macro_rules! stashed_type {
    ($(#[$doc:meta])* $stashed:ident, $type:ident<'gc>, $inner:ty) => {
        $(#[$doc])*
        #[derive(Clone)]
        pub struct $stashed(DynamicRoot<Rootable![$inner]>);

        impl fmt::Debug for $stashed {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($stashed))
                    .field(&self.0.as_ptr())
                    .finish()
            }
        }

        impl<'gc> Stashable<'gc> for $type<'gc> {
            type Stashed = $stashed;

            fn stash(self, mc: &Mutation<'gc>, roots: DynamicRootSet<'gc>) -> Self::Stashed {
                $stashed(roots.stash::<Rootable![$inner]>(mc, self.into_inner()))
            }
        }

        impl Fetchable for $stashed {
            type Fetched<'gc> = $type<'gc>;

            fn fetch<'gc>(&self, roots: DynamicRootSet<'gc>) -> Self::Fetched<'gc> {
                $type::from_inner(roots.fetch(&self.0))
            }
        }
    };
}

stashed_type!(StashedString, String<'gc>, StringInner);

stashed_type!(StashedTable, Table<'gc>, TableInner<'_>);

stashed_type!(StashedClosure, Closure<'gc>, ClosureInner<'_>);

stashed_type!(StashedCallback, Callback<'gc>, CallbackInner<'_>);

stashed_type!(StashedThread, Thread<'gc>, ThreadInner<'_>);

stashed_type!(StashedUserData, UserData<'gc>, UserDataInner<'_>);

stashed_type!(StashedExecutor, Executor<'gc>, ExecutorInner<'_>);

#[derive(Debug, Clone)]
pub enum StashedFunction {
    Closure(StashedClosure),
    Callback(StashedCallback),
}

impl From<StashedClosure> for StashedFunction {
    fn from(closure: StashedClosure) -> Self {
        Self::Closure(closure)
    }
}

impl From<StashedCallback> for StashedFunction {
    fn from(callback: StashedCallback) -> Self {
        Self::Callback(callback)
    }
}

impl<'gc> Stashable<'gc> for Function<'gc> {
    type Stashed = StashedFunction;

    fn stash(self, mc: &Mutation<'gc>, roots: DynamicRootSet<'gc>) -> Self::Stashed {
        match self {
            Function::Closure(closure) => StashedFunction::Closure(closure.stash(mc, roots)),
            Function::Callback(callback) => StashedFunction::Callback(callback.stash(mc, roots)),
        }
    }
}

impl Fetchable for StashedFunction {
    type Fetched<'gc> = Function<'gc>;

    fn fetch<'gc>(&self, roots: DynamicRootSet<'gc>) -> Self::Fetched<'gc> {
        match self {
            StashedFunction::Closure(closure) => Function::Closure(closure.fetch(roots)),
            StashedFunction::Callback(callback) => Function::Callback(callback.fetch(roots)),
        }
    }
}

#[derive(Debug, Clone)]
pub enum StashedValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    String(StashedString),
    Table(StashedTable),
    Function(StashedFunction),
    Thread(StashedThread),
    UserData(StashedUserData),
}

impl StashedValue {
    pub fn to_bool(self) -> bool {
        match self {
            StashedValue::Nil => false,
            StashedValue::Boolean(false) => false,
            _ => true,
        }
    }

    pub fn as_primitive<'gc>(&self) -> Option<Value<'gc>> {
        match self {
            StashedValue::Nil => Some(Value::Nil),
            StashedValue::Boolean(b) => Some(Value::Boolean(*b)),
            StashedValue::Integer(i) => Some(Value::Integer(*i)),
            StashedValue::Number(n) => Some(Value::Number(*n)),
            _ => None,
        }
    }
}

impl From<bool> for StashedValue {
    fn from(v: bool) -> StashedValue {
        StashedValue::Boolean(v)
    }
}

impl From<i64> for StashedValue {
    fn from(v: i64) -> StashedValue {
        StashedValue::Integer(v)
    }
}

impl From<f64> for StashedValue {
    fn from(v: f64) -> StashedValue {
        StashedValue::Number(v)
    }
}

impl From<StashedString> for StashedValue {
    fn from(v: StashedString) -> StashedValue {
        StashedValue::String(v)
    }
}

impl From<StashedTable> for StashedValue {
    fn from(v: StashedTable) -> StashedValue {
        StashedValue::Table(v)
    }
}

impl From<StashedFunction> for StashedValue {
    fn from(v: StashedFunction) -> StashedValue {
        StashedValue::Function(v)
    }
}

impl From<StashedClosure> for StashedValue {
    fn from(v: StashedClosure) -> StashedValue {
        StashedValue::Function(StashedFunction::Closure(v))
    }
}

impl From<StashedCallback> for StashedValue {
    fn from(v: StashedCallback) -> StashedValue {
        StashedValue::Function(StashedFunction::Callback(v))
    }
}

impl From<StashedUserData> for StashedValue {
    fn from(v: StashedUserData) -> StashedValue {
        StashedValue::UserData(v)
    }
}

impl<'gc> Stashable<'gc> for Value<'gc> {
    type Stashed = StashedValue;

    fn stash(self, mc: &Mutation<'gc>, roots: DynamicRootSet<'gc>) -> Self::Stashed {
        match self {
            Value::Nil => StashedValue::Nil,
            Value::Boolean(b) => StashedValue::Boolean(b),
            Value::Integer(i) => StashedValue::Integer(i),
            Value::Number(n) => StashedValue::Number(n),
            Value::String(s) => StashedValue::String(s.stash(mc, roots)),
            Value::Table(t) => StashedValue::Table(t.stash(mc, roots)),
            Value::Function(f) => StashedValue::Function(f.stash(mc, roots)),
            Value::Thread(t) => StashedValue::Thread(t.stash(mc, roots)),
            Value::UserData(u) => StashedValue::UserData(u.stash(mc, roots)),
        }
    }
}

impl Fetchable for StashedValue {
    type Fetched<'gc> = Value<'gc>;

    fn fetch<'gc>(&self, roots: DynamicRootSet<'gc>) -> Self::Fetched<'gc> {
        match self {
            StashedValue::Nil => Value::Nil,
            StashedValue::Boolean(b) => Value::Boolean(*b),
            StashedValue::Integer(i) => Value::Integer(*i),
            StashedValue::Number(n) => Value::Number(*n),
            StashedValue::String(s) => Value::String(s.fetch(roots)),
            StashedValue::Table(t) => Value::Table(t.fetch(roots)),
            StashedValue::Function(f) => Value::Function(f.fetch(roots)),
            StashedValue::Thread(t) => Value::Thread(t.fetch(roots)),
            StashedValue::UserData(u) => Value::UserData(u.fetch(roots)),
        }
    }
}

pub enum StashedError {
    Lua(StashedValue),
    Runtime(RuntimeError),
}

impl From<StashedValue> for StashedError {
    fn from(error: StashedValue) -> Self {
        Self::Lua(error)
    }
}

impl From<RuntimeError> for StashedError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl<E: Into<anyhow::Error>> From<E> for StashedError {
    fn from(error: E) -> Self {
        RuntimeError::from(error).into()
    }
}

impl<'gc> Stashable<'gc> for Error<'gc> {
    type Stashed = StashedError;

    fn stash(self, mc: &Mutation<'gc>, roots: DynamicRootSet<'gc>) -> Self::Stashed {
        match self {
            Error::Lua(err) => StashedError::Lua(err.0.stash(mc, roots)),
            Error::Runtime(err) => StashedError::Runtime(err),
        }
    }
}

impl Fetchable for StashedError {
    type Fetched<'gc> = Error<'gc>;

    fn fetch<'gc>(&self, roots: DynamicRootSet<'gc>) -> Self::Fetched<'gc> {
        match self {
            StashedError::Lua(err) => Error::from_value(err.fetch(roots)),
            StashedError::Runtime(err) => err.clone().into(),
        }
    }
}
