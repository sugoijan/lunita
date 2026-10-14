use std::{cell::Cell, f64::consts::PI};

use gc_arena::{Collect, Mutation, Rootable};

use super::args::{
    arg_error, check_any, check_integer, check_number, error, is_lua51, opt_integer,
};
use crate::{
    Callback, CallbackReturn, Context, Error, LimitError, SequenceReturn, Stack, Table, Value,
    async_sequence, constant::f64_to_i64, meta_ops,
};

pub fn load_math<'gc>(ctx: Context<'gc>) -> Result<(), LimitError> {
    let math = Table::new(&ctx);

    load_baseline(ctx, math)?;
    load_cmp(ctx, math)?;

    load_random(ctx, math)?;

    load_trig(ctx, math)?;
    load_float(ctx, math)?;

    ctx.set_global("math", math)?;
    Ok(())
}

/// A number argument, an integer if it is one (`lua_isinteger`, so not for strings) and a float
/// otherwise.
fn check_numeric<'gc>(
    ctx: Context<'gc>,
    stack: &Stack<'gc, '_>,
    name: &str,
    i: usize,
) -> Result<Value<'gc>, Error<'gc>> {
    match stack.get(i) {
        Value::Integer(i) => Ok(Value::Integer(i)),
        _ => check_number(ctx, stack, name, i).map(Value::Number),
    }
}

/// A function of one number.
fn unary<'gc>(mc: &Mutation<'gc>, name: &'static str, f: fn(f64) -> f64) -> Callback<'gc> {
    Callback::from_fn(mc, move |ctx, _, mut stack| {
        let x = check_number(ctx, &stack, name, 0)?;
        stack.replace(ctx, f(x));
        Ok(CallbackReturn::Return)
    })
}

// `math.ceil` or `math.floor`, rounding with `f`.
fn rounding<'gc>(mc: &Mutation<'gc>, name: &'static str, f: fn(f64) -> f64) -> Callback<'gc> {
    Callback::from_fn(mc, move |ctx, _, mut stack| {
        let v = match check_numeric(ctx, &stack, name, 0)? {
            Value::Integer(i) => Value::Integer(i),
            v => to_int(ctx, f(v.to_number().unwrap_or_default())),
        };
        stack.replace(ctx, v);
        Ok(CallbackReturn::Return)
    })
}

/// A float that is an integer as an integer (Lua 5.4's `pushnumint`), in Lua 5.1 always a float.
fn to_int(ctx: Context<'_>, v: f64) -> Value<'_> {
    match f64_to_i64(v) {
        Some(i) if !is_lua51(ctx) => Value::Integer(i),
        _ => Value::Number(v),
    }
}

pub fn load_baseline<'gc>(ctx: Context<'gc>, math: Table<'gc>) -> Result<(), LimitError> {
    math.set_field(
        ctx,
        "abs",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let abs = match check_numeric(ctx, &stack, "abs", 0)? {
                // `math.abs(math.mininteger)` wraps, as in PUC-Rio Lua.
                Value::Integer(i) => Value::Integer(i.wrapping_abs()),
                v => Value::Number(v.to_number().unwrap_or_default().abs()),
            };
            stack.replace(ctx, abs);
            Ok(CallbackReturn::Return)
        }),
    )?;

    math.set_field(ctx, "huge", Value::Number(f64::INFINITY))?;
    // DELIBERATE(puc): in Lua51 mode too, where 5.1 has neither (see COMPATIBILITY.md).
    math.set_field(ctx, "maxinteger", Value::Integer(i64::MAX))?;
    math.set_field(ctx, "mininteger", Value::Integer(i64::MIN))?;
    math.set_field(ctx, "pi", Value::Number(PI))?;

    math.set_field(
        ctx,
        "tointeger",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let v = check_any(&stack, "tointeger", 0)?;
            stack.replace(ctx, v.to_integer().map_or(Value::Nil, Value::Integer));
            Ok(CallbackReturn::Return)
        }),
    )?;

    math.set_field(
        ctx,
        "type",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let t = match check_any(&stack, "type", 0)? {
                Value::Integer(_) => Some("integer"),
                Value::Number(_) => Some("float"),
                _ => None,
            };
            stack.replace(ctx, t);
            Ok(CallbackReturn::Return)
        }),
    )?;
    Ok(())
}

// `math.min` and `math.max` of Lua 5.1, which take numbers only.
fn min_max_51<'gc>(
    ctx: Context<'gc>,
    mut stack: Stack<'gc, '_>,
    name: &str,
    max: bool,
) -> Result<CallbackReturn<'gc>, Error<'gc>> {
    let mut result = check_number(ctx, &stack, name, 0)?;
    for i in 1..stack.len() {
        let n = check_number(ctx, &stack, name, i)?;
        if (max && n > result) || (!max && n < result) {
            result = n;
        }
    }
    stack.replace(ctx, result);
    Ok(CallbackReturn::Return)
}

// `math.min` and `math.max` of the default mode, which compare any values with metamethods, as
// 5.4's `lua_compare` does: the candidate replaces the best so far when `best < candidate` (`max`)
// or `candidate < best` (`min`).
fn min_max_54<'gc>(
    ctx: Context<'gc>,
    stack: Stack<'gc, '_>,
    name: &str,
    max: bool,
) -> Result<CallbackReturn<'gc>, Error<'gc>> {
    check_any(&stack, name, 0)?;
    let s = async_sequence(&ctx, |locals, mut seq| {
        let mut best = locals.stash(&ctx, stack.get(0));
        let args = stack.len();
        async move {
            for i in 1..args {
                let (call, bottom) = seq.try_enter(|ctx, locals, _, mut stack| {
                    let bottom = args;
                    let (best, candidate) = (locals.fetch(&best), stack.get(i));
                    let (lhs, rhs) = if max {
                        (best, candidate)
                    } else {
                        (candidate, best)
                    };
                    match meta_ops::less_than(ctx, lhs, rhs)? {
                        meta_ops::MetaResult::Value(v) => {
                            stack.resize(bottom);
                            stack.push_back(v);
                            Ok((None, bottom))
                        }
                        meta_ops::MetaResult::Call(meta_ops::MetaCall { function, args }) => {
                            stack.resize(bottom);
                            stack.extend(args);
                            Ok((Some(locals.stash(&ctx, function)), bottom))
                        }
                    }
                })?;
                if let Some(func) = call {
                    seq.call(&func, bottom).await?;
                }
                seq.enter(|ctx, locals, _, stack| {
                    if stack.get(bottom).to_bool() {
                        best = locals.stash(&ctx, stack.get(i))
                    }
                });
            }
            seq.enter(|ctx, locals, _, mut stack| {
                stack.replace(ctx, locals.fetch(&best));
            });
            Ok(SequenceReturn::Return)
        }
    });
    Ok(CallbackReturn::Sequence(s))
}

// `math.min` (`max == false`) and `math.max` (`max == true`) of the running mode.
fn min_max<'gc>(
    ctx: Context<'gc>,
    stack: Stack<'gc, '_>,
    name: &str,
    max: bool,
) -> Result<CallbackReturn<'gc>, Error<'gc>> {
    if is_lua51(ctx) {
        min_max_51(ctx, stack, name, max)
    } else {
        min_max_54(ctx, stack, name, max)
    }
}

pub fn load_cmp<'gc>(ctx: Context<'gc>, math: Table<'gc>) -> Result<(), LimitError> {
    math.set_field(
        ctx,
        "ult",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let a = check_integer(ctx, &stack, "ult", 0)?;
            let b = check_integer(ctx, &stack, "ult", 1)?;
            stack.replace(ctx, (a as u64) < (b as u64));
            Ok(CallbackReturn::Return)
        }),
    )?;

    math.set_field(
        ctx,
        "max",
        Callback::from_fn(&ctx, |ctx, _, stack| min_max(ctx, stack, "max", true)),
    )?;
    math.set_field(
        ctx,
        "min",
        Callback::from_fn(&ctx, |ctx, _, stack| min_max(ctx, stack, "min", false)),
    )?;
    Ok(())
}

pub fn load_float<'gc>(ctx: Context<'gc>, math: Table<'gc>) -> Result<(), LimitError> {
    math.set_field(ctx, "ceil", rounding(&ctx, "ceil", f64::ceil))?;
    math.set_field(ctx, "floor", rounding(&ctx, "floor", f64::floor))?;

    math.set_field(
        ctx,
        "deg",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let x = check_number(ctx, &stack, "deg", 0)?;
            // Lua 5.1 divides by the radians per degree, Lua 5.4 multiplies by the inverse.
            let deg = if is_lua51(ctx) {
                x / (PI / 180.0)
            } else {
                x * (180.0 / PI)
            };
            stack.replace(ctx, deg);
            Ok(CallbackReturn::Return)
        }),
    )?;

    math.set_field(ctx, "rad", unary(&ctx, "rad", |x| x * (PI / 180.0)))?;
    math.set_field(ctx, "exp", unary(&ctx, "exp", f64::exp))?;
    math.set_field(ctx, "sqrt", unary(&ctx, "sqrt", f64::sqrt))?;

    math.set_field(
        ctx,
        "log",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let x = check_number(ctx, &stack, "log", 0)?;
            // Lua 5.1 has no base argument.
            let log = if is_lua51(ctx) || stack.get(1).is_nil() {
                x.ln()
            } else {
                match check_number(ctx, &stack, "log", 1)? {
                    2.0 => x.log2(),
                    10.0 => x.log10(),
                    base => x.ln() / base.ln(),
                }
            };
            stack.replace(ctx, log);
            Ok(CallbackReturn::Return)
        }),
    )?;

    math.set_field(
        ctx,
        "fmod",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let result = match (stack.get(0), stack.get(1)) {
                // Lua 5.4 keeps integers integers.
                (Value::Integer(a), Value::Integer(d)) if !is_lua51(ctx) => match d {
                    0 => return Err(arg_error("fmod", 1, "zero")),
                    // Avoids the overflow of `mininteger % -1`.
                    -1 => Value::Integer(0),
                    d => Value::Integer(a % d),
                },
                _ => {
                    let a = check_number(ctx, &stack, "fmod", 0)?;
                    let b = check_number(ctx, &stack, "fmod", 1)?;
                    // Rust's `%` on floats is C's `fmod`.
                    Value::Number(a % b)
                }
            };
            stack.replace(ctx, result);
            Ok(CallbackReturn::Return)
        }),
    )?;

    math.set_field(
        ctx,
        "modf",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            if is_lua51(ctx) {
                // C's `modf`: both parts have the sign of the argument.
                let x = check_number(ctx, &stack, "modf", 0)?;
                let integral = x.trunc();
                let fraction = if x.is_infinite() {
                    0.0f64.copysign(x)
                } else {
                    (x - integral).copysign(x)
                };
                stack.replace(ctx, (integral, fraction));
                return Ok(CallbackReturn::Return);
            }
            if let Value::Integer(i) = stack.get(0) {
                stack.replace(ctx, (i, 0.0));
                return Ok(CallbackReturn::Return);
            }
            let x = check_number(ctx, &stack, "modf", 0)?;
            let integral = if x < 0.0 { x.ceil() } else { x.floor() };
            // (The test is needed for infinities.)
            let fraction = if x == integral { 0.0 } else { x - integral };
            stack.replace(ctx, (to_int(ctx, integral), fraction));
            Ok(CallbackReturn::Return)
        }),
    )?;
    Ok(())
}

pub fn load_trig<'gc>(ctx: Context<'gc>, math: Table<'gc>) -> Result<(), LimitError> {
    math.set_field(ctx, "acos", unary(&ctx, "acos", f64::acos))?;
    math.set_field(ctx, "asin", unary(&ctx, "asin", f64::asin))?;

    math.set_field(
        ctx,
        "atan",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let y = check_number(ctx, &stack, "atan", 0)?;
            // Lua 5.1 has no second argument.
            let atan = if is_lua51(ctx) {
                y.atan()
            } else if stack.get(1).is_nil() {
                y.atan2(1.0)
            } else {
                y.atan2(check_number(ctx, &stack, "atan", 1)?)
            };
            stack.replace(ctx, atan);
            Ok(CallbackReturn::Return)
        }),
    )?;

    math.set_field(ctx, "cos", unary(&ctx, "cos", f64::cos))?;
    math.set_field(ctx, "sin", unary(&ctx, "sin", f64::sin))?;
    math.set_field(ctx, "tan", unary(&ctx, "tan", f64::tan))?;
    Ok(())
}

// DELIBERATE(puc): in Lua51 mode too, instead of the C library's `rand` (see COMPATIBILITY.md).
/// The state of `math.random`: Lua 5.4's xoshiro256** generator, so that a seed gives the same
/// sequence as PUC-Rio Lua 5.4 on every platform.
///
/// It is seeded with 0 when created, and by [`Lua::set_random_seed`](crate::Lua::set_random_seed)
/// or `math.randomseed`; it never uses a source of entropy.
#[derive(Debug, Collect)]
#[collect(require_static)]
pub struct RandomState {
    state: Cell<[u64; 4]>,
    // The seed given by the host, which `math.randomseed()` (without arguments) restores.
    seed: Cell<u64>,
}

impl Default for RandomState {
    fn default() -> Self {
        let random = RandomState {
            state: Cell::new([0; 4]),
            seed: Cell::new(0),
        };
        random.set_seed(0, 0);
        random
    }
}

impl RandomState {
    /// Seed the generator like `math.randomseed(n1, n2)`.
    pub fn set_seed(&self, n1: u64, n2: u64) {
        let mut state = [n1, 0xff, n2, 0];
        // Discard the initial values to "spread" the seed.
        for _ in 0..16 {
            next_random(&mut state);
        }
        self.state.set(state);
    }

    /// Seed the generator, and make `math.randomseed()` restore this seed.
    pub fn set_host_seed(&self, seed: u64) {
        self.seed.set(seed);
        self.set_seed(seed, 0);
    }

    /// The next 64 random bits.
    pub fn next(&self) -> u64 {
        let mut state = self.state.get();
        let r = next_random(&mut state);
        self.state.set(state);
        r
    }

    /// A float in `[0, 1)`, from the 53 high bits of a random value.
    pub fn to_float(rv: u64) -> f64 {
        (rv >> 11) as f64 * (0.5 / (1u64 << 52) as f64)
    }

    /// A random integer in `[0, n]`, without bias.
    pub fn project(&self, ran: u64, n: u64) -> u64 {
        if n & n.wrapping_add(1) == 0 {
            // `n + 1` is a power of 2.
            return ran & n;
        }
        // The smallest `2^b - 1` not smaller than `n`.
        let lim = u64::MAX >> n.leading_zeros();
        let mut ran = ran & lim;
        while ran > n {
            ran = self.next() & lim;
        }
        ran
    }
}

fn next_random(state: &mut [u64; 4]) -> u64 {
    let [state0, state1, state2, state3] = *state;
    let state2 = state2 ^ state0;
    let state3 = state3 ^ state1;
    let res = state1.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
    *state = [
        state0 ^ state3,
        state1 ^ state2,
        state2 ^ (state1 << 17),
        state3.rotate_left(45),
    ];
    res
}

pub fn load_random<'gc>(ctx: Context<'gc>, math: Table<'gc>) -> Result<(), LimitError> {
    math.set_field(
        ctx,
        "random",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let random = ctx.singleton::<Rootable![RandomState]>();
            let rv = random.next();
            let (low, up) = match stack.len() {
                0 => {
                    stack.replace(ctx, RandomState::to_float(rv));
                    return Ok(CallbackReturn::Return);
                }
                1 => {
                    let up = check_integer(ctx, &stack, "random", 0)?;
                    if up == 0 && !is_lua51(ctx) {
                        // A full random integer.
                        stack.replace(ctx, rv as i64);
                        return Ok(CallbackReturn::Return);
                    }
                    (1, up)
                }
                2 => (
                    check_integer(ctx, &stack, "random", 0)?,
                    check_integer(ctx, &stack, "random", 1)?,
                ),
                _ => return Err(error("wrong number of arguments")),
            };
            if low > up {
                // Lua 5.1 blames the upper limit.
                let arg = if is_lua51(ctx) { stack.len() - 1 } else { 0 };
                return Err(arg_error("random", arg, "interval is empty"));
            }
            let p = random.project(rv, (up as u64).wrapping_sub(low as u64));
            stack.replace(ctx, p.wrapping_add(low as u64) as i64);
            Ok(CallbackReturn::Return)
        }),
    )?;

    math.set_field(
        ctx,
        "randomseed",
        Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let random = ctx.singleton::<Rootable![RandomState]>();
            let lua51 = is_lua51(ctx);
            // Lua 5.1 requires a seed and takes only one.
            let (n1, n2) = if stack.is_empty() && !lua51 {
                (random.seed.get() as i64, 0)
            } else if lua51 {
                (check_integer(ctx, &stack, "randomseed", 0)?, 0)
            } else {
                (
                    check_integer(ctx, &stack, "randomseed", 0)?,
                    opt_integer(ctx, &stack, "randomseed", 1, 0)?,
                )
            };
            random.set_seed(n1 as u64, n2 as u64);
            // Lua 5.4 returns the seeds, Lua 5.1 nothing.
            if lua51 {
                stack.clear();
            } else {
                stack.replace(ctx, (n1, n2));
            }
            Ok(CallbackReturn::Return)
        }),
    )?;
    Ok(())
}
