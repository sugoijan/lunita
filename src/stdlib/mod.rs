mod args;
mod base;
mod coroutine;
mod io;
mod math;
mod string;
mod table;

pub use self::{
    base::load_base,
    coroutine::load_coroutine,
    io::load_io,
    math::{RandomState, load_math},
    string::load_string,
    table::load_table,
};

use crate::{BoxSequence, CallbackReturn, Context, Error, Sequence, SequencePoll};

// The return of a callback that started `sequence` and got `poll` from its first run: the sequence
// continues unless it returned. A call it makes takes the whole stack as its arguments, so the
// sequence must have made it with nothing below them.
fn continue_sequence<'gc>(
    ctx: Context<'gc>,
    poll: SequencePoll<'gc>,
    sequence: impl Sequence<'gc> + 'gc,
) -> Result<CallbackReturn<'gc>, Error<'gc>> {
    Ok(match poll {
        SequencePoll::Return => CallbackReturn::Return,
        SequencePoll::Call {
            function,
            bottom: 0,
        } => CallbackReturn::Call {
            function,
            then: Some(BoxSequence::new(&ctx, sequence)),
        },
        SequencePoll::Call { .. } => {
            return Err(anyhow::anyhow!("sequence started with values below its call").into());
        }
        _ => CallbackReturn::Sequence(BoxSequence::new(&ctx, sequence)),
    })
}
