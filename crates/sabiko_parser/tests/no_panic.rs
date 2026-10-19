//! The parser must never panic, whatever the input. Two input distributions: arbitrary
//! text, and "token soup" assembled from Sabiko's own lexemes, which reaches far deeper
//! into the grammar than random bytes. Accepted programs must also round-trip through the
//! pretty printer. This complements the libFuzzer target in `fuzz/`, which needs nightly.

use proptest::prelude::*;
use sabiko_diagnostics::DiagnosticSink;
use sabiko_span::FileId;

const LEXEMES: &[&str] = &[
    "fn",
    "let",
    "mut",
    "struct",
    "const",
    "pub",
    "if",
    "else",
    "while",
    "loop",
    "break",
    "continue",
    "return",
    "as",
    "true",
    "false",
    "_",
    "enum",
    "match",
    "impl",
    "self",
    "Self",
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
    "->",
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
    "#",
    "?",
    "'a",
    "'a'",
    "x",
    "Foo",
    "i32",
    "0",
    "1_000u32",
    "0xFF",
    "1.5",
    "2e3",
    "\"s\"",
    "\"unterminated",
    "/* c */",
    "/* open",
    "// line\n",
    "/// doc\n",
    "//! inner\n",
    "\n",
    " ",
];

fn check(src: &str) {
    let mut sink = DiagnosticSink::new();
    let file = sabiko_parser::parse_file(FileId(0), src, &mut sink);
    if !sink.has_errors() {
        let printed = sabiko_ast::pretty::pretty(&file);
        let mut sink2 = DiagnosticSink::new();
        let reparsed = sabiko_parser::parse_file(FileId(0), &printed, &mut sink2);
        assert!(
            !sink2.has_errors(),
            "pretty output failed to parse:\n{printed}\n{:#?}",
            sink2.diagnostics()
        );
        assert_eq!(
            sabiko_ast::pretty::pretty(&reparsed),
            printed,
            "pretty is not a fixpoint for:\n{src}"
        );
    }
    // Every diagnostic must point into the file.
    for d in sink.diagnostics() {
        for l in &d.labels {
            assert!(
                l.span.end as usize <= src.len(),
                "label span out of range: {d:?}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_text_never_panics(src in ".*") {
        check(&src);
    }

    #[test]
    fn token_soup_never_panics(parts in prop::collection::vec(prop::sample::select(LEXEMES), 0..40)) {
        let src = parts.join(" ");
        check(&src);
    }
}
