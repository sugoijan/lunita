use std::{error::Error as StdError, fmt, string::String as StdString, sync::Arc};

use gc_arena::{Collect, Rootable};
use thiserror::Error;

use crate::{
    Callback, CallbackReturn, Context, FromValue, IntoValue, LimitError, MetaMethod, Singleton,
    Table, UserData, Value, thread::VMError,
};

#[derive(Debug, Clone, Copy, Error)]
#[error("type error, expected {expected}, found {found}")]
pub struct TypeError {
    pub expected: &'static str,
    pub found: &'static str,
}

/// An error raised directly from Lua which contains a Lua value.
///
/// Any [`Value`] can be raised as an error and it will be contained here.
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct LuaError<'gc>(pub Value<'gc>);

impl<'gc> fmt::Display for LuaError<'gc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl<'gc> From<Value<'gc>> for LuaError<'gc> {
    fn from(error: Value<'gc>) -> Self {
        LuaError(error)
    }
}

impl<'gc> LuaError<'gc> {
    pub fn to_extern(self) -> ExternLuaError {
        self.into()
    }
}

/// A [`LuaError`] that is not bound to the GC context.
///
/// All primitive values (nil, booleans, integers, numbers) are represented here exactly. Strings
/// are converted *lossily* into normal Rust strings. Tables, functions, threads, and userdata are
/// stored in their *raw pointer* form.
#[derive(Debug, Clone, Error)]
pub enum ExternLuaError {
    #[error("nil")]
    Nil,
    #[error("{0}")]
    Boolean(bool),
    #[error("{0}")]
    Integer(i64),
    #[error("{0}")]
    Number(f64),
    #[error("{0}")]
    String(StdString),
    #[error("<table {0:p}>")]
    Table(*const ()),
    #[error("<function {0:p}>")]
    Function(*const ()),
    #[error("<thread {0:p}>")]
    Thread(*const ()),
    #[error("<userdata {0:p}>")]
    UserData(*const ()),
}

impl<'gc> From<LuaError<'gc>> for ExternLuaError {
    fn from(error: LuaError<'gc>) -> Self {
        // Null only for the variants converted in full, which do not use it.
        let ptr = error.0.as_ptr().unwrap_or(std::ptr::null());
        match error.0 {
            Value::Nil => ExternLuaError::Nil,
            Value::Boolean(b) => ExternLuaError::Boolean(b),
            Value::Integer(i) => ExternLuaError::Integer(i),
            Value::Number(n) => ExternLuaError::Number(n),
            Value::String(s) => ExternLuaError::String(s.display_lossy().to_string()),
            Value::Table(_) => ExternLuaError::Table(ptr),
            Value::Function(_) => ExternLuaError::Function(ptr),
            Value::Thread(_) => ExternLuaError::Thread(ptr),
            Value::UserData(_) => ExternLuaError::UserData(ptr),
        }
    }
}

// SAFETY: The pointers in `ExternLuaError` are not actually dereferenced at all, they are purely
// informational.
unsafe impl Send for ExternLuaError {}
unsafe impl Sync for ExternLuaError {}

/// A shareable, dynamically typed wrapper around a normal Rust error.
///
/// Rust errors can be caught and re-raised through Lua which allows for unrestricted sharing, so
/// this type contains its error inside an `Arc` pointer to allow for this.
#[derive(Debug, Clone, Collect)]
#[collect(require_static)]
pub struct RuntimeError(pub Arc<anyhow::Error>);

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<E: Into<anyhow::Error>> From<E> for RuntimeError {
    fn from(err: E) -> Self {
        Self::new(err)
    }
}

impl RuntimeError {
    pub fn new(err: impl Into<anyhow::Error>) -> Self {
        Self(Arc::new(err.into()))
    }

    pub fn root_cause(&self) -> &(dyn StdError + 'static) {
        self.0.root_cause()
    }

    pub fn is<E>(&self) -> bool
    where
        E: fmt::Display + fmt::Debug + Send + Sync + 'static,
    {
        self.0.is::<E>()
    }

    pub fn downcast<E>(&self) -> Option<&E>
    where
        E: fmt::Display + fmt::Debug + Send + Sync + 'static,
    {
        self.0.downcast_ref::<E>()
    }

    /// If this error is a resource limit error, or was caused by one, returns which limit was
    /// exceeded.
    pub fn limit_error(&self) -> Option<LimitError> {
        self.0.chain().find_map(|err| {
            // A `VMError::Limit` is transparent: the limit error is neither it nor its source.
            match err.downcast_ref::<VMError>() {
                Some(VMError::Limit(limit)) => Some(*limit),
                _ => err.downcast_ref::<LimitError>().copied(),
            }
        })
    }
}

impl AsRef<dyn StdError + 'static> for RuntimeError {
    fn as_ref(&self) -> &(dyn StdError + 'static) {
        (*self.0).as_ref()
    }
}

/// An error message from a library function, which the executor turns into a Lua string with the
/// position of the calling Lua code when the function returns it (as `luaL_error` adds it).
#[derive(Debug, Error)]
#[error("{}", StdString::from_utf8_lossy(.0))]
pub(crate) struct LibraryError(pub(crate) Vec<u8>);

/// An error that can be raised from Lua code.
///
/// This can be either a [`LuaError`] containing a Lua [`Value`], or a [`RuntimeError`] containing a
/// Rust error.
#[derive(Debug, Clone, Collect)]
#[collect(no_drop)]
pub enum Error<'gc> {
    Lua(LuaError<'gc>),
    Runtime(RuntimeError),
}

impl<'gc> fmt::Display for Error<'gc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Lua(err) => write!(f, "lua error: {err}"),
            Error::Runtime(err) => write!(f, "runtime error: {err:#}"),
        }
    }
}

impl<'gc> From<Value<'gc>> for Error<'gc> {
    fn from(value: Value<'gc>) -> Self {
        Self::from_value(value)
    }
}

impl<'gc> From<LuaError<'gc>> for Error<'gc> {
    fn from(error: LuaError<'gc>) -> Self {
        Self::Lua(error)
    }
}

impl<'gc> From<RuntimeError> for Error<'gc> {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl<'gc, E: Into<anyhow::Error>> From<E> for Error<'gc> {
    fn from(error: E) -> Self {
        Self::Runtime(RuntimeError::new(error))
    }
}

impl<'gc> Error<'gc> {
    /// Turn a Lua [`Value`] into an `Error`.
    ///
    /// If the provided value is a [`UserData`] object which holds a [`RuntimeError`], then this
    /// conversion will clone the held `RuntimeError` and properly return an [`Error::Runtime`]
    /// variant. This is how Rust errors are properly transported through Lua: a `RuntimeError`
    /// which is turned into a `Value` with [`Error::to_value`] will always turn back into a
    /// `RuntimeError` error with [`Error::from_value`].
    ///
    /// If the given value is *any other* kind of Lua value, then this will return a [`LuaError`]
    /// instead.
    pub fn from_value(value: Value<'gc>) -> Self {
        if let Value::UserData(ud) = value {
            if let Ok(err) = ud.downcast_static::<RuntimeError>() {
                return Error::Runtime(err.clone());
            }
        }

        Error::Lua(value.into())
    }

    /// Convert an `Error` into a Lua value.
    ///
    /// For Lua errors, this simply returns the original Lua [`Value`] directly.
    ///
    /// For Rust errors, this will return a [`UserData`] value which holds a [`RuntimeError`]. The
    /// `UserData` object will also have a `__tostring` metamethod which prints the error properly
    /// when printed from Lua.
    pub fn to_value(&self, ctx: Context<'gc>) -> Value<'gc> {
        match self {
            Error::Lua(err) => err.0,
            Error::Runtime(err) => {
                #[derive(Copy, Clone, Collect)]
                #[collect(no_drop)]
                struct UDMeta<'gc>(Table<'gc>);

                impl<'gc> UDMeta<'gc> {
                    // Refused only past the memory limit; the singleton is kept, so the next
                    // error tries again.
                    fn install(&self, ctx: Context<'gc>) {
                        if !self.0.get_value(ctx, MetaMethod::ToString).is_nil() {
                            return;
                        }
                        let _ = self.0.set_field(
                            ctx,
                            MetaMethod::ToString.name(),
                            Callback::from_fn(&ctx, |ctx, _, mut stack| {
                                let ud = stack.consume::<UserData>(ctx)?;
                                let error = ud.downcast_static::<RuntimeError>()?;
                                // With its causes, as `Error` displays it.
                                stack.replace(ctx, format!("{error:#}"));
                                Ok(CallbackReturn::Return)
                            }),
                        );
                    }
                }

                impl<'gc> Singleton<'gc> for UDMeta<'gc> {
                    fn create(ctx: Context<'gc>) -> Self {
                        let meta = Self(Table::new(&ctx));
                        meta.install(ctx);
                        meta
                    }
                }

                let meta = ctx.singleton::<Rootable![UDMeta<'_>]>();
                meta.install(ctx);
                let ud = UserData::new_static(&ctx, err.clone());
                ud.set_metatable(&ctx, Some(meta.0));
                ud.into()
            }
        }
    }

    pub fn to_extern(&self) -> ExternError {
        self.clone().into_extern()
    }

    /// If this error is a resource limit error, returns which limit was exceeded.
    pub fn limit_error(&self) -> Option<LimitError> {
        match self {
            Error::Lua(_) => None,
            Error::Runtime(err) => err.limit_error(),
        }
    }

    pub fn into_extern(self) -> ExternError {
        self.into()
    }
}

impl<'gc> IntoValue<'gc> for Error<'gc> {
    fn into_value(self, ctx: Context<'gc>) -> Value<'gc> {
        self.to_value(ctx)
    }
}

impl<'gc> FromValue<'gc> for Error<'gc> {
    fn from_value(_: Context<'gc>, value: Value<'gc>) -> Result<Self, TypeError> {
        Ok(Error::from_value(value))
    }
}

/// An [`enum@Error`] that is not bound to the GC context.
#[derive(Debug, Clone)]
pub enum ExternError {
    Lua(ExternLuaError),
    Runtime(RuntimeError),
}

impl fmt::Display for ExternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExternError::Lua(err) => write!(f, "lua error: {err}"),
            ExternError::Runtime(err) => write!(f, "runtime error: {err:#}"),
        }
    }
}

impl StdError for ExternError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            ExternError::Lua(err) => Some(err),
            ExternError::Runtime(err) => Some(err.as_ref()),
        }
    }
}

impl ExternError {
    /// If this error is a resource limit error, returns which limit was exceeded.
    pub fn limit_error(&self) -> Option<LimitError> {
        match self {
            ExternError::Lua(_) => None,
            ExternError::Runtime(err) => err.limit_error(),
        }
    }

    pub fn root_cause(&self) -> &(dyn StdError + 'static) {
        match self {
            ExternError::Lua(err) => err,
            ExternError::Runtime(err) => err.root_cause(),
        }
    }
}

impl From<ExternLuaError> for ExternError {
    fn from(error: ExternLuaError) -> Self {
        Self::Lua(error)
    }
}

impl From<RuntimeError> for ExternError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl<'gc> From<Error<'gc>> for ExternError {
    fn from(err: Error<'gc>) -> Self {
        match err {
            Error::Lua(err) => err.to_extern().into(),
            Error::Runtime(e) => e.into(),
        }
    }
}
