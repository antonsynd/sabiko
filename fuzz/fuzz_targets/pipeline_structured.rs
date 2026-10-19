//! Whole-pipeline fuzzing on structured input: the fuzzer's bytes drive a small
//! generator that assembles items, statements and expressions from Sabiko lexemes, with
//! raw "token soup" as one of the alternatives at every level. Flat soup almost never
//! parses, so this is what gets the fuzzer past the parser into resolution, type
//! checking, borrow checking and codegen. Only "no panic" is asserted. Run with
//! `cargo +nightly fuzz run pipeline_structured` from `fuzz/`, or
//! `scripts/fuzz.sh pipeline_structured 600`.
#![no_main]

use arbitrary::{Arbitrary, Result, Unstructured};
use libfuzzer_sys::fuzz_target;
use sabiko_driver::{Emit, Options, OutputKind};

/// Every token the lexer knows plus the identifiers and literals the fixtures use, for
/// the raw-soup alternative.
const LEXEMES: &[&str] = &[
    // keywords
    "as",
    "break",
    "const",
    "continue",
    "crate",
    "dyn",
    "else",
    "enum",
    "extern",
    "false",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "match",
    "mod",
    "mut",
    "pub",
    "ref",
    "return",
    "self",
    "Self",
    "static",
    "struct",
    "super",
    "trait",
    "true",
    "type",
    "unsafe",
    "use",
    "where",
    "while",
    "_",
    // punctuation
    "(",
    ")",
    "{",
    "}",
    "[",
    "]",
    ",",
    ";",
    ":",
    "::",
    ".",
    "..",
    "..=",
    "->",
    "=>",
    "=",
    "==",
    "!=",
    "<",
    "<=",
    ">",
    ">=",
    "+",
    "-",
    "*",
    "/",
    "%",
    "!",
    "&&",
    "||",
    "&",
    "|",
    "^",
    "<<",
    ">>",
    "+=",
    "-=",
    "*=",
    "/=",
    "%=",
    "&=",
    "|=",
    "^=",
    "<<=",
    ">>=",
    "#",
    "?",
    "@",
    "~",
    "$",
    // identifiers, types, literals, comments
    "a",
    "b",
    "x",
    "y",
    "p",
    "Main",
    "Point",
    "X",
    "Y",
    "Add",
    "Print",
    "PrintLine",
    "Panic",
    "LIMIT",
    "i32",
    "u8",
    "f64",
    "bool",
    "char",
    "str",
    "0",
    "1",
    "42",
    "1_000u32",
    "0xFF",
    "1.5",
    "2e3",
    "\"s\"",
    "'x'",
    "'a",
    "#[derive(Copy, Clone)]",
    "// c\n",
    "/* c */",
    "\n",
    " ",
];

const LOCALS: &[&str] = &["a", "b", "x", "y", "p", "q", "n", "flag", "s", "c"];
const FUNCS: &[&str] = &["Main", "Add", "Touch", "Make", "Use", "F", "G"];
const BUILTINS: &[&str] = &["Print", "PrintLine", "Panic"];
const STRUCTS: &[&str] = &["Point", "Pair", "Wrap"];
const FIELDS: &[&str] = &["X", "Y", "A", "B", "Inner"];
const CONSTS: &[&str] = &["LIMIT", "RATIO", "NAME"];
const PRIMS: &[&str] = &[
    "i8", "i16", "i32", "i64", "isize", "u8", "u16", "u32", "u64", "usize", "f32", "f64", "bool",
    "char", "&str", "()", "!",
];
const LITERALS: &[&str] = &[
    "0",
    "1",
    "2",
    "42",
    "-1",
    "1_000u32",
    "0xFF",
    "255u8",
    "7i64",
    "3usize",
    "2147483647",
    "2147483648",
    "18446744073709551615",
    "1.5",
    "2e3",
    "0.5f32",
    "true",
    "false",
    "\"s\"",
    "\"text\"",
    "'x'",
    "'\\n'",
];
const BINOPS: &[&str] = &[
    "+", "-", "*", "/", "%", "==", "!=", "<", "<=", ">", ">=", "&&", "||", "&", "|", "^", "<<",
    ">>",
];
const ASSIGN_OPS: &[&str] = &[
    "=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>=",
];

struct Gen<'a, 'b> {
    u: &'a mut Unstructured<'b>,
    out: String,
}

impl Gen<'_, '_> {
    fn push(&mut self, s: &str) {
        self.out.push_str(s);
        self.out.push(' ');
    }

    fn pick(&mut self, items: &[&str]) -> Result<()> {
        let s = *self.u.choose(items)?;
        self.push(s);
        Ok(())
    }

    /// A short run of raw lexemes: the escape hatch that keeps syntax errors and
    /// unexpected shapes reachable from every position.
    fn soup(&mut self) -> Result<()> {
        let n = self.u.int_in_range(0..=6)?;
        for _ in 0..n {
            self.pick(LEXEMES)?;
        }
        Ok(())
    }

    fn ty(&mut self, depth: u8) -> Result<()> {
        match self.u.int_in_range(0..=9)? {
            0..=5 => self.pick(PRIMS),
            6 | 7 => self.pick(STRUCTS),
            8 if depth > 0 => {
                self.push("&");
                if self.u.arbitrary()? {
                    self.push("mut");
                }
                self.ty(depth - 1)
            }
            _ => self.soup(),
        }
    }

    fn expr(&mut self, depth: u8) -> Result<()> {
        if depth == 0 {
            return match self.u.int_in_range(0..=2)? {
                0 => self.pick(LITERALS),
                1 => self.pick(LOCALS),
                _ => self.pick(CONSTS),
            };
        }
        match self.u.int_in_range(0..=19)? {
            0 | 1 => self.pick(LITERALS),
            2 | 3 => self.pick(LOCALS),
            4 => self.pick(CONSTS),
            5 | 6 => {
                self.expr(depth - 1)?;
                self.pick(BINOPS)?;
                self.expr(depth - 1)
            }
            7 => {
                self.pick(&["-", "!", "*", "&", "&mut"])?;
                self.expr(depth - 1)
            }
            8 | 9 => {
                let callee = if self.u.arbitrary()? { FUNCS } else { BUILTINS };
                self.pick(callee)?;
                self.push("(");
                let n = self.u.int_in_range(0..=3)?;
                for i in 0..n {
                    if i > 0 {
                        self.push(",");
                    }
                    self.expr(depth - 1)?;
                }
                self.push(")");
                Ok(())
            }
            10 => {
                self.expr(depth - 1)?;
                self.push(".");
                self.pick(FIELDS)
            }
            11 => {
                self.expr(depth - 1)?;
                self.push("as");
                self.ty(1)
            }
            12 => {
                self.push("(");
                self.expr(depth - 1)?;
                self.push(")");
                Ok(())
            }
            13 => {
                self.pick(STRUCTS)?;
                self.push("{");
                let n = self.u.int_in_range(0..=3)?;
                for i in 0..n {
                    if i > 0 {
                        self.push(",");
                    }
                    self.pick(FIELDS)?;
                    self.push(":");
                    self.expr(depth - 1)?;
                }
                self.push("}");
                Ok(())
            }
            14 => {
                self.push("if");
                self.expr(depth - 1)?;
                self.block(depth - 1)?;
                if self.u.arbitrary()? {
                    self.push("else");
                    self.block(depth - 1)?;
                }
                Ok(())
            }
            15 => self.block(depth - 1),
            16 => {
                self.push("loop");
                self.block(depth - 1)
            }
            17 => {
                self.push("return");
                if self.u.arbitrary()? {
                    self.expr(depth - 1)?;
                }
                Ok(())
            }
            _ => self.soup(),
        }
    }

    fn stmt(&mut self, depth: u8) -> Result<()> {
        match self.u.int_in_range(0..=11)? {
            0..=3 => {
                self.push("let");
                if self.u.arbitrary()? {
                    self.push("mut");
                }
                self.pick(LOCALS)?;
                if self.u.arbitrary()? {
                    self.push(":");
                    self.ty(1)?;
                }
                if self.u.arbitrary()? {
                    self.push("=");
                    self.expr(depth)?;
                }
                self.push(";");
                Ok(())
            }
            4 | 5 => {
                self.expr(depth)?;
                self.push(";");
                Ok(())
            }
            6 => {
                // Assignment to a place: a local, a field or a deref.
                match self.u.int_in_range(0..=2)? {
                    0 => self.pick(LOCALS)?,
                    1 => {
                        self.pick(LOCALS)?;
                        self.push(".");
                        self.pick(FIELDS)?;
                    }
                    _ => {
                        self.push("*");
                        self.pick(LOCALS)?;
                    }
                }
                self.pick(ASSIGN_OPS)?;
                self.expr(depth)?;
                self.push(";");
                Ok(())
            }
            7 => {
                self.push("while");
                self.expr(depth)?;
                self.block(depth)
            }
            8 => {
                self.push("if");
                self.expr(depth)?;
                self.block(depth)?;
                if self.u.arbitrary()? {
                    self.push("else");
                    self.block(depth)?;
                }
                Ok(())
            }
            9 => self.pick(&["break ;", "continue ;", "return ;", "loop { break ; }"]),
            10 => {
                self.expr(depth)?;
                Ok(())
            }
            _ => self.soup(),
        }
    }

    fn block(&mut self, depth: u8) -> Result<()> {
        self.push("{");
        if depth > 0 {
            let n = self.u.int_in_range(0..=4)?;
            for _ in 0..n {
                self.stmt(depth - 1)?;
            }
            if self.u.arbitrary()? {
                self.expr(depth - 1)?;
            }
        }
        self.push("}");
        Ok(())
    }

    fn item(&mut self) -> Result<()> {
        match self.u.int_in_range(0..=9)? {
            0..=4 => {
                self.push("fn");
                self.pick(FUNCS)?;
                self.push("(");
                let n = self.u.int_in_range(0..=3)?;
                for i in 0..n {
                    if i > 0 {
                        self.push(",");
                    }
                    if self.u.arbitrary()? {
                        self.push("mut");
                    }
                    self.pick(LOCALS)?;
                    self.push(":");
                    self.ty(1)?;
                }
                self.push(")");
                if self.u.arbitrary()? {
                    self.push("->");
                    self.ty(1)?;
                }
                self.block(3)
            }
            5 | 6 => {
                if self.u.arbitrary()? {
                    self.push("#[derive(Copy, Clone)]");
                }
                self.push("struct");
                self.pick(STRUCTS)?;
                match self.u.int_in_range(0..=2)? {
                    0 => self.push(";"),
                    _ => {
                        self.push("{");
                        let n = self.u.int_in_range(0..=4)?;
                        for i in 0..n {
                            if i > 0 {
                                self.push(",");
                            }
                            self.pick(FIELDS)?;
                            self.push(":");
                            self.ty(1)?;
                        }
                        self.push("}");
                    }
                }
                Ok(())
            }
            7 => {
                self.push("const");
                self.pick(CONSTS)?;
                self.push(":");
                self.ty(0)?;
                self.push("=");
                self.expr(1)?;
                self.push(";");
                Ok(())
            }
            _ => self.soup(),
        }
    }
}

#[derive(Debug)]
struct Program(String);

impl<'a> Arbitrary<'a> for Program {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        let mut g = Gen {
            u,
            out: String::new(),
        };
        // Running out of bytes ends generation with whatever was built so far: that is
        // still a program worth compiling, and keeps every byte prefix meaningful.
        let n = g.u.int_in_range(1..=6).unwrap_or(1);
        for _ in 0..n {
            if g.item().is_err() || g.out.len() > 8192 {
                break;
            }
            g.out.push('\n');
        }
        Ok(Self(g.out))
    }
}

fuzz_target!(|program: Program| {
    // `SABIKO_FUZZ_PRINT=1` shows the program a crash artifact decodes to
    // (`scripts/fuzz.sh` does this for every reproducer).
    if std::env::var_os("SABIKO_FUZZ_PRINT").is_some() {
        eprintln!("---- program ----\n{}\n---- end ----", program.0);
    }
    let il = Options {
        emit: Emit::Il,
        output_kind: OutputKind::Library,
        ..Options::default()
    };
    let _ = sabiko_driver::compile_str("fuzz.sabi", &program.0, il);
    let mir = Options {
        emit: Emit::Mir,
        ..Options::default()
    };
    let _ = sabiko_driver::compile_str("fuzz.sabi", &program.0, mir);
});
