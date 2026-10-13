use crate::{
    constant::is_exact_float_integer,
    opcode::{Operation, RCIndex},
    types::RegisterIndex,
    Constant,
};

use super::parser::{BinaryOperator, UnaryOperator};

// Binary operators which map directly to a single opcode
#[derive(Debug, PartialEq, Eq, Hash, Copy, Clone)]
pub enum SimpleBinOp {
    Add,
    Sub,
    Mul,
    Mod,
    Pow,
    Div,
    IDiv,
    BitAnd,
    BitOr,
    BitXor,
    ShiftLeft,
    ShiftRight,
}

// Binary operators which map to Eq / Less / LessEq operations, alone when they decide a branch and
// combined with Jump and LoadBool when a value is needed
#[derive(Debug, PartialEq, Eq, Hash, Copy, Clone)]
pub enum ComparisonBinOp {
    NotEqual,
    Equal,
    LessThan,
    LessEqual,
    GreaterThan,
    GreaterEqual,
}

// 'and' and 'or', which short circuit their right hand side
#[derive(Debug, PartialEq, Eq, Hash, Copy, Clone)]
pub enum ShortCircuitBinOp {
    And,
    Or,
}

// Categorized BinaryOperator
#[derive(Debug, PartialEq, Eq, Hash, Copy, Clone)]
pub enum BinOpCategory {
    Simple(SimpleBinOp),
    Comparison(ComparisonBinOp),
    ShortCircuit(ShortCircuitBinOp),
    Concat,
}

pub fn categorize_binop(binop: BinaryOperator) -> BinOpCategory {
    match binop {
        BinaryOperator::Add => BinOpCategory::Simple(SimpleBinOp::Add),
        BinaryOperator::Sub => BinOpCategory::Simple(SimpleBinOp::Sub),
        BinaryOperator::Mul => BinOpCategory::Simple(SimpleBinOp::Mul),
        BinaryOperator::Mod => BinOpCategory::Simple(SimpleBinOp::Mod),
        BinaryOperator::Pow => BinOpCategory::Simple(SimpleBinOp::Pow),
        BinaryOperator::Div => BinOpCategory::Simple(SimpleBinOp::Div),
        BinaryOperator::IDiv => BinOpCategory::Simple(SimpleBinOp::IDiv),
        BinaryOperator::BitAnd => BinOpCategory::Simple(SimpleBinOp::BitAnd),
        BinaryOperator::BitOr => BinOpCategory::Simple(SimpleBinOp::BitOr),
        BinaryOperator::BitXor => BinOpCategory::Simple(SimpleBinOp::BitXor),
        BinaryOperator::ShiftLeft => BinOpCategory::Simple(SimpleBinOp::ShiftLeft),
        BinaryOperator::ShiftRight => BinOpCategory::Simple(SimpleBinOp::ShiftRight),
        BinaryOperator::Concat => BinOpCategory::Concat,
        BinaryOperator::NotEqual => BinOpCategory::Comparison(ComparisonBinOp::NotEqual),
        BinaryOperator::Equal => BinOpCategory::Comparison(ComparisonBinOp::Equal),
        BinaryOperator::LessThan => BinOpCategory::Comparison(ComparisonBinOp::LessThan),
        BinaryOperator::LessEqual => BinOpCategory::Comparison(ComparisonBinOp::LessEqual),
        BinaryOperator::GreaterThan => BinOpCategory::Comparison(ComparisonBinOp::GreaterThan),
        BinaryOperator::GreaterEqual => BinOpCategory::Comparison(ComparisonBinOp::GreaterEqual),
        BinaryOperator::And => BinOpCategory::ShortCircuit(ShortCircuitBinOp::And),
        BinaryOperator::Or => BinOpCategory::ShortCircuit(ShortCircuitBinOp::Or),
    }
}

pub fn simple_binop_operation(
    simple_binop: SimpleBinOp,
    dest: RegisterIndex,
    left: RCIndex,
    right: RCIndex,
) -> Operation {
    match simple_binop {
        SimpleBinOp::Add => Operation::Add { dest, left, right },
        SimpleBinOp::Sub => Operation::Sub { dest, left, right },
        SimpleBinOp::Mul => Operation::Mul { dest, left, right },
        SimpleBinOp::Div => Operation::Div { dest, left, right },
        SimpleBinOp::IDiv => Operation::IDiv { dest, left, right },
        SimpleBinOp::Mod => Operation::Mod { dest, left, right },
        SimpleBinOp::Pow => Operation::Pow { dest, left, right },
        SimpleBinOp::BitAnd => Operation::BitAnd { dest, left, right },
        SimpleBinOp::BitOr => Operation::BitOr { dest, left, right },
        SimpleBinOp::BitXor => Operation::BitXor { dest, left, right },
        SimpleBinOp::ShiftLeft => Operation::ShiftLeft { dest, left, right },
        SimpleBinOp::ShiftRight => Operation::ShiftRight { dest, left, right },
    }
}

pub fn simple_binop_const_fold<S: AsRef<[u8]>>(
    simple_binop: SimpleBinOp,
    left: &Constant<S>,
    right: &Constant<S>,
) -> Option<Constant<S>> {
    // Like PUC-Rio Lua, only fold numbers: string coercions and errors (such as integer division
    // by zero) are left to run time.
    if !is_number(left) || !is_number(right) {
        return None;
    }
    // `%` on floats depends on the Lua version (see `Compat`), so it is left to run time.
    if matches!(simple_binop, SimpleBinOp::Mod)
        && (matches!(left, Constant::Number(_)) || matches!(right, Constant::Number(_)))
    {
        return None;
    }
    if !is_compat_independent(left) || !is_compat_independent(right) {
        return None;
    }
    let result = match simple_binop {
        SimpleBinOp::Add => left.add(right),
        SimpleBinOp::Sub => left.subtract(right),
        SimpleBinOp::Mul => {
            // Lua 5.1 only has floats, where a zero product with a negative factor is -0.
            // Past 2^63, Lua 5.4 wraps around where 5.1 rounds.
            if let (Constant::Integer(a), Constant::Integer(b)) = (left, right) {
                if ((*a == 0 || *b == 0) && (*a < 0 || *b < 0)) || a.checked_mul(*b).is_none() {
                    return None;
                }
            }
            left.multiply(right)
        }
        SimpleBinOp::Mod => left.modulo(right),
        SimpleBinOp::Pow => left.exponentiate(right),
        SimpleBinOp::Div => left.float_divide(right),
        SimpleBinOp::IDiv => left.floor_divide(right),
        _ => None,
    }?;
    is_compat_independent(&result).then_some(result)
}

pub fn comparison_binop_operation(
    comparison_binop: ComparisonBinOp,
    left: RCIndex,
    right: RCIndex,
    skip_if: bool,
) -> Operation {
    match comparison_binop {
        ComparisonBinOp::Equal => Operation::Eq {
            skip_if,
            left,
            right,
        },
        ComparisonBinOp::NotEqual => Operation::Eq {
            skip_if: !skip_if,
            left,
            right,
        },
        ComparisonBinOp::LessThan => Operation::Less {
            skip_if,
            left,
            right,
        },
        ComparisonBinOp::LessEqual => Operation::LessEq {
            skip_if,
            left,
            right,
        },
        ComparisonBinOp::GreaterThan => Operation::Less {
            skip_if,
            left: right,
            right: left,
        },
        ComparisonBinOp::GreaterEqual => Operation::LessEq {
            skip_if,
            left: right,
            right: left,
        },
    }
}

pub fn comparison_binop_const_fold<S: AsRef<[u8]>>(
    comparison_binop: ComparisonBinOp,
    left: &Constant<S>,
    right: &Constant<S>,
) -> Option<Constant<S>> {
    if !is_compat_independent(left) || !is_compat_independent(right) {
        return None;
    }
    match comparison_binop {
        ComparisonBinOp::Equal => Some(Constant::Boolean(left.is_equal(right))),
        ComparisonBinOp::NotEqual => Some(Constant::Boolean(!left.is_equal(right))),
        ComparisonBinOp::LessThan => Some(Constant::Boolean(left.less_than(right)?)),
        ComparisonBinOp::LessEqual => Some(Constant::Boolean(left.less_equal(right)?)),
        ComparisonBinOp::GreaterThan => Some(Constant::Boolean(right.less_than(left)?)),
        ComparisonBinOp::GreaterEqual => Some(Constant::Boolean(right.less_equal(left)?)),
    }
}

pub fn unop_operation(
    unop: UnaryOperator,
    dest: RegisterIndex,
    source: RegisterIndex,
) -> Operation {
    match unop {
        UnaryOperator::Minus => Operation::Minus { dest, source },
        UnaryOperator::Not => Operation::Not { dest, source },
        UnaryOperator::BitNot => Operation::BitNot { dest, source },
        UnaryOperator::Len => Operation::Length { dest, source },
    }
}

pub fn unop_const_fold<S: AsRef<[u8]>>(
    unop: UnaryOperator,
    cons: &Constant<S>,
) -> Option<Constant<S>> {
    if !is_compat_independent(cons) {
        return None;
    }
    match unop {
        // Lua 5.1 only has floats, where -0 is a negative zero.
        // DELIBERATE(puc): PUC 5.1 merges the constants 0 and -0 within a function; lunita keeps
        // each sign (see COMPATIBILITY.md).
        UnaryOperator::Minus if matches!(cons, Constant::Integer(0)) => None,
        UnaryOperator::Minus if is_number(cons) => cons.negate().filter(is_compat_independent),
        UnaryOperator::Minus => None,
        UnaryOperator::Not => Some(cons.not()),
        UnaryOperator::BitNot if is_number(cons) => cons.bitwise_not(),
        UnaryOperator::BitNot => None,
        UnaryOperator::Len => None,
    }
}

// Whether a constant is the same value in every `Compat` mode: integers past 2^53 are floats in Lua
// 5.1 (see `FunctionPrototype::compile_with_budget`), so operations on them, or giving them, are
// left to run time.
fn is_compat_independent<S>(cons: &Constant<S>) -> bool {
    match *cons {
        Constant::Integer(i) => is_exact_float_integer(i),
        _ => true,
    }
}

fn is_number<S>(cons: &Constant<S>) -> bool {
    matches!(cons, Constant::Integer(_) | Constant::Number(_))
}
