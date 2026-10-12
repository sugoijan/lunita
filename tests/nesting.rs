//! Deeply nested and long chained syntax must either compile or fail with a compile error, never
//! overflow the native stack in the parser, the compiler, or when dropping the syntax tree.

use piccolo::{Closure, Executor, Lua};

// Debug builds use several times more stack per level than release builds (about 3 MiB at the
// deepest accepted nesting, 650 KiB in release), so run on a thread with a known stack size.
const STACK_SIZE: usize = 16 << 20;

#[derive(Debug, PartialEq)]
enum Outcome {
    Ok,
    CompileError,
    RuntimeError,
}

fn run(source: String) -> Outcome {
    std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let mut lua = Lua::core();
            let Ok(executor) = lua.try_enter(|ctx| {
                let closure = Closure::load(ctx, None, source.as_bytes())?;
                Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
            }) else {
                return Outcome::CompileError;
            };
            match lua.execute::<()>(&executor) {
                Ok(()) => Outcome::Ok,
                Err(_) => Outcome::RuntimeError,
            }
        })
        .unwrap()
        .join()
        .unwrap()
}

fn nested(prefix: &str, open: &str, middle: &str, close: &str, n: usize) -> String {
    format!("{prefix}{}{middle}{}", open.repeat(n), close.repeat(n))
}

const DEEP: usize = 100_000;

#[test]
fn deep_suffix_chains() {
    for (prefix, suffix) in [
        ("local a = {} a.b = a local x = a", ".b"),
        ("local a = {} a[a] = a local x = a", "[a]"),
        ("local function f() return f end local x = f", "()"),
        (
            "local a = {} function a:b() return a end local x = a",
            ":b()",
        ),
        ("local a = {} a.b = a a", ".b"),
    ] {
        let source = format!("{prefix}{}", suffix.repeat(DEEP));
        assert_eq!(run(source), Outcome::CompileError, "{prefix} {suffix}");
    }

    let source = format!("local a = {{}} a.b = a a{} = 1", ".b".repeat(DEEP));
    assert_eq!(run(source), Outcome::CompileError);

    let source = format!(
        "local a = {{}} a.b = a function a{}.c() end",
        ".b".repeat(DEEP)
    );
    assert_eq!(run(source), Outcome::CompileError);
}

#[test]
fn deep_expressions() {
    for (prefix, op) in [
        ("local a = 1 local x = a", "+a"),
        ("local a = 1 local x = a", " < a"),
        ("local a = 'a' local x = a", "..a"),
        ("local a = 2 local x = a", "^a"),
        ("local x = 1", "+1"),
    ] {
        let source = format!("{prefix}{}", op.repeat(DEEP));
        let outcome = run(source);
        assert!(
            matches!(outcome, Outcome::Ok | Outcome::CompileError),
            "{prefix} {op}: {outcome:?}"
        );
    }

    for (prefix, open, middle, close, n) in [
        ("local x = ", "- ", "1", "", DEEP),
        ("local x = ", "not ", "1", "", DEEP),
        ("local x = ", "(", "1", ")", DEEP),
        ("local x = ", "{", "", "}", DEEP),
        ("local x = ", "function() return ", "1", " end", 5_000),
        ("", "do ", "", "end ", DEEP),
        ("local a = true ", "if a then ", "", "end ", DEEP),
        ("local a = false ", "while a do ", "", "end ", DEEP),
        ("local a = true ", "repeat ", "", "until a ", DEEP),
        ("local f = print local x = ", "f(1, ", "1", ")", DEEP),
    ] {
        let outcome = run(nested(prefix, open, middle, close, n));
        assert!(
            matches!(outcome, Outcome::Ok | Outcome::CompileError),
            "{open}: {outcome:?}"
        );
    }
}

#[test]
fn deepest_accepted_nesting() {
    // Find the deepest accepted nesting of each construct and check it also runs.
    for (prefix, open, middle, close) in [
        ("local a = {} a.b = a local x = a", ".b", "", ""),
        ("local x = ", "(", "1", ")"),
        ("local x = ", "{", "", "}"),
        ("local x = ", "function() return ", "1", " end"),
        ("local a = true ", "if a then ", "", "end "),
        (
            "local function f(...) return ... end local x = ",
            "f({(",
            "1",
            ")})",
        ),
    ] {
        let (mut accepted, mut rejected) = (0, 1000);
        while rejected - accepted > 1 {
            let n = (accepted + rejected) / 2;
            match run(nested(prefix, open, middle, close, n)) {
                Outcome::Ok => accepted = n,
                Outcome::CompileError => rejected = n,
                Outcome::RuntimeError => panic!("{open}: runtime error at depth {n}"),
            }
        }
        assert!(accepted >= 40, "{open}: only {accepted} levels accepted");
    }
}

#[test]
fn many_registers() {
    // Running out of registers is a compile error, including at exactly the last register.
    for n in [127, 128, 129, 200] {
        let outcome = run(nested("local x = ", "{", "", "}", n));
        assert!(
            matches!(outcome, Outcome::Ok | Outcome::CompileError),
            "{n}: {outcome:?}"
        );
    }
    let locals = (0..300)
        .map(|i| format!("a{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(run(format!("local {locals} = 1")), Outcome::CompileError);

    // Locals initialized to nil up to the last register (`LoadNil` ending at register 255).
    let locals = (0..254)
        .map(|i| format!("a{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(run(format!("local {locals} = 1 local x, y")), Outcome::Ok);
}

#[test]
fn integer_edge_regressions() {
    assert_eq!(
        run("assert(math.mininteger .. '' == '-9223372036854775808')".to_owned()),
        Outcome::Ok
    );
}
