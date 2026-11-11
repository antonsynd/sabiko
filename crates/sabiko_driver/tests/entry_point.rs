//! An executable's entry point: a file with no free function `Main` gets rustc's E0601
//! (Sabiko's E0501, "an executable needs a `Main` function"), which points nowhere, so the
//! UI harness cannot annotate it. Items named `Main` that are not free functions, and
//! free functions with other names, do not count as one (`entry_point_or_error`).

use sabiko_driver::{compile_str, Emit, Options, OutputKind};

fn errors(src: &str) -> Vec<String> {
    let c = compile_str(
        "entry.sabi",
        src,
        Options {
            emit: Emit::Il,
            output_kind: OutputKind::Executable,
            ..Options::default()
        },
    );
    c.diagnostics
        .iter()
        .map(|d| format!("{} {}", d.code, d.message))
        .collect()
}

const MISSING: &str = "E0501 an executable needs a `Main` function";

#[test]
fn no_items() {
    assert_eq!(errors(""), [MISSING]);
}

#[test]
fn other_free_functions() {
    assert_eq!(errors("fn Helper() {}\nfn Other<T>(t: T) {}\n"), [MISSING]);
}

#[test]
fn items_named_main_that_are_not_free_functions() {
    let src = "struct Main;\n\
               struct S;\n\
               impl S {\n    fn Main() {}\n}\n\
               trait T {\n    fn Main();\n}\n\
               impl T for i32 {\n    fn Main() {}\n}\n";
    assert_eq!(errors(src), [MISSING]);
}

#[test]
fn library_needs_no_main() {
    let c = compile_str(
        "lib.sabi",
        "fn Helper() {}\n",
        Options {
            emit: Emit::Il,
            output_kind: OutputKind::Library,
            ..Options::default()
        },
    );
    assert!(c.diagnostics.is_empty(), "{:?}", c.diagnostics);
}
