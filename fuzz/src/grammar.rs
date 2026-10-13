//! Lua programs generated from the fuzzer's bytes: every statement and expression form, calls to
//! the whole standard library and string methods, metatables and coroutines, and the limit cases'
//! sources (`seeds.txt`) spliced in as blocks.
//!
//! Every choice reads the bytes, and the first option of each is the simplest, so that a short
//! input still gives a small valid program. A node budget and a nesting limit bound the output.

use arbitrary::{Result, Unstructured};

/// Names the programs use, all declared by the prelude (the last three are globals).
const NAMES: &[&str] = &["a", "b", "c", "s", "t", "f", "n", "x", "y", "z"];

const PRELUDE: &str = "local a, b, c, s, t, f, n = 1, 2.5, 'x', 'hello world 123', \
    {1, 2, 3, k = 'v', [10] = 'ten'}, function(...) return ... end, 3\n\
    x, y, z = 0, 'y', {}\n";

const LIBRARY: &[&str] = &[
    "assert",
    "collectgarbage",
    "error",
    "getmetatable",
    "ipairs",
    "load",
    "next",
    "pairs",
    "pcall",
    "print",
    "rawequal",
    "rawget",
    "rawlen",
    "rawset",
    "select",
    "setmetatable",
    "tonumber",
    "tostring",
    "type",
    "xpcall",
    "coroutine.create",
    "coroutine.resume",
    "coroutine.running",
    "coroutine.status",
    "coroutine.yield",
    "math.abs",
    "math.acos",
    "math.asin",
    "math.atan",
    "math.ceil",
    "math.cos",
    "math.deg",
    "math.exp",
    "math.floor",
    "math.fmod",
    "math.log",
    "math.max",
    "math.min",
    "math.modf",
    "math.rad",
    "math.random",
    "math.randomseed",
    "math.sin",
    "math.sqrt",
    "math.tan",
    "math.tointeger",
    "math.type",
    "math.ult",
    "string.byte",
    "string.char",
    "string.find",
    "string.format",
    "string.gmatch",
    "string.gsub",
    "string.len",
    "string.lower",
    "string.match",
    "string.rep",
    "string.reverse",
    "string.sub",
    "string.upper",
    "table.concat",
    "table.insert",
    "table.move",
    "table.pack",
    "table.remove",
    "table.sort",
    "table.unpack",
];

const STRING_METHODS: &[&str] = &[
    "byte", "find", "format", "gmatch", "gsub", "len", "lower", "match", "rep", "reverse", "sub",
    "upper",
];

const CONSTANTS: &[&str] = &[
    "math.huge",
    "-math.huge",
    "math.pi",
    "math.maxinteger",
    "math.mininteger",
    "_VERSION",
];

const INTEGERS: &[&str] = &[
    "0",
    "1",
    "-1",
    "2",
    "3",
    "10",
    "255",
    "256",
    "1000",
    "65536",
    "2147483647",
    "-2147483648",
    "9223372036854775807",
    "0x7fffffffffffffff",
    "0xffffffffffffffff",
    "0x10",
    "1000000",
    "1e6",
];

const FLOATS: &[&str] = &[
    "0.5", "1.5", "-0.0", "0.1", "1e308", "1e-308", "5e-324", "2^53", "2^63", "1/0", "-1/0", "0/0",
    "3.0", "1e15", "0x1p4", ".5",
];

const STRINGS: &[&str] = &[
    "''",
    "'a'",
    "'abc'",
    "'hello'",
    "'%d'",
    "'%s'",
    "'%5.2f'",
    "'%q'",
    "'%x'",
    "'%g'",
    "'%a'",
    "'%c'",
    "'%i'",
    "'%%'",
    "'%-10s|'",
    "'%+.3e'",
    "'%a+'",
    "'(%w+)'",
    "'%d+'",
    "'[%a_][%w_]*'",
    "'%b()'",
    "'%f[%w]%w+'",
    "'(a*(.)%w(%s*))'",
    "'^(.-)$'",
    "'[^%s]+'",
    "'%1'",
    "'(()a)'",
    "'.-'",
    "'a*'",
    "'x?'",
    "'[a-'",
    "'%'",
    "'(()'",
    "'%z'",
    "'\\0'",
    "'\\x41\\65\\u{48}'",
    "'\\z  '",
    "[[long\nstring]]",
    "[==[]]]==]",
    "'10'",
    "' 0x1F '",
    "'1e2'",
    "'nan'",
    "'%%%d'",
    "'abc\\0def'",
    "('x'):rep(100)",
    "('ab'):rep(1000, ',')",
];

const BINOPS: &[&str] = &[
    "+", "-", "*", "/", "//", "%", "^", "..", "==", "~=", "<", "<=", ">", ">=", "and", "or", "&",
    "|", "~", "<<", ">>",
];

// (Followed by a space, so that `-` and `-1` do not make a comment.)
const UNOPS: &[&str] = &["- ", "not ", "# ", "~ "];

const METAMETHODS: &[&str] = &[
    "__index",
    "__newindex",
    "__call",
    "__add",
    "__sub",
    "__mul",
    "__div",
    "__mod",
    "__pow",
    "__unm",
    "__idiv",
    "__band",
    "__bor",
    "__bxor",
    "__shl",
    "__shr",
    "__bnot",
    "__concat",
    "__len",
    "__eq",
    "__lt",
    "__le",
    "__tostring",
    "__metatable",
    "__name",
    "__close",
    "__gc",
    "__mode",
    "__pairs",
];

const MAX_DEPTH: u32 = 8;

pub struct Generator<'a, 'b> {
    u: &'a mut Unstructured<'b>,
    out: String,
    depth: u32,
    loops: u32,
    budget: u32,
    labels: u32,
}

/// Generates a program, splicing in `seeds` (Lua sources) when the input chooses to.
pub fn program(u: &mut Unstructured<'_>, seeds: &[&str]) -> Result<String> {
    let mut g = Generator {
        u,
        out: String::from(PRELUDE),
        depth: 0,
        loops: 0,
        budget: 300,
        labels: 0,
    };
    // (Seeds are the limit cases, which mostly run their whole budget or up to the memory limit,
    // a hundred times longer than other programs: splicing one into 1 program in 32 keeps them a
    // third of the fuzzing time.)
    if !seeds.is_empty() && g.chance(1, 32)? {
        let seed = g.u.choose(seeds)?;
        g.out.push_str("do\n");
        g.out.push_str(seed);
        g.out.push_str("\nend\n");
    }
    g.block(12)?;
    if g.chance(1, 4)? {
        g.out.push_str("return ");
        g.exprlist(3)?;
        g.out.push('\n');
    }
    Ok(g.out)
}

impl Generator<'_, '_> {
    /// True with odds `n` in `d`, false once the input is used up (where `Unstructured::ratio` is
    /// always true, which would never end a loop).
    fn chance(&mut self, n: u8, d: u8) -> Result<bool> {
        if self.u.is_empty() {
            return Ok(false);
        }
        self.u.ratio(n, d)
    }

    fn pick(&mut self, n: u32) -> Result<u32> {
        self.u.int_in_range(0..=n.saturating_sub(1))
    }

    fn push(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn word(&mut self, words: &[&str]) -> Result<()> {
        let w = *self.u.choose(words)?;
        self.push(w);
        Ok(())
    }

    fn name(&mut self) -> Result<()> {
        self.word(NAMES)
    }

    fn block(&mut self, max: u32) -> Result<()> {
        let n = self.u.int_in_range(1..=max)?;
        for _ in 0..n {
            if self.budget == 0 {
                break;
            }
            self.stat()?;
            self.push("\n");
        }
        Ok(())
    }

    fn nested_block(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth < MAX_DEPTH {
            self.block(4)?;
        } else {
            self.push("local _ = 1");
        }
        self.depth -= 1;
        Ok(())
    }

    fn loop_body(&mut self) -> Result<()> {
        self.loops += 1;
        self.nested_block()?;
        self.loops -= 1;
        Ok(())
    }

    fn stat(&mut self) -> Result<()> {
        self.budget = self.budget.saturating_sub(1);
        match self.pick(22)? {
            0 => {
                self.push("local ");
                self.name()?;
                self.push(" = ");
                self.expr()?;
            }
            1 => {
                self.var()?;
                if self.chance(1, 3)? {
                    self.push(", ");
                    self.var()?;
                }
                self.push(" = ");
                self.exprlist(3)?;
            }
            2 => self.call(true)?,
            3 => {
                self.push("do ");
                self.nested_block()?;
                self.push(" end");
            }
            4 => {
                self.push("while ");
                self.condition("false")?;
                self.push(" do ");
                self.loop_body()?;
                self.push(" end");
            }
            5 => {
                self.push("repeat ");
                self.loop_body()?;
                self.push(" until ");
                self.condition("true")?;
            }
            6 => {
                self.push("if ");
                self.expr()?;
                self.push(" then ");
                self.nested_block()?;
                while self.chance(1, 3)? {
                    self.push(" elseif ");
                    self.expr()?;
                    self.push(" then ");
                    self.nested_block()?;
                }
                if self.chance(1, 2)? {
                    self.push(" else ");
                    self.nested_block()?;
                }
                self.push(" end");
            }
            7 => {
                self.push("for i = ");
                self.expr()?;
                self.push(", ");
                self.expr()?;
                if self.chance(1, 2)? {
                    self.push(", ");
                    self.expr()?;
                }
                self.push(" do ");
                self.loop_body()?;
                self.push(" end");
            }
            8 => {
                self.push("for k, v in ");
                match self.pick(5)? {
                    0 => self.push("pairs(t)"),
                    1 => self.push("ipairs(t)"),
                    2 => self.push("next, t"),
                    3 => {
                        self.push("string.gmatch(");
                        self.expr()?;
                        self.push(", ");
                        self.word(STRINGS)?;
                        self.push(")");
                    }
                    _ => self.exprlist(3)?,
                }
                self.push(" do ");
                self.loop_body()?;
                self.push(" end");
            }
            9 => {
                self.push("function ");
                match self.pick(3)? {
                    0 => self.name()?,
                    1 => self.push("t.m"),
                    _ => self.push("t:m"),
                }
                self.funcbody()?;
            }
            10 => {
                self.push("local function ");
                self.name()?;
                self.funcbody()?;
            }
            11 => {
                self.push("do return ");
                self.exprlist(2)?;
                self.push(" end");
            }
            12 if self.loops > 0 => self.push("break"),
            12 => self.push(";"),
            13 => {
                self.labels += 1;
                let l = self.labels;
                self.push(&format!("do goto l{l} ::l{l}:: end"));
            }
            14 => {
                self.push("local k");
                self.labels += 1;
                let l = self.labels;
                self.push(&format!("{l} <const> = "));
                self.expr()?;
            }
            15 => {
                self.push("pcall(function(...) ");
                self.function_block()?;
                self.push(" end, ");
                self.exprlist(2)?;
                self.push(")");
            }
            16 => {
                self.push("local co = coroutine.create(function(...) ");
                self.function_block()?;
                self.push(" end) coroutine.resume(co, ");
                self.exprlist(2)?;
                self.push(") coroutine.resume(co)");
            }
            17 => {
                self.push("setmetatable(");
                self.name()?;
                self.push(", ");
                self.metatable()?;
                self.push(")");
            }
            18 => {
                self.push("local ");
                self.name()?;
                self.push(", ");
                self.name()?;
                self.push(" = ");
                self.exprlist(3)?;
            }
            19 => {
                self.push("xpcall(function(...) ");
                self.function_block()?;
                self.push(" end, function(m, ...) ");
                self.function_block()?;
                self.push(" return m end)");
            }
            20 => {
                self.push("s = string.format(");
                self.word(STRINGS)?;
                self.push(", ");
                self.exprlist(3)?;
                self.push(")");
            }
            _ => {
                self.push("t[");
                self.expr()?;
                self.push(" ] = ");
                self.expr()?;
            }
        }
        Ok(())
    }

    // A loop condition, `default` (which ends the loop) unless the input chooses an expression, so
    // that most programs end before their fuel budget.
    fn condition(&mut self, default: &str) -> Result<()> {
        if self.chance(1, 2)? {
            self.expr()
        } else {
            self.push(default);
            Ok(())
        }
    }

    fn funcbody(&mut self) -> Result<()> {
        match self.pick(4)? {
            // (Always vararg, so that the body may use `...`.)
            0 => self.push("(...)"),
            1 => self.push("(a, ...)"),
            2 => self.push("(a, b, ...)"),
            _ => self.push("(...)"),
        }
        self.push(" ");
        self.function_block()?;
        if self.chance(1, 2)? {
            self.push(" return ");
            self.exprlist(2)?;
        }
        self.push(" end");
        Ok(())
    }

    // A function's body, where `break` has no loop to leave.
    fn function_block(&mut self) -> Result<()> {
        let loops = std::mem::replace(&mut self.loops, 0);
        let r = self.nested_block();
        self.loops = loops;
        r
    }

    fn metatable(&mut self) -> Result<()> {
        self.push("{");
        let n = self.u.int_in_range(1..=3)?;
        for _ in 0..n {
            self.word(METAMETHODS)?;
            self.push(" = ");
            match self.pick(3)? {
                0 => {
                    self.push("function(...) ");
                    self.function_block()?;
                    self.push(" return ");
                    self.exprlist(2)?;
                    self.push(" end");
                }
                1 => self.push("t"),
                _ => self.expr()?,
            }
            self.push(", ");
        }
        self.push("}");
        Ok(())
    }

    fn var(&mut self) -> Result<()> {
        match self.pick(4)? {
            0 | 1 => self.name(),
            2 => {
                self.name()?;
                self.push("[ ");
                self.expr()?;
                self.push(" ]");
                Ok(())
            }
            _ => {
                self.name()?;
                self.push(".k");
                Ok(())
            }
        }
    }

    fn exprlist(&mut self, max: u32) -> Result<()> {
        let n = self.u.int_in_range(1..=max)?;
        for i in 0..n {
            if i > 0 {
                self.push(", ");
            }
            self.expr()?;
        }
        Ok(())
    }

    fn args(&mut self) -> Result<()> {
        self.push("(");
        if self.chance(4, 5)? {
            self.exprlist(4)?;
        }
        self.push(")");
        Ok(())
    }

    // A call, as a statement if `statement`.
    fn call(&mut self, statement: bool) -> Result<()> {
        match self.pick(5)? {
            0 | 1 => {
                self.word(LIBRARY)?;
                self.args()
            }
            2 => {
                // (A statement starts with a `;`, so that the `(` does not call the end of the
                // previous statement.)
                self.push(if statement { ";(" } else { "(" });
                self.expr()?;
                self.push("):");
                self.word(STRING_METHODS)?;
                self.args()
            }
            3 => {
                self.name()?;
                self.args()
            }
            _ => {
                self.push("t:m");
                self.args()
            }
        }
    }

    fn expr(&mut self) -> Result<()> {
        self.budget = self.budget.saturating_sub(1);
        if self.depth >= MAX_DEPTH || self.budget == 0 {
            return self.atom();
        }
        self.depth += 1;
        let r = match self.pick(14)? {
            0..=3 => self.atom(),
            4 | 5 => {
                self.expr()?;
                self.push(" ");
                self.word(BINOPS)?;
                self.push(" ");
                self.expr()
            }
            6 => {
                self.word(UNOPS)?;
                self.expr()
            }
            7 => {
                self.push("(");
                self.expr()?;
                self.push(")");
                Ok(())
            }
            8 => self.table(),
            9 => {
                self.push("function");
                self.funcbody()
            }
            10 | 11 => self.call(false),
            12 => {
                self.name()?;
                self.push("[ ");
                self.expr()?;
                self.push(" ]");
                Ok(())
            }
            _ => {
                self.push("select(");
                self.expr()?;
                self.push(", ...)");
                Ok(())
            }
        };
        self.depth -= 1;
        r
    }

    fn table(&mut self) -> Result<()> {
        self.push("{");
        let n = self.pick(5)?;
        for _ in 0..n {
            match self.pick(3)? {
                0 => self.expr()?,
                1 => {
                    self.push("[ ");
                    self.expr()?;
                    self.push(" ] = ");
                    self.expr()?;
                }
                _ => {
                    self.name()?;
                    self.push(" = ");
                    self.expr()?;
                }
            }
            let separator = if self.chance(1, 2)? { ", " } else { "; " };
            self.push(separator);
        }
        if self.chance(1, 5)? {
            self.push("...");
        }
        self.push("}");
        Ok(())
    }

    fn atom(&mut self) -> Result<()> {
        match self.pick(9)? {
            0 => self.name(),
            1 => self.word(INTEGERS),
            2 => self.word(FLOATS),
            3 => self.word(STRINGS),
            4 => self.word(CONSTANTS),
            5 => self.word(&["nil", "true", "false"]),
            6 => {
                self.push("...");
                Ok(())
            }
            7 => {
                self.push("#");
                self.name()
            }
            _ => {
                let i = self.u.arbitrary::<i64>()?;
                self.push(&format!("({i})"));
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use lunita::{Closure, Lua};

    /// Most generated programs must compile, or the target only fuzzes the parser.
    #[test]
    fn programs_compile() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut compiled = 0;
        let total = 500;
        for i in 0..total {
            let bytes = (0..4096)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect::<Vec<_>>();
            let mut u = arbitrary::Unstructured::new(&bytes);
            let source = super::program(&mut u, crate::seeds()).unwrap();
            let mut lua = Lua::core();
            let ok = lua.try_enter(|ctx| {
                Closure::load(ctx, None, source.as_bytes())?;
                Ok(())
            });
            match ok {
                Ok(()) => compiled += 1,
                Err(err) if i < 1000 => {
                    if let Ok(dir) = std::env::var("DUMP") {
                        let _ = std::fs::write(format!("{dir}/g{i}.lua"), &source);
                        let _ = std::fs::write(format!("{dir}/g{i}.err"), err.to_string());
                    }
                }
                Err(_) => {}
            }
        }
        assert!(compiled * 10 >= total * 9, "{compiled} of {total} compiled");
    }
}
