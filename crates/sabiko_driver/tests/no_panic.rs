//! The whole pipeline must never panic, whatever the input. Three input distributions,
//! each reaching deeper than the last: arbitrary text, "token soup" assembled from
//! Sabiko's own lexemes (which parses often enough to exercise resolution and type
//! checking), and random edits to the real test corpus (nearly valid programs, which
//! reach borrow checking and codegen). Accepted programs must produce IL.
//!
//! The default case count keeps this well under a minute; the nightly workflow sets
//! `PROPTEST_CASES` to run far more. The libFuzzer targets in `fuzz/` cover the same
//! ground with coverage guidance but need a nightly toolchain.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use proptest::prelude::*;
use proptest::test_runner::RngSeed;
use sabiko_driver::{compile_str, Compilation, Emit, Options, OutputKind};
use sabiko_lexer::TokenKind;

/// Every keyword and punctuation token, plus identifiers, literals of each kind and
/// comments, so that programs like `fn Main() { let p = Point { X: 1, Y: 2 }; PrintLine(p.X); }`
/// come up now and then.
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
    // identifiers: locals, items, fields, builtins, types
    "a",
    "b",
    "x",
    "y",
    "p",
    "flag",
    "Main",
    "Point",
    "Foo",
    "X",
    "Y",
    "Add",
    "Touch",
    "Print",
    "PrintLine",
    "Panic",
    "LIMIT",
    "i8",
    "i16",
    "i32",
    "i64",
    "isize",
    "u8",
    "u16",
    "u32",
    "u64",
    "usize",
    "f32",
    "f64",
    "bool",
    "char",
    "str",
    // literals of each kind
    "0",
    "1",
    "2",
    "42",
    "1_000u32",
    "0xFF",
    "255u8",
    "7i64",
    "1.5",
    "2e3",
    "0.5f32",
    "18446744073709551615",
    "\"s\"",
    "\"unterminated",
    "'x'",
    "'\\n'",
    "'a",
    // attributes and comments
    "#[derive(Copy, Clone)]",
    "// c\n",
    "/* c */",
    "/* open",
    // layout
    "\n",
    " ",
];

/// Replacement literals for the "replace a literal" edit: one of each kind, including
/// values that overflow or fail to type-check in most positions.
const LITERALS: &[&str] = &[
    "0",
    "1",
    "-1",
    "2147483647",
    "2147483648",
    "18446744073709551615",
    "0xFF",
    "1u8",
    "7i64",
    "3usize",
    "1.5",
    "2e3",
    "0.5f32",
    "true",
    "false",
    "\"s\"",
    "'x'",
];

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

/// A fixed seed unless `PROPTEST_RNG_SEED` is set (the nightly workflow sets it to
/// `random`): a PR run must not go red on a compiler bug that only a lucky seed finds,
/// and a failure must reproduce on the next run. The nightly run explores.
fn seed() -> RngSeed {
    if std::env::var_os("PROPTEST_RNG_SEED").is_some() {
        ProptestConfig::default().rng_seed
    } else {
        RngSeed::Fixed(0x5ab1_c0de)
    }
}

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: cases(),
        rng_seed: seed(),
        // Shrinking a failing corpus mutation reruns the compiler on each candidate; a
        // few hundred steps is plenty to get a readable reproducer.
        max_shrink_iters: 512,
        // Integration tests have no `lib.rs` for proptest's regression files to sit
        // beside; the failure report prints the input anyway.
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// Runs the pipeline to IL (and, since it is a separate code path over the same bodies,
/// the MIR printer). Returns the IL compilation.
fn compile(src: &str) -> Compilation {
    let mir = Options {
        emit: Emit::Mir,
        ..Options::default()
    };
    let _ = compile_str("prop.sabi", src, mir);
    let il = Options {
        emit: Emit::Il,
        output_kind: OutputKind::Library,
        ..Options::default()
    };
    compile_str("prop.sabi", src, il)
}

fn check(src: &str) {
    let c = compile(src);
    // Every diagnostic must point into the file.
    for d in &c.diagnostics {
        for l in &d.labels {
            assert!(
                l.span.end as usize <= src.len(),
                "label span out of range: {d:?}"
            );
        }
    }
    if !c.has_errors() {
        assert!(
            !c.output.is_empty(),
            "accepted program produced no IL:\n{src}"
        );
    }
}

// ---- corpus mutations ---------------------------------------------------------------------

/// Every `.sabi` under `tests/` and `crates/*/tests/fixtures`, collected once.
fn corpus() -> &'static [(PathBuf, String)] {
    static CORPUS: OnceLock<Vec<(PathBuf, String)>> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        collect_sabi(&root.join("tests"), &mut files);
        if let Ok(crates) = std::fs::read_dir(root.join("crates")) {
            for entry in crates.flatten() {
                collect_sabi(&entry.path().join("tests/fixtures"), &mut files);
            }
        }
        files.sort();
        assert!(
            !files.is_empty(),
            "no .sabi files found under {}",
            root.display()
        );
        files
            .into_iter()
            .map(|p| {
                let src = std::fs::read_to_string(&p).unwrap();
                (p, src)
            })
            .collect()
    })
}

fn collect_sabi(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sabi(&path, out);
        } else if path.extension().is_some_and(|e| e == "sabi") {
            out.push(path);
        }
    }
}

/// One random edit. The indices are raw and reduced modulo the relevant count when the
/// edit is applied, so a strategy does not need to know the file's shape in advance.
#[derive(Clone, Copy, Debug)]
enum Edit {
    DeleteToken(usize),
    DuplicateLine(usize),
    SwapLines(usize, usize),
    /// Replace the identifier at the first index with the one at the second.
    ReplaceIdent(usize, usize),
    ReplaceLiteral(usize, usize),
    /// `&` becomes `&mut` and `&mut` becomes `&`.
    FlipRef(usize),
    DerefIdent(usize),
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    prop_oneof![
        any::<usize>().prop_map(Edit::DeleteToken),
        any::<usize>().prop_map(Edit::DuplicateLine),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Edit::SwapLines(a, b)),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Edit::ReplaceIdent(a, b)),
        (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Edit::ReplaceLiteral(a, b)),
        any::<usize>().prop_map(Edit::FlipRef),
        any::<usize>().prop_map(Edit::DerefIdent),
    ]
}

/// Token texts and byte ranges of `src`, comments excluded (lexical errors are ignored:
/// a mutated file may well have them, and the pipeline must cope anyway).
fn tokens(src: &str) -> Vec<(TokenKind, std::ops::Range<usize>)> {
    let mut sink = sabiko_diagnostics::DiagnosticSink::new();
    sabiko_lexer::lex(sabiko_span::FileId(0), src, &mut sink)
        .into_iter()
        .filter(|t| {
            !matches!(
                t.kind,
                TokenKind::Eof
                    | TokenKind::DocComment
                    | TokenKind::InnerDocComment
                    | TokenKind::BlockComment
            )
        })
        .map(|t| (t.kind, t.span.range()))
        .collect()
}

fn pick<T: Clone>(items: &[T], raw: usize) -> Option<T> {
    if items.is_empty() {
        None
    } else {
        Some(items[raw % items.len()].clone())
    }
}

fn apply(src: &str, edit: Edit) -> String {
    let toks = tokens(src);
    let of_kind = |kind: TokenKind| -> Vec<std::ops::Range<usize>> {
        toks.iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, r)| r.clone())
            .collect()
    };
    let splice = |range: std::ops::Range<usize>, with: &str| -> String {
        let mut s = String::with_capacity(src.len() + with.len());
        s.push_str(&src[..range.start]);
        s.push_str(with);
        s.push_str(&src[range.end..]);
        s
    };
    match edit {
        Edit::DeleteToken(i) => match pick(&toks, i) {
            Some((_, r)) => splice(r, ""),
            None => src.to_owned(),
        },
        Edit::DuplicateLine(i) => {
            let mut lines: Vec<&str> = src.lines().collect();
            if let Some(line) = pick(&lines, i) {
                lines.insert(i % lines.len(), line);
            }
            lines.join("\n")
        }
        Edit::SwapLines(a, b) => {
            let mut lines: Vec<&str> = src.lines().collect();
            if !lines.is_empty() {
                let n = lines.len();
                lines.swap(a % n, b % n);
            }
            lines.join("\n")
        }
        Edit::ReplaceIdent(at, with) => {
            let idents = of_kind(TokenKind::Ident);
            match (pick(&idents, at), pick(&idents, with)) {
                (Some(target), Some(source)) => {
                    let text = src[source].to_owned();
                    splice(target, &text)
                }
                _ => src.to_owned(),
            }
        }
        Edit::ReplaceLiteral(at, with) => {
            let lits: Vec<_> = toks
                .iter()
                .filter(|(k, _)| {
                    matches!(
                        k,
                        TokenKind::Int
                            | TokenKind::Float
                            | TokenKind::Str
                            | TokenKind::Char
                            | TokenKind::True
                            | TokenKind::False
                    )
                })
                .map(|(_, r)| r.clone())
                .collect();
            match pick(&lits, at) {
                Some(target) => splice(target, LITERALS[with % LITERALS.len()]),
                None => src.to_owned(),
            }
        }
        Edit::FlipRef(i) => {
            let amps: Vec<usize> = toks
                .iter()
                .enumerate()
                .filter(|(_, (k, _))| *k == TokenKind::Amp)
                .map(|(idx, _)| idx)
                .collect();
            let Some(idx) = pick(&amps, i) else {
                return src.to_owned();
            };
            let amp = toks[idx].1.clone();
            match toks.get(idx + 1) {
                Some((TokenKind::Mut, mut_range)) => splice(amp.start..mut_range.end, "&"),
                _ => splice(amp, "&mut "),
            }
        }
        Edit::DerefIdent(i) => match pick(&of_kind(TokenKind::Ident), i) {
            Some(r) => splice(r.start..r.start, "*"),
            None => src.to_owned(),
        },
    }
}

fn mutate(src: &str, edits: &[Edit]) -> String {
    edits
        .iter()
        .fold(src.to_owned(), |acc, &edit| apply(&acc, edit))
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn arbitrary_text_never_panics(src in ".*") {
        check(&src);
    }

    #[test]
    fn token_soup_never_panics(
        parts in prop::collection::vec(prop::sample::select(LEXEMES), 0..60)
    ) {
        check(&parts.join(" "));
    }

    #[test]
    fn corpus_mutations_never_panic(
        file in any::<prop::sample::Index>(),
        edits in prop::collection::vec(edit_strategy(), 1..=4),
    ) {
        let (path, src) = &corpus()[file.index(corpus().len())];
        let mutated = mutate(src, &edits);
        // Catch the panic so the report carries the mutated program, which is the
        // reproducer, instead of only the corpus index and the edits.
        let Ok(c) = std::panic::catch_unwind(|| compile(&mutated)) else {
            prop_assert!(
                false,
                "compiler panicked on {} with {edits:?}:\n----\n{mutated}\n----",
                path.display()
            );
            unreachable!()
        };
        if !c.has_errors() {
            prop_assert!(
                !c.output.is_empty(),
                "accepted program produced no IL ({} with {edits:?}):\n{mutated}",
                path.display()
            );
        }
    }
}

/// The unmodified corpus goes through the same path, so a fixture that stops compiling
/// is caught here with its name rather than through a mutation.
#[test]
fn unmodified_corpus_never_panics() {
    for (path, src) in corpus() {
        let c = compile(src);
        if !c.has_errors() {
            assert!(
                !c.output.is_empty(),
                "accepted program produced no IL: {}",
                path.display()
            );
        }
    }
}

/// Each edit kind does what its name says on a small program.
#[test]
fn edits_apply() {
    let src = "fn F(a: &i32) -> i32 {\n    let b = *a + 1;\n    b\n}\n";
    assert_eq!(
        apply(src, Edit::FlipRef(0)),
        "fn F(a: &mut i32) -> i32 {\n    let b = *a + 1;\n    b\n}\n"
    );
    assert_eq!(
        apply(&apply(src, Edit::FlipRef(0)), Edit::FlipRef(0)),
        "fn F(a: & i32) -> i32 {\n    let b = *a + 1;\n    b\n}\n"
    );
    assert_eq!(
        apply(src, Edit::DeleteToken(0)),
        " F(a: &i32) -> i32 {\n    let b = *a + 1;\n    b\n}\n"
    );
    assert_eq!(
        apply(src, Edit::DerefIdent(1)),
        "fn F(*a: &i32) -> i32 {\n    let b = *a + 1;\n    b\n}\n"
    );
    assert_eq!(
        apply(src, Edit::ReplaceIdent(0, 1)),
        "fn a(a: &i32) -> i32 {\n    let b = *a + 1;\n    b\n}\n"
    );
    assert_eq!(
        apply(src, Edit::ReplaceLiteral(0, 2)),
        "fn F(a: &i32) -> i32 {\n    let b = *a + -1;\n    b\n}\n"
    );
    assert_eq!(
        apply(src, Edit::SwapLines(1, 2)),
        "fn F(a: &i32) -> i32 {\n    b\n    let b = *a + 1;\n}"
    );
    assert_eq!(
        apply(src, Edit::DuplicateLine(2)),
        "fn F(a: &i32) -> i32 {\n    let b = *a + 1;\n    b\n    b\n}"
    );
    assert_eq!(apply("", Edit::DeleteToken(3)), "");
    assert_eq!(apply("", Edit::SwapLines(1, 2)), "");
}
