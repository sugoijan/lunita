mod executor;
mod thread;
mod vm;

use thiserror::Error;

use crate::meta_ops::{MetaCallError, MetaOperatorError};

pub use self::{
    executor::{
        BadExecutorMode, CurrentThread, Execution, Executor, ExecutorInner, ExecutorMode,
        UpperLuaFrame,
    },
    thread::{
        BadThreadMode, OpenUpValue, Thread, ThreadInner, ThreadMode, MAX_FRAMES, MAX_FRAMES_51,
        MAX_STACK_SIZE,
    },
};

#[derive(Debug, Clone, Error)]
pub enum VMError {
    #[error("{}", if *.0 {
        "operation expects variable stack"
    } else {
        "unexpected variable stack during operation"
    })]
    ExpectedVariableStack(bool),
    #[error("Bad types for SetList op, expected table, integer, found {0}, {1}")]
    BadSetList(&'static str, &'static str),
    #[error("bad call")]
    BadCall(#[from] MetaCallError),
    #[error("operator error")]
    OperatorError(#[from] MetaOperatorError),
    #[error("_ENV upvalue is only allowed on top-level closure")]
    BadEnvUpValue,
    #[error("Invalid types in for loop; expected numbers, found {0}, {1}, and {2}")]
    BadForLoop(&'static str, &'static str, &'static str),
    #[error("'for' {0} value must be a number, found {1}")]
    BadForLoopPrep(&'static str, &'static str),
    #[error("'for' step is zero")]
    ForStepZero,
    #[error("stack overflow")]
    StackOverflow,
    #[error(transparent)]
    Limit(#[from] crate::LimitError),
    #[error("internal error: the top frame is not a Lua frame")]
    NotLuaFrame,
    #[error("internal error: invalid return frame")]
    BadReturn,
    #[error("internal error: invalid stack index")]
    BadStack,
    #[error("internal error: invalid program counter")]
    BadPc,
    #[error("internal error: invalid constant index")]
    BadConstant,
    #[error("internal error: invalid upvalue index")]
    BadUpValue,
    #[error("internal error: invalid prototype index")]
    BadPrototype,
}
