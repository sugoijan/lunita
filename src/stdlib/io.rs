use std::{
    io::{self, Write},
    pin::Pin,
};

use gc_arena::Collect;

use crate::{
    BoxSequence, Callback, CallbackReturn, Context, Error, Execution, LimitError, Sequence,
    SequencePoll, Stack, Value,
    fuel::{cost, fuel_for},
    meta_ops::{self, MetaResult},
};

pub fn load_io<'gc>(ctx: Context<'gc>) -> Result<(), LimitError> {
    ctx.set_global(
        "print",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            #[derive(Collect)]
            #[collect(require_static)]
            struct PrintSeq {
                first: bool,
            }

            impl<'gc> Sequence<'gc> for PrintSeq {
                fn poll(
                    mut self: Pin<&mut Self>,
                    ctx: Context<'gc>,
                    mut exec: Execution<'gc, '_>,
                    mut stack: Stack<'gc, '_>,
                ) -> Result<SequencePoll<'gc>, Error<'gc>> {
                    let mut stdout = io::stdout();

                    while let Some(value) = stack.pop_back() {
                        match meta_ops::tostring(ctx, value)? {
                            MetaResult::Value(v) => {
                                if self.first {
                                    self.first = false;
                                } else {
                                    stdout.write_all(b"\t")?;
                                }
                                if let Value::String(s) = v {
                                    exec.fuel().consume(fuel_for(
                                        s.as_bytes().len(),
                                        cost::BYTES_PROCESSED,
                                    ));
                                    stdout.write_all(s.as_bytes())?;
                                } else {
                                    // DELIBERATE(puc): `print` takes any `__tostring` result (PUC requires a string; see COMPATIBILITY.md).
                                    write!(stdout, "{}", v.display())?;
                                }
                            }
                            MetaResult::Call(call) => {
                                let bottom = stack.len();
                                stack.extend(call.args);
                                return Ok(SequencePoll::Call {
                                    function: call.function,
                                    bottom,
                                });
                            }
                        }
                    }

                    stdout.write_all(b"\n")?;
                    stdout.flush()?;
                    Ok(SequencePoll::Return)
                }
            }

            stack.as_mut_slice().reverse();

            Ok(CallbackReturn::Sequence(BoxSequence::new(
                &ctx,
                PrintSeq { first: true },
            )))
        }),
    )?;
    Ok(())
}
