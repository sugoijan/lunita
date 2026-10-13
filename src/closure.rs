use std::hash::{Hash, Hasher};

use gc_arena::{barrier::unlock, lock::Lock, Collect, Gc, Mutation};
use thiserror::Error;

use crate::{
    compiler::{self, CompiledPrototype, FunctionRef, LineNumber},
    compiler::{lexer::LexError, BudgetError, CompileBudget, CompileLimits},
    constant::is_exact_float_integer,
    hash::HashKeys,
    memory::{Traced, TrackedVec, Written},
    opcode::OpCode,
    thread::OpenUpValue,
    types::UpValueDescriptor,
    Compat, Constant, Context, LimitError, String, Table, Value,
};

// Note: These errors must not have #[error(transparent)] so that
// anyhow::Error::root_cause and downcasting work as expected by the
// interpreter. (Even though that gives slightly cleaner error messages).
#[derive(Debug, Error)]
pub enum CompilerError {
    #[error("parse error")]
    Parsing(#[from] compiler::ParseError),
    #[error("compile error")]
    Compilation(#[from] compiler::CompileError),
    /// Compiling the chunk would exceed the memory or string length limit of the Lua instance.
    #[error("limit error")]
    Limit(#[from] LimitError),
}

/// A compiled Lua function.
///
/// In Lua jargon, a "prototype" is only executable code, it has none of its "upvalues" set and
/// cannot be called directly.
///
/// If a prototype has only an single (optional) `_ENV` upvalue, then it can be turned into an
/// executable `Closure` by binding it with its environment with [`Closure::new`].
#[derive(Debug, Collect)]
#[collect(no_drop)]
pub struct FunctionPrototype<'gc> {
    pub chunk_name: String<'gc>,
    pub reference: FunctionRef<String<'gc>>,
    pub fixed_params: u8,
    pub has_varargs: bool,
    pub stack_size: u16,
    pub constants: TrackedVec<Constant<String<'gc>>>,
    pub opcodes: TrackedVec<OpCode>,
    pub opcode_line_numbers: TrackedVec<(usize, LineNumber)>,
    pub upvalues: TrackedVec<UpValueDescriptor>,
    pub prototypes: TrackedVec<Gc<'gc, FunctionPrototype<'gc>>>,
    queued: Traced<FunctionPrototype<'static>>,
}

impl<'gc> FunctionPrototype<'gc> {
    pub fn from_compiled(
        mc: &Mutation<'gc>,
        chunk_name: String<'gc>,
        compiled_function: &CompiledPrototype<String<'gc>>,
    ) -> Self {
        Self::from_compiled_map_strings(mc, chunk_name, compiled_function, |s| *s)
    }

    pub fn from_compiled_map_strings<S>(
        mc: &Mutation<'gc>,
        chunk_name: String<'gc>,
        compiled_function: &CompiledPrototype<S>,
        map_string: impl Fn(&S) -> String<'gc>,
    ) -> Self {
        fn new<'gc, S>(
            mc: &Mutation<'gc>,
            chunk_name: String<'gc>,
            compiled_function: &CompiledPrototype<S>,
            map_string: impl Fn(&S) -> String<'gc> + Copy,
        ) -> FunctionPrototype<'gc> {
            let constants = TrackedVec::from_iter_exact(
                compiled_function
                    .constants
                    .iter()
                    .map(|c| c.as_string_ref().map_string(map_string)),
            );
            let opcodes = TrackedVec::from_iter_exact(compiled_function.opcodes.iter().copied());
            let opcode_line_numbers =
                TrackedVec::from_iter_exact(compiled_function.opcode_line_numbers.iter().copied());
            let upvalues = TrackedVec::from_iter_exact(compiled_function.upvalues.iter().copied());
            let prototypes = TrackedVec::from_iter_exact(
                compiled_function
                    .prototypes
                    .iter()
                    .map(|cf| Gc::new(mc, new(mc, chunk_name, cf, map_string))),
            );

            FunctionPrototype {
                chunk_name,
                reference: compiled_function
                    .reference
                    .as_string_ref()
                    .map_strings(map_string),
                fixed_params: compiled_function.fixed_params,
                has_varargs: compiled_function.has_varargs,
                stack_size: compiled_function.stack_size,
                constants,
                opcodes,
                opcode_line_numbers,
                upvalues,
                prototypes,
                queued: Traced::new(),
            }
        }

        new(mc, chunk_name, compiled_function, &map_string)
    }

    /// Compile a chunk, within the memory and string length limits of the Lua instance (see
    /// [`crate::limits`]).
    pub fn compile(
        ctx: Context<'gc>,
        source_name: &str,
        source: &[u8],
    ) -> Result<FunctionPrototype<'gc>, CompilerError> {
        let budget = CompileBudget::new(CompileLimits::for_context(ctx, None));
        Self::compile_with_budget(ctx, source_name, source, &budget)
    }

    /// Compile a chunk, charging the resources used to `budget`.
    ///
    /// Exceeding the memory or string length limits of the budget is a [`CompilerError::Limit`]
    /// (and marks the limit as exceeded, see [`Context::exceed_limit`]).
    pub fn compile_with_budget(
        ctx: Context<'gc>,
        source_name: &str,
        source: &[u8],
        budget: &CompileBudget,
    ) -> Result<FunctionPrototype<'gc>, CompilerError> {
        #[derive(Copy, Clone)]
        struct Interner<'gc>(Context<'gc>);

        impl<'gc> compiler::StringInterner for Interner<'gc> {
            type String = String<'gc>;

            fn intern(&mut self, s: &[u8]) -> Self::String {
                self.0.intern(s)
            }

            fn hash_keys(&self) -> HashKeys {
                self.0.interned_strings().table_hash_keys()
            }
        }

        let interner = Interner(ctx);

        let chunk =
            compiler::parse_chunk_with_budget(source, interner, budget).map_err(|err| match err
                .kind
            {
                compiler::ParseErrorKind::Budget(BudgetError::Memory) => {
                    CompilerError::Limit(ctx.exceed_limit(LimitError::Memory))
                }
                compiler::ParseErrorKind::LexError(LexError::StringTooLong) => {
                    CompilerError::Limit(ctx.exceed_limit(LimitError::StringLength))
                }
                _ => CompilerError::Parsing(err),
            })?;
        let compiled_function = compiler::compile_chunk_with_budget(&chunk, interner, budget)
            .map_err(|err| match err.kind {
                compiler::CompileErrorKind::Budget(BudgetError::Memory) => {
                    CompilerError::Limit(ctx.exceed_limit(LimitError::Memory))
                }
                _ => CompilerError::Compilation(err),
            })?;
        drop(chunk);
        let compiled_function = match ctx.compat() {
            Compat::Lua51 => lua51_constants(compiled_function),
            Compat::Default => compiled_function,
        };

        Ok(FunctionPrototype::from_compiled(
            &ctx,
            ctx.intern(source_name.as_bytes()),
            &compiled_function,
        ))
    }
}

// Lua 5.1 only has floats: integer constants past 2^53 are the floats they round to (the constant
// folder leaves operations on them to run time).
fn lua51_constants<S>(mut proto: CompiledPrototype<S>) -> CompiledPrototype<S> {
    fn convert<S>(proto: &mut CompiledPrototype<S>) {
        for c in &mut proto.constants {
            if let Constant::Integer(i) = *c {
                if !is_exact_float_integer(i) {
                    *c = Constant::Number(i as f64);
                }
            }
        }
        for p in &mut proto.prototypes {
            convert(p);
        }
    }
    convert(&mut proto);
    proto
}

#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub enum UpValueState<'gc> {
    Open(OpenUpValue<'gc>),
    Closed(Value<'gc>),
}

/// The state of an upvalue, with the field that counts it for the memory limit's reserve.
#[derive(Debug, Collect)]
#[collect(no_drop)]
pub struct UpValueInner<'gc> {
    state: Lock<UpValueState<'gc>>,
    queued: Written<UpValueInner<'static>>,
}

#[derive(Debug, Collect, Copy, Clone)]
#[collect(no_drop)]
pub struct UpValue<'gc>(Gc<'gc, UpValueInner<'gc>>);

impl<'gc> UpValue<'gc> {
    pub fn new(mc: &Mutation<'gc>, state: UpValueState<'gc>) -> Self {
        Self(Gc::new(
            mc,
            UpValueInner {
                state: Lock::new(state),
                queued: Written::new(),
            },
        ))
    }

    pub fn from_inner(inner: Gc<'gc, UpValueInner<'gc>>) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> Gc<'gc, UpValueInner<'gc>> {
        self.0
    }

    pub fn get(self) -> UpValueState<'gc> {
        self.0.state.get()
    }

    pub fn set(self, mc: &Mutation<'gc>, state: UpValueState<'gc>) {
        unlock!(Gc::write(mc, self.0), UpValueInner, state).set(state);
    }
}

#[derive(Debug, Copy, Clone, Error)]
pub enum ClosureError {
    #[error("cannot use prototype with upvalues other than _ENV to create top-level closure")]
    HasUpValues,
    #[error("closure requires _ENV upvalue but no environment was provided")]
    RequiresEnv,
}

#[derive(Debug, Collect)]
#[collect(no_drop)]
pub struct ClosureInner<'gc> {
    proto: Gc<'gc, FunctionPrototype<'gc>>,
    upvalues: TrackedVec<UpValue<'gc>>,
    queued: Traced<ClosureInner<'static>>,
}

/// A garbage collected pointer to an executable Lua function.
///
/// A `Closure` represents a [`FunctionPrototype`] bound to an environment. A closure "closes over"
/// free variables that it references, and as such, calling a `Closure` may reference (and mutate!)
/// these closed over variables. In Lua jargon, these references that closures "close over" are
/// called "upvalues".
#[derive(Debug, Copy, Clone, Collect)]
#[collect(no_drop)]
pub struct Closure<'gc>(Gc<'gc, ClosureInner<'gc>>);

impl<'gc> PartialEq for Closure<'gc> {
    fn eq(&self, other: &Closure<'gc>) -> bool {
        Gc::ptr_eq(self.0, other.0)
    }
}

impl<'gc> Eq for Closure<'gc> {}

impl<'gc> Hash for Closure<'gc> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Gc::as_ptr(self.0).hash(state)
    }
}

impl<'gc> Closure<'gc> {
    /// Create a top-level closure, prototype must not have any upvalues besides _ENV.
    pub fn new(
        mc: &Mutation<'gc>,
        proto: FunctionPrototype<'gc>,
        environment: Option<Table<'gc>>,
    ) -> Result<Closure<'gc>, ClosureError> {
        let proto = Gc::new(mc, proto);
        let mut upvalues = TrackedVec::new();

        if !proto.upvalues.is_empty() {
            if *proto.upvalues != [UpValueDescriptor::Environment] {
                return Err(ClosureError::HasUpValues);
            } else if let Some(environment) = environment {
                upvalues.push(UpValue::new(
                    mc,
                    UpValueState::Closed(Value::Table(environment)),
                ));
            } else {
                return Err(ClosureError::RequiresEnv);
            }
        }

        Ok(Closure(Gc::new(
            mc,
            ClosureInner {
                proto,
                upvalues,
                queued: Traced::new(),
            },
        )))
    }

    pub fn from_parts(
        mc: &Mutation<'gc>,
        proto: Gc<'gc, FunctionPrototype<'gc>>,
        upvalues: TrackedVec<UpValue<'gc>>,
    ) -> Self {
        Self(Gc::new(
            mc,
            ClosureInner {
                proto,
                upvalues,
                queued: Traced::new(),
            },
        ))
    }

    pub fn from_inner(inner: Gc<'gc, ClosureInner<'gc>>) -> Self {
        Self(inner)
    }

    pub fn into_inner(self) -> Gc<'gc, ClosureInner<'gc>> {
        self.0
    }

    /// Compile a top-level closure from source, using the globals table as the `_ENV` table.
    pub fn load(
        ctx: Context<'gc>,
        name: Option<&str>,
        source: &[u8],
    ) -> Result<Closure<'gc>, CompilerError> {
        Self::load_with_env(ctx, name, source, ctx.globals())
    }

    /// Compile a top-level closure from source, using the given table as the `_ENV` table.
    pub fn load_with_env(
        ctx: Context<'gc>,
        name: Option<&str>,
        source: &[u8],
        env: Table<'gc>,
    ) -> Result<Closure<'gc>, CompilerError> {
        let proto = FunctionPrototype::compile(ctx, name.unwrap_or("<anonymous>"), source)?;
        #[expect(
            clippy::unwrap_used,
            reason = "a compiled chunk has no upvalues other than _ENV, and _ENV is provided"
        )]
        let closure = Closure::new(&ctx, proto, Some(env)).unwrap();
        Ok(closure)
    }

    pub fn prototype(self) -> Gc<'gc, FunctionPrototype<'gc>> {
        self.0.proto
    }

    pub fn upvalues(self) -> &'gc [UpValue<'gc>] {
        &Gc::as_ref(self.0).upvalues
    }
}
