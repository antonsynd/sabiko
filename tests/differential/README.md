# Differential tests against rustc

Each `*.sabi` file here is a program that is valid Rust syntax as well as Sabiko (Sabiko is
a syntactic subset of Rust apart from .NET casing). `crates/sabiko_driver/tests/differential.rs`
compiles every file with both compilers and classifies each verdict:

- `accept`: no errors;
- `borrow-error`: errors, all of them from the borrow checker (Sabiko E0400–E0417; rustc
  E0381/E0382/E0384, E0499–E0507, E0515, E0594–E0597, E0106, E0716, and the uncoded
  "lifetime may not live long enough");
- `OTHER-ERROR`: anything else. This always fails the test: the corpus must be valid in
  both languages up to borrow checking, so a parse, resolution or type error in either
  compiler means the program (or, rarely, a compiler) needs fixing.

The test passes when the two verdicts agree for every file. A file named `ok_*` must be
`accept` in both; `err_*` must be `borrow-error` in both. Error codes are reported but not
compared (the two compilers number their diagnostics differently), nor are positions. One
concept per file, under ~25 lines, a one-line comment at the top, `ok_`/`err_` pairs side
by side. Directories group the rules: `moves/`, `loans/`, `mutability/`, `builtins/`, and
`never/` (borrows of diverging expressions, typed `&!`).

The harness prepends this prelude, which defines Sabiko's builtin functions for rustc:

```rust
#![allow(nonstandard_style, unused, dead_code)]
fn Print<T: std::fmt::Display>(x: T) { print!("{x}") }
fn PrintLine<T: std::fmt::Display>(x: T) { println!("{x}") }
fn Panic(msg: &str) -> ! { panic!("{msg}") }
```

and a blank line, then runs `rustc --edition 2021 --crate-type lib --emit=metadata
-A warnings` in a scratch directory under the system temp dir, which it removes
afterwards. It uses `$RUSTC` if set, else `rustc` on `PATH`; when neither runs, the test
prints a notice and is skipped.

Restrictions of the shared subset today: `fn`, `struct` with named fields,
`#[derive(Copy, Clone)]`, `let`/`let mut`, `if`/`else`, `while`, `loop`, `break`,
`continue`, `return`, struct literals, field access, `&`/`&mut`/`*`, `as`, literals, and
the builtins `Print(x)`, `PrintLine(x)` (any integer, float, `bool`, `char` or `&str`) and
`Panic(msg: &str) -> !`. A corpus program must not define its own `Print`, `PrintLine` or
`Panic`: Sabiko lets a user item shadow the builtin, rustc sees a duplicate of the
prelude's. No methods, traits, generics, tuples, arrays, enums, `match`, closures,
modules, macros, explicit lifetimes or references in struct fields, and no `char` literal
above U+FFFF (a Sabiko `char` is one UTF-16 code unit, so those are E0318). Function names are
PascalCase, locals camelCase. A function returning a reference must have exactly one
reference parameter (the one elision rule both compilers share).

Run: `cargo test -p sabiko_driver --test differential`. The one-line summary per file is
on stderr (`-- --nocapture` to see it when the test passes).

## Run-time differential tests (`run/`)

Programs under `run/` have a `fn Main()` and print. `crates/sabikoc/tests/differential_run.rs`
builds each one as an executable with both compilers (rustc gets the prelude plus
`fn main() { Main() }`; sabikoc compiles in debug mode, matching rustc's default overflow
checks), runs both, and requires identical stdout and exit codes (both compilers exit with
101 on a panic, including arithmetic overflow). Standard error is compared too:

- after a clean exit (code 0) neither program may have written to stderr;
- after a panic (code 101) the panic *message* lines must match. rustc prints
  `thread 'main' panicked at src/main.rs:L:C:` (newer versions insert the thread id) and
  Sabiko `thread 'main' panicked:`; the message is the next line in both, and the harness
  runs the Rust binary with `RUST_BACKTRACE=0` so nothing follows it. One difference is
  accepted: Sabiko reports every overflow of `+`, `-`, `*` and unary `-` as "attempt to
  perform arithmetic with overflow" because the CLR's `.ovf` opcodes do not say which
  operator overflowed, where rustc says "attempt to add/subtract/multiply/negate with
  overflow". Division, remainder, shift and `Panic` messages must be identical.

The test is skipped with a notice when `rustc` or the .NET toolchain is missing
(`SABIKO_REQUIRE_RUSTC=1` / `SABIKO_REQUIRE_DOTNET=1` turn the skip into a failure). The
helpers (locating rustc, running both programs, comparing runs, classifying verdicts) live
in `crates/sabikoc/tests/common/mod.rs` and are shared with the generated tests below.

## Generated programs (`crates/sabiko_testgen`, `crates/sabikoc/tests/generated.rs`)

The hand-written corpus is complemented by random programs from `sabiko_testgen`:
`generate(seed, &Knobs)` is a deterministic function of a `u64` seed and a knob struct
(number of functions, structs and constants, line budget, expression and block depth, and a
violation percentage). Every program is valid in both languages up to borrow checking and
prints its results, so both oracles above apply to it. The subset is the one listed above
plus `const` items, nested field paths, `break value`, compound assignment and casts between
every integer type but `i128`/`u128`, floats, `bool` and ASCII `char`; see the crate
documentation for the exact list and the exclusions. `Knobs::default()` lets a few per cent
of the generator's borrow-model checks be skipped so that some programs contain a use after
move, a conflicting borrow, an assignment to an immutable binding or a write through `&`;
`Knobs::borrow_clean()` never skips a check.

`generated.rs` checks three properties, each over `SABIKO_GENERATED_SEEDS` seeds starting
at `SABIKO_GENERATED_BASE_SEED` (default 1):

1. compile-time verdicts agree with rustc (default knobs; 60 seeds by default), with the
   same classification as the corpus test: any non-borrow error on either side fails;
2. borrow-clean programs run identically (stdout, exit code, panic message; 40 seeds by
   default), built with `rustc --crate-type bin` and `sabikoc --emit=exe` plus ILVerify.
   rustc gets the prelude above with `arithmetic_overflow` and `unconditional_panic` added
   to the `allow` attribute, so a constant-visible overflow or division by zero compiles and
   panics at run time on both sides;
3. the Sabiko pipeline is deterministic (compiling twice gives identical diagnostics, MIR
   and IL) and idempotent under no-op insertion: `insert_noops` adds `let _ = ();`, `{}`
   and `();` at random statement boundaries and the error codes and verdict must not
   change (60 seeds by default).

A failure prints the seed, the program and both compilers' output, so
`sabiko_testgen::generate(seed, &knobs)` reproduces it; the nightly workflow runs thousands
of seeds. When a disagreement is a compiler bug, the generator avoids the construct behind a
`TODO` naming the bug until it is fixed (`grep TODO crates/sabiko_testgen/src`), so that the
committed tests stay green. The crate's own tests check that 500 seeds parse, resolve and
type-check, that borrow-clean programs pass the whole pipeline, and that generation is
deterministic.
