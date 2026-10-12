use allocator_api2::vec;
use gc_arena::allocator_api::MetricsAlloc;

use crate::{
    compat::Compat,
    constant::f64_to_i64,
    meta_ops::{self, ConcatMetaResult, MetaResult},
    opcode::{Operation, RCIndex},
    thread::thread::MetaReturn,
    types::{RegisterIndex, UpValueDescriptor, VarCount},
    Closure, Constant, Context, Function, String, Table, Value,
};

use super::{
    thread::{LuaFrame, LuaRegisters},
    VMError,
};

// Runs the VM for the given number of instructions or until the current LuaFrame may have been
// changed.
//
// Returns the number of instructions that were run.
pub(super) fn run_vm<'gc>(
    ctx: Context<'gc>,
    mut lua_frame: LuaFrame<'gc, '_>,
    max_instructions: u32,
) -> Result<u32, VMError> {
    if max_instructions == 0 {
        return Ok(0);
    }

    let current_function = lua_frame.closure()?;
    let current_prototype = current_function.prototype();
    let current_upvalues = current_function.upvalues();
    // The executor consumes the fuel for the instructions run and the fuel debt after the batch.
    let fuel_left = i64::from(lua_frame.fuel.remaining());
    let mut registers = lua_frame.registers()?;
    let mut instructions_run = 0;

    // The compiler only emits register, constant, upvalue and prototype indexes in range, and jumps
    // within the function, but we check every access anyway.

    fn get_rc<'gc>(
        registers: &LuaRegisters<'gc, '_>,
        constants: &[Constant<String<'gc>>],
        rc: RCIndex,
    ) -> Result<Value<'gc>, VMError> {
        match rc {
            RCIndex::Register(r) => registers.get(r.0 as usize),
            RCIndex::Constant(c) => constants
                .get(c.0 as usize)
                .map(|&c| c.into())
                .ok_or(VMError::BadConstant),
        }
    }

    let get_upvalue_ref = |i: usize| current_upvalues.get(i).copied().ok_or(VMError::BadUpValue);

    loop {
        let op = current_prototype
            .opcodes
            .get(*registers.pc)
            .ok_or(VMError::BadPc)?
            .decode();
        *registers.pc += 1;

        match op {
            Operation::Move { dest, source } => {
                registers.set(dest.0 as usize, registers.get(source.0 as usize)?)?;
            }

            Operation::LoadConstant { dest, constant } => {
                registers.set(
                    dest.0 as usize,
                    current_prototype
                        .constants
                        .get(constant.0 as usize)
                        .map(|&c| c.into())
                        .ok_or(VMError::BadConstant)?,
                )?;
            }

            Operation::LoadBool {
                dest,
                value,
                skip_next,
            } => {
                registers.set(dest.0 as usize, Value::Boolean(value))?;
                if skip_next {
                    *registers.pc += 1;
                }
            }

            Operation::LoadNil { dest, count } => {
                let start = dest.0 as usize;
                registers
                    .stack_frame
                    .get_mut(start..start + count as usize)
                    .ok_or(VMError::BadStack)?
                    .fill(Value::Nil);
            }

            Operation::NewTable {
                dest,
                array_size,
                map_size,
            } => {
                let table = Table::try_with_capacity(ctx, array_size as usize, map_size as usize)?;
                registers.set(dest.0 as usize, Value::Table(table))?;
            }

            Operation::GetTable { dest, table, key } => {
                let table = registers.get(table.0 as usize)?;
                let key = get_rc(&registers, &current_prototype.constants, key)?;
                match meta_ops::index(ctx, table, key)? {
                    MetaResult::Value(v) => {
                        registers.set(dest.0 as usize, v)?;
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::SetTable { table, key, value } => {
                let table = registers.get(table.0 as usize)?;
                let key = get_rc(&registers, &current_prototype.constants, key)?;
                let value = get_rc(&registers, &current_prototype.constants, value)?;
                if let Some(call) = meta_ops::new_index(ctx, table, key, value)? {
                    lua_frame.call_meta_function(
                        ctx,
                        call.function,
                        &call.args,
                        MetaReturn::None,
                    )?;
                    break;
                }
            }

            Operation::GetUpTable { dest, table, key } => {
                let table = registers.get_upvalue(&ctx, get_upvalue_ref(table.0 as usize)?);
                let key = get_rc(&registers, &current_prototype.constants, key)?;
                match meta_ops::index(ctx, table, key)? {
                    MetaResult::Value(v) => {
                        registers.set(dest.0 as usize, v)?;
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::SetUpTable { table, key, value } => {
                let table = registers.get_upvalue(&ctx, get_upvalue_ref(table.0 as usize)?);
                let key = get_rc(&registers, &current_prototype.constants, key)?;
                let value = get_rc(&registers, &current_prototype.constants, value)?;
                if let Some(call) = meta_ops::new_index(ctx, table, key, value)? {
                    lua_frame.call_meta_function(
                        ctx,
                        call.function,
                        &call.args,
                        MetaReturn::None,
                    )?;
                    break;
                }
            }

            Operation::SetList { base, count } => {
                lua_frame.set_table_list(ctx, base, count)?;
                registers = lua_frame.registers()?;
            }

            Operation::Call {
                func,
                args,
                returns,
            } => {
                lua_frame.call_function(ctx, func, args, returns)?;
                break;
            }

            Operation::TailCall { func, args } => {
                lua_frame.tail_call_function(ctx, func, args)?;
                break;
            }

            Operation::Return { start, count } => {
                lua_frame.return_upper(ctx, start, count)?;
                break;
            }

            Operation::VarArgs { dest, count } => {
                lua_frame.varargs(ctx, dest, count)?;
                registers = lua_frame.registers()?;
            }

            Operation::Jump {
                offset,
                close_upvalues,
            } => {
                *registers.pc = add_offset(*registers.pc, offset)?;
                if let Some(r) = close_upvalues.to_u8() {
                    registers.close_upvalues(&ctx, RegisterIndex(r));
                }
            }

            Operation::Test { value, is_true } => {
                let value = registers.get(value.0 as usize)?;
                if value.to_bool() == is_true {
                    *registers.pc += 1;
                }
            }

            Operation::TestSet {
                dest,
                value,
                is_true,
            } => {
                let value = registers.get(value.0 as usize)?;
                if value.to_bool() == is_true {
                    *registers.pc += 1;
                } else {
                    registers.set(dest.0 as usize, value)?;
                }
            }

            Operation::Closure { proto, dest } => {
                let proto = *current_prototype
                    .prototypes
                    .get(proto.0 as usize)
                    .ok_or(VMError::BadPrototype)?;
                let mut upvalues = vec::Vec::new_in(MetricsAlloc::new(&ctx));
                for &desc in proto.upvalues.iter() {
                    match desc {
                        UpValueDescriptor::Environment => {
                            return Err(VMError::BadEnvUpValue.into());
                        }
                        UpValueDescriptor::ParentLocal(reg) => {
                            upvalues.push(registers.open_upvalue(&ctx, reg));
                        }
                        UpValueDescriptor::Outer(uvindex) => {
                            upvalues.push(get_upvalue_ref(uvindex.0 as usize)?);
                        }
                    }
                }

                let closure = Closure::from_parts(&ctx, proto, upvalues);
                registers.set(dest.0 as usize, Value::Function(Function::Closure(closure)))?;
            }

            // DELIBERATE(puc): an integer loop counts exactly past 2^53 in Lua51 mode too, where 5.1's
            // float counter stalls (see COMPATIBILITY.md).
            Operation::NumericForPrep { base, jump } => {
                let base = base.0 as usize;
                let (init, limit, step) = (
                    registers.get(base)?,
                    registers.get(base + 1)?,
                    registers.get(base + 2)?,
                );
                // On success, the loop body (which follows this instruction) is entered with the
                // loop variable set, otherwise we skip the loop by jumping past `NumericForLoop`.
                let skip = match for_prep(ctx, init, limit, step)? {
                    ForPrep::Skip => true,
                    ForPrep::Integer { init, count } => {
                        registers.set(base, Value::Integer(init))?;
                        registers.set(base + 1, Value::Integer(count as i64))?;
                        registers.set(base + 3, Value::Integer(init))?;
                        false
                    }
                    ForPrep::Float { init, limit, step } => {
                        registers.set(base, Value::Number(init))?;
                        registers.set(base + 1, Value::Number(limit))?;
                        registers.set(base + 2, Value::Number(step))?;
                        registers.set(base + 3, Value::Number(init))?;
                        false
                    }
                };
                if skip {
                    *registers.pc = add_offset(*registers.pc, jump)? + 1;
                }
            }

            Operation::NumericForLoop { base, jump } => {
                let base = base.0 as usize;
                match (
                    registers.get(base)?,
                    registers.get(base + 1)?,
                    registers.get(base + 2)?,
                ) {
                    (Value::Integer(index), Value::Integer(count), Value::Integer(step)) => {
                        // The remaining iteration count is stored in place of the limit as an
                        // unsigned integer, so the loop never overflows.
                        let count = count as u64;
                        if count > 0 {
                            let index = index.wrapping_add(step);
                            registers.set(base, Value::Integer(index))?;
                            registers.set(base + 1, Value::Integer((count - 1) as i64))?;
                            registers.set(base + 3, Value::Integer(index))?;
                            *registers.pc = add_offset(*registers.pc, jump)?;
                        }
                    }
                    (Value::Number(index), Value::Number(limit), Value::Number(step)) => {
                        let index = index + step;
                        if if step > 0.0 {
                            index <= limit
                        } else {
                            limit <= index
                        } {
                            registers.set(base, Value::Number(index))?;
                            registers.set(base + 3, Value::Number(index))?;
                            *registers.pc = add_offset(*registers.pc, jump)?;
                        }
                    }
                    (index, limit, step) => {
                        return Err(VMError::BadForLoop(
                            index.type_name(),
                            limit.type_name(),
                            step.type_name(),
                        ));
                    }
                }
            }

            Operation::GenericForCall { base, var_count } => {
                lua_frame.call_function_keep(ctx, base, 2, VarCount::constant(var_count))?;
                break;
            }

            Operation::GenericForLoop { base, jump } => {
                let control = registers.get(base.0 as usize + 1)?;
                // Only nil ends the loop: a false control value is passed on, as in PUC-Rio Lua.
                if !control.is_nil() {
                    registers.set(base.0 as usize, control)?;
                    *registers.pc = add_offset(*registers.pc, jump)?;
                }
            }

            Operation::Method { base, table, key } => {
                let table = registers.get(table.0 as usize)?;
                let key = get_rc(&registers, &current_prototype.constants, key)?;
                registers.set(base.0 as usize + 1, table)?;
                match meta_ops::index(ctx, table, key)? {
                    MetaResult::Value(v) => {
                        registers.set(base.0 as usize, v)?;
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(base),
                        )?;
                        break;
                    }
                }
            }

            Operation::Concat {
                dest,
                source,
                count,
            } => {
                let base = source.0 as usize;
                let values = registers
                    .stack_frame
                    .get(base..base + count as usize)
                    .ok_or(VMError::BadStack)?;
                match meta_ops::concat_many(ctx, values)? {
                    ConcatMetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    ConcatMetaResult::Call(func) => {
                        // This would be nice to do in-place (without copying params)
                        // but that turns out to be difficult to do without corrupting
                        // the stack, and likely isn't worth the complexity for the
                        // fallback case.
                        let args = values.to_owned();
                        lua_frame.call_meta_function(
                            ctx,
                            func,
                            &args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::GetUpValue { source, dest } => {
                registers.set(
                    dest.0 as usize,
                    registers.get_upvalue(&ctx, get_upvalue_ref(source.0 as usize)?),
                )?;
            }

            Operation::SetUpValue { source, dest } => {
                registers.set_upvalue(
                    &ctx,
                    get_upvalue_ref(dest.0 as usize)?,
                    registers.get(source.0 as usize)?,
                );
            }

            Operation::Length { dest, source } => {
                match meta_ops::len(ctx, registers.get(source.0 as usize)?)? {
                    MetaResult::Value(v) => {
                        registers.set(dest.0 as usize, v)?;
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Eq {
                skip_if,
                left,
                right,
            } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::equal(ctx, left, right)? {
                    MetaResult::Value(v) => {
                        if v.to_bool() == skip_if {
                            *registers.pc += 1;
                        }
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::SkipIf(skip_if),
                        )?;
                        break;
                    }
                }
            }

            Operation::Less {
                skip_if,
                left,
                right,
            } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::less_than(ctx, left, right)? {
                    MetaResult::Value(v) => {
                        if v.to_bool() == skip_if {
                            *registers.pc += 1;
                        }
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::SkipIf(skip_if),
                        )?;
                        break;
                    }
                }
            }

            Operation::LessEq {
                skip_if,
                left,
                right,
            } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::less_equal(ctx, left, right)? {
                    MetaResult::Value(v) => {
                        if v.to_bool() == skip_if {
                            *registers.pc += 1;
                        }
                    }
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::SkipIf(skip_if),
                        )?;
                        break;
                    }
                }
            }

            Operation::Not { dest, source } => {
                let source = registers.get(source.0 as usize)?;
                registers.set(dest.0 as usize, (!source.to_bool()).into())?;
            }

            Operation::Minus { dest, source } => {
                let value = registers.get(source.0 as usize)?;
                match meta_ops::negate(ctx, value)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::BitNot { dest, source } => {
                let value = registers.get(source.0 as usize)?;
                match meta_ops::bitwise_not(ctx, value)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Add { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::add(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Sub { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::subtract(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Mul { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::multiply(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Div { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::float_divide(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::IDiv { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::floor_divide(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Mod { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::modulo(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::Pow { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::exponentiate(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::BitAnd { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::bitwise_and(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::BitOr { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::bitwise_or(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::BitXor { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::bitwise_xor(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::ShiftLeft { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::shift_left(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }

            Operation::ShiftRight { dest, left, right } => {
                let left = get_rc(&registers, &current_prototype.constants, left)?;
                let right = get_rc(&registers, &current_prototype.constants, right)?;
                match meta_ops::shift_right(ctx, left, right)? {
                    MetaResult::Value(v) => registers.set(dest.0 as usize, v)?,
                    MetaResult::Call(call) => {
                        lua_frame.call_meta_function(
                            ctx,
                            call.function,
                            &call.args,
                            MetaReturn::Register(dest),
                        )?;
                        break;
                    }
                }
            }
        }

        instructions_run += 1;
        if instructions_run >= max_instructions {
            break;
        }
        // An instruction that charged fuel for its work (such as a long concatenation) ends the
        // batch if the step is out of fuel or a limit was exceeded.
        let debt = ctx.fuel_debt();
        if debt > 0
            && (i64::from(debt) + i64::from(instructions_run) >= fuel_left
                || ctx.limit_exceeded().is_some())
        {
            break;
        }
    }

    Ok(instructions_run)
}

fn add_offset(pc: usize, offset: i16) -> Result<usize, VMError> {
    pc.checked_add_signed(offset.into()).ok_or(VMError::BadPc)
}

enum ForPrep {
    Skip,
    Integer { init: i64, count: u64 },
    Float { init: f64, limit: f64, step: f64 },
}

// Prepares a numeric for loop as PUC-Rio Lua 5.4 does: if the initial value and the step are both
// integers, the loop is an integer loop whose iteration count is computed up front (so it can never
// overflow), otherwise it is a float loop.
fn for_prep<'gc>(
    ctx: Context<'gc>,
    init: Value<'gc>,
    limit: Value<'gc>,
    step: Value<'gc>,
) -> Result<ForPrep, VMError> {
    // Converting string operands takes time proportional to their length.
    meta_ops::charge_strings(ctx, &[init, limit, step]);
    let lua51 = ctx.compat() == Compat::Lua51;
    // Lua 5.1 has only float loops, so a zero integer step takes the float path below.
    let integers = match (init, step) {
        (Value::Integer(init), Value::Integer(step)) if !(lua51 && step == 0) => Some((init, step)),
        _ => None,
    };
    if let Some((init, step)) = integers {
        if step == 0 {
            return Err(VMError::ForStepZero);
        }

        let limit = match limit {
            Value::Integer(limit) => limit,
            limit => {
                let Some(limit) = limit.to_number() else {
                    return Err(VMError::BadForLoopPrep("limit", limit.type_name()));
                };
                // Round the limit towards the loop's direction, and clip it to the integer range
                // if it is out of range.
                let rounded = if step < 0 {
                    limit.ceil()
                } else {
                    limit.floor()
                };
                match f64_to_i64(rounded) {
                    Some(limit) => limit,
                    None if limit > 0.0 => {
                        if step < 0 {
                            return Ok(ForPrep::Skip);
                        }
                        i64::MAX
                    }
                    None => {
                        if step > 0 {
                            return Ok(ForPrep::Skip);
                        }
                        i64::MIN
                    }
                }
            }
        };

        if if step > 0 { init > limit } else { init < limit } {
            return Ok(ForPrep::Skip);
        }

        let count = if step > 0 {
            (limit as u64).wrapping_sub(init as u64) / (step as u64)
        } else {
            // `step + 1` avoids negating `i64::MIN`.
            (init as u64).wrapping_sub(limit as u64) / ((-(step + 1)) as u64 + 1)
        };
        Ok(ForPrep::Integer { init, count })
    } else {
        let Some(init) = init.to_number() else {
            return Err(VMError::BadForLoopPrep("initial", init.type_name()));
        };
        let Some(limit) = limit.to_number() else {
            return Err(VMError::BadForLoopPrep("limit", limit.type_name()));
        };
        let Some(step) = step.to_number() else {
            return Err(VMError::BadForLoopPrep("step", step.type_name()));
        };
        if lua51 {
            // Lua 5.1 starts from `init - step + step` and runs while the counter compares
            // with the limit (`<=` for a positive step, `>=` otherwise): a NaN or an infinite
            // step runs no iteration, and a zero step runs none when the start is below the
            // limit but otherwise never ends (until the fuel does).
            let init = init - step + step;
            if !(if step > 0.0 {
                init <= limit
            } else {
                init >= limit
            }) {
                return Ok(ForPrep::Skip);
            }
            return Ok(ForPrep::Float { init, limit, step });
        }
        if step == 0.0 {
            return Err(VMError::ForStepZero);
        }
        if if step > 0.0 {
            limit < init
        } else {
            init < limit
        } {
            Ok(ForPrep::Skip)
        } else {
            Ok(ForPrep::Float { init, limit, step })
        }
    }
}
