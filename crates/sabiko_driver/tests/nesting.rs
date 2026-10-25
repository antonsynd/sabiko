//! The parser's nesting limit, and the pipeline's stack at that limit. Every later stage
//! recurses over the syntax tree, so a program nested exactly `MAX_NESTING` deep must
//! get through all of them, and one level more must be a single E0109 rather than a
//! stack overflow (nightly fuzzing found the overflow with ~1900 nested blocks).

use sabiko_diagnostics::ErrorCode;
use sabiko_driver::{compile_str, Compilation, Emit, Options, OutputKind};
use sabiko_parser::MAX_NESTING;

struct Shape {
    name: &'static str,
    /// Levels the program has besides its `n` repetitions (the `let` initialiser is one).
    base: u32,
    /// Whether the program at the limit is valid, so that it reaches code generation.
    valid: bool,
    program: fn(usize) -> String,
}

/// Each repetition is one level, and `&` in a type or expression counts once (`&&` lexes
/// as one token but counts twice).
const SHAPES: &[Shape] = &[
    Shape {
        name: "parentheses",
        base: 1,
        valid: true,
        program: |n| {
            format!(
                "fn Main() {{ let x = {}0{}; }}",
                "(".repeat(n),
                ")".repeat(n)
            )
        },
    },
    Shape {
        name: "blocks",
        base: 1,
        valid: true,
        program: |n| {
            format!(
                "fn Main() {{ let x = {}0{}; }}",
                "{".repeat(n),
                "}".repeat(n)
            )
        },
    },
    Shape {
        name: "negations",
        base: 1,
        valid: true,
        program: |n| format!("fn Main() {{ let x = {}1; }}", "-".repeat(n)),
    },
    Shape {
        name: "nots",
        base: 1,
        valid: true,
        program: |n| format!("fn Main() {{ let x = {}true; }}", "!".repeat(n)),
    },
    Shape {
        name: "borrows",
        base: 1,
        valid: false,
        program: |n| format!("fn Main() {{ let x = {}1; }}", "&".repeat(n)),
    },
    Shape {
        name: "binary chain",
        base: 1,
        valid: true,
        program: |n| format!("fn Main() {{ let x = 0{}; }}", " + 1".repeat(n)),
    },
    Shape {
        name: "cast chain",
        base: 1,
        valid: true,
        program: |n| format!("fn Main() {{ let x = 0{}; }}", " as i32".repeat(n)),
    },
    Shape {
        name: "calls",
        base: 1,
        valid: true,
        program: |n| {
            format!(
                "fn F(x: i32) -> i32 {{ x }} fn Main() {{ let x = {}0{}; }}",
                "F(".repeat(n),
                ")".repeat(n)
            )
        },
    },
    Shape {
        name: "field chain",
        base: 1,
        valid: false,
        program: |n| {
            format!(
                "struct S {{ A: i32 }} fn Main() {{ let s = S {{ A: 1 }}; let x = s{}; }}",
                ".A".repeat(n)
            )
        },
    },
    Shape {
        name: "index chain",
        base: 1,
        valid: false,
        program: |n| format!("fn Main() {{ let x = a{}; }}", "[0]".repeat(n)),
    },
    Shape {
        name: "else if chain",
        // The last `else if`'s condition is one level below it.
        base: 2,
        valid: true,
        program: |n| {
            format!(
                "fn Main() {{ let c = true; let x = if c {{}}{}; }}",
                " else if c {}".repeat(n)
            )
        },
    },
    Shape {
        name: "nested ifs",
        base: 1,
        valid: true,
        program: |n| {
            format!(
                "fn Main() {{ let c = true; let x = {}{}; }}",
                "if c { ".repeat(n),
                "} ".repeat(n)
            )
        },
    },
    Shape {
        name: "nested whiles",
        base: 1,
        valid: true,
        program: |n| {
            format!(
                "fn Main() {{ let c = false; let x = {}{}; }}",
                "while c { ".repeat(n),
                "} ".repeat(n)
            )
        },
    },
    Shape {
        name: "returns",
        base: 1,
        valid: true,
        program: |n| format!("fn Main() {{ let x = {}(); }}", "return ".repeat(n)),
    },
    Shape {
        name: "assignments",
        base: 1,
        valid: false,
        program: |n| {
            format!(
                "fn Main() {{ let mut y = 0; let x = {}0; }}",
                "y = ".repeat(n)
            )
        },
    },
    Shape {
        name: "reference types",
        base: 0,
        valid: false,
        program: |n| format!("fn Main() {{ let x: {}i32; }}", "&".repeat(n)),
    },
];

fn compile(src: &str) -> Compilation {
    let options = Options {
        emit: Emit::Il,
        output_kind: OutputKind::Library,
        ..Options::default()
    };
    compile_str("nesting.sabi", src, options)
}

#[test]
fn programs_at_the_limit_compile() {
    for shape in SHAPES {
        let n = usize::try_from(MAX_NESTING - shape.base).unwrap();
        let src = (shape.program)(n);
        let c = compile(&src);
        let codes: Vec<_> = c.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            !codes.contains(&ErrorCode::NESTING_TOO_DEEP),
            "{}: {n} repetitions are within the limit",
            shape.name
        );
        if shape.valid {
            assert!(
                !c.has_errors() && !c.output.is_empty(),
                "{}: expected IL, got {}",
                shape.name,
                c.render_diagnostics(sabiko_diagnostics::Format::Human)
            );
        }
    }
}

#[test]
fn one_level_past_the_limit_is_one_error() {
    for shape in SHAPES {
        let n = usize::try_from(MAX_NESTING - shape.base + 1).unwrap();
        let c = compile(&(shape.program)(n));
        let codes: Vec<_> = c.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, [ErrorCode::NESTING_TOO_DEEP], "{}", shape.name);
    }
}

/// Far past the limit the parser stops at the limit, so the cost does not grow with the
/// input (the fuzzer's reproducer nested ~1900 blocks).
#[test]
fn deep_nesting_is_one_error() {
    let n = 100_000;
    let src = format!(
        "fn Main() {{ let x = {}0{}; }}",
        "{".repeat(n),
        "}".repeat(n)
    );
    let c = compile(&src);
    let codes: Vec<_> = c.diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(codes, [ErrorCode::NESTING_TOO_DEEP]);
}
