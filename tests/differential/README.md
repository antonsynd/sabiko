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
by side. Directories group the rules: `moves/`, `loans/`, `mutability/`.

The harness prepends `#![allow(nonstandard_style, unused, dead_code)]` and a blank line,
then runs `rustc --edition 2021 --crate-type lib --emit=metadata -A warnings` in a scratch
directory under the system temp dir, which it removes afterwards. It uses `$RUSTC` if set,
else `rustc` on `PATH`; when neither runs, the test prints a notice and is skipped.

Restrictions of the shared subset today: `fn`, `struct` with named fields,
`#[derive(Copy, Clone)]`, `let`/`let mut`, `if`/`else`, `while`, `loop`, `break`,
`continue`, `return`, struct literals, field access, `&`/`&mut`/`*`, `as`, literals. No
methods, traits, generics, tuples, arrays, enums, `match`, closures, modules, macros,
explicit lifetimes or references in struct fields. Function names are PascalCase, locals
camelCase. A function returning a reference must have exactly one reference parameter
(the one elision rule both compilers share).

Run: `cargo test -p sabiko_driver --test differential`. The one-line summary per file is
on stderr (`-- --nocapture` to see it when the test passes).
