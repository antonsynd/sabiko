# Sabiko: notes for Claude Code

Sabiko is a Rust-inspired language compiling to verifiable .NET CIL. The compiler is a Rust
workspace (`crates/`), the runtime library is C# (`runtime/`), and the .NET toolchain lives
in `./tools` (gitignored, installed by `scripts/setup-dotnet.sh`). Start with `docs/PLAN.md`
(phases and status paragraphs), `docs/DECISIONS.md` (numbered decisions; add a row when you
make one) and `docs/SPEC.md` (the language as implemented).

## Commands

```sh
scripts/setup-dotnet.sh && source tools/env.sh   # ilasm, ILVerify, ref assemblies, csc, builds Sabiko.Runtime.dll
cargo build --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings   # pedantic lints are on; CI fails on any warning
cargo test --workspace
cargo run -q -p sabikoc -- file.sabi --emit=exe -o out/x.dll && dotnet out/x.dll
cargo run -q -p sabikoc -- file.sabi --emit=mir|il|ast|pretty|tokens
scripts/build-runtime.sh      # after editing runtime/Sabiko.Runtime/*.cs
scripts/check-runtime.sh      # pins the runtime's formatting and panic behaviour
cargo install cargo-deny --locked --version 0.20.2
cargo deny check              # deny.toml: licences, bans, sources (CI) and advisories (nightly)
python3 benchmarks/run_benchmarks.py [--smoke] [name...]   # Sabiko vs C# runtime/memory (nightly)
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, build and test on Linux, macOS and Windows
with the .NET toolchain installed and `SABIKO_REQUIRE_DOTNET=1`, so the run-pass and
run-time differential tests must run there, not skip. Run all four checks before pushing.
A separate Linux job runs `cargo deny check licenses bans sources`; run it when you add or
update a dependency (a new licence must be added to `deny.toml`'s allow list on purpose).

## Tests and how to update expectations

- UI golden tests: `tests/ui/<stage>/*.sabi` with `//~ ERROR Exxxx msg` annotations and
  `.stderr` files; the directory selects `--stop-after`. `BLESS=1 cargo test -p
  sabiko_driver --test ui` regenerates; review the diff.
- Snapshot tests (insta): MIR in `crates/sabiko_mir/tests`, IL in
  `crates/sabiko_codegen/tests`. `INSTA_UPDATE=always cargo test -p <crate>` accepts;
  delete stray `.snap.new` files.
- Run-pass: `tests/run-pass/**/*.sabi` + `.stdout`, `//@ exit-code: N`, `//@ release`.
  `BLESS=1 cargo test -p sabikoc --test run_pass` writes `.stdout`; read the output and
  check it against Rust semantics, there is no oracle.
- Differential against rustc: `tests/differential/` (compile-time verdicts, `ok_`/`err_`
  names) and `tests/differential/run/` (both compilers build and run the program; stdout and
  exit code must match). Every program must be valid Rust too; the harness prepends a
  prelude defining `Print`, `PrintLine`, `Panic` and `fn main() { Main() }`.
- Generated programs: `crates/sabiko_testgen` + `crates/sabikoc/tests/generated.rs`;
  `SABIKO_GENERATED_SEEDS=400 SABIKO_GENERATED_BASE_SEED=<n>` scales a run; a failure prints
  the seed and the program. A construct the generator must avoid gets a TODO naming the bug.
- Property tests: `PROPTEST_CASES=4096 cargo test -p sabiko_driver --test no_panic`.
- Fuzzing (nightly Rust): `scripts/fuzz.sh pipeline 600`; crashes land in `fuzz/artifacts`.
  Setup for fuzzing and mutation testing, and the Homebrew `cargo`-shadows-rustup pitfall,
  are in `docs/TESTING.md` (Fuzzing, Mutation testing).
- Mutation testing: `scripts/mutants.sh sabiko_borrowck` (slow; thresholds inside);
  triage survivors into tests or the equivalent-mutant list in `docs/TESTING.md`.
- Hand-built MIR unit tests for the borrow checker live in `crates/sabiko_borrowck/tests`.
- Benchmarks (nightly): `benchmarks/<name>/bench.sabi` + `bench.cs`, same output; see
  `benchmarks/README.md`. The nightly fails when a Sabiko benchmark allocates >10% over
  `benchmarks/allocation-baseline.json`; `python3 benchmarks/allocation_gate.py update
  --results <run_benchmarks.py --json output>` changes it, deliberately.
- `docs/TESTING.md` describes every layer; CI runs the fast parts on every push, the
  nightly workflow runs fuzzing, thousands of generated seeds and mutation testing.

## Architecture (one crate per stage)

`sabiko_lexer` → `sabiko_parser` (AST in `sabiko_ast`) → `sabiko_resolve` (DefId/LocalId side
tables; builtins `Print`/`PrintLine`/`Panic` appended after user items) → `sabiko_types`
(inference, item signatures, const eval) → `sabiko_mir` (rustc-like MIR: build, validate,
simplify, pretty) → `sabiko_borrowck` (mutability, init, moves, Polonius loans) →
`sabiko_codegen` (textual IL) → `sabiko_driver` (pipeline, `Options`) → `sabikoc` (CLI,
drives `ilasm`). Diagnostics and error codes are in `sabiko_diagnostics`: E00xx lexer,
E01xx parser (E0107 = not yet supported syntax), E02xx resolve, E03xx typeck, E04xx
borrowck, E05xx codegen (E0500 = not yet supported by codegen), E0900 internal.

## Conventions

- Language: PascalCase items and fields, camelCase locals and parameters; `.sabi` files;
  Sabiko stays a syntactic subset of Rust so rustc can be the oracle.
- The MIR validator runs after every pass (debug builds panic on a violation).
- Only emit IL that passes ILVerify; the single ignored error is `ReturnPtrToStack`
  (decision 30). Never silently miscompile: unsupported constructs are E0500 errors.
- Keep each phase's status paragraph in `docs/PLAN.md` current, and document gaps there.
- Toolchain is pinned in `rust-toolchain.toml`; match the version in CI.
