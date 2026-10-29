# Sabiko implementation plan

**Sabiko** (錆 *sabi* "rust" + 子 *-ko* "child"): a small Rust-inspired language that
compiles to verifiable CIL, enforces ownership, borrowing and mutability at compile
time, frees Sabiko-owned memory deterministically without the GC, and interoperates
with C# with zero marshalling.

This document is the working plan: architecture, phases with exit criteria, and the
verification/testing strategy. It consolidates the two design notes this project
started from (*Designing a Borrow-Checked CIL Language* and *Designing Ferrite*).

---

## 1. Design decisions (locked in unless revisited)

| Area | Decision | Why |
| --- | --- | --- |
| Implementation language | Rust (`cargo` workspace) | Matches the Ferrite note; repo is already set up for Rust. |
| Compile target | ECMA-335 CIL, .NET 8+ | Verifiable IL; CLR enforces byref safety and bounds checks for free. |
| Backend v1 | Emit textual `.il`, assemble with `ilasm` (`Microsoft.NETCore.ILAsm` NuGet) | Human-readable output, trivial golden tests, fastest bring-up. |
| Backend v2 | Direct PE/metadata writer in Rust | Removes the `ilasm` dependency; only after v1 is stable. |
| Borrow checking | On a MIR (control-flow graph), NLL-style, flow-sensitive | Same architecture as rustc; borrow checking on an AST does not scale. |
| Structs | CIL value types, `[StructLayout(Sequential)]` | Stack allocated, byte-identical layout to C# structs. |
| `&T` / `&mut T` | CIL managed pointers (`T&`); `&T` emitted as `in T` (`IsReadOnlyAttribute`), `&mut T` as `ref T` | Zero-cost, C# sees natural `in`/`ref` parameters. |
| Structs holding references | Emitted as `ref struct` (`IsByRefLikeAttribute`) | CLR then guarantees they never escape to the heap. |
| Heap ownership | `Box<T>` over `NativeMemory.Alloc/Free`, requires `T: unmanaged` | Deterministic free, no GC tracking. |
| .NET reference types | Opaque `Gc<T>` handle, borrow-checked at the handle level | Explicit opt-in to GC-managed memory for interop only. |
| Strings | `&str` = `ReadOnlySpan<char>` (Phase 5); owned `String` = native UTF-16 buffer freed deterministically (Phase 7); `System.String` only as `Gc<System::String>` with zero-copy `.as_str()` (Phase 7) | Keeps every struct `unmanaged`; C# passes `string` to `&str` implicitly; one copy, no transcoding, to produce a .NET string. |
| Slices | `&[T]` = `ReadOnlySpan<T>`, `&mut [T]` = `Span<T>`; `[T; N]` = inline array | Bounds checks provided by the CLR. |
| Traits | .NET interfaces; `impl Trait for S` = `S : ITrait`; bounds = generic constraints | Reified generics shared with C#; static dispatch via `constrained. callvirt`. |
| Dynamic dispatch | Only through explicit `Gc<dyn Trait>` | Boxing is never implicit. |
| Enums | Tagged value type: `tag` field + per-variant payload fields (overlap only when all payloads are unmanaged) | Correct for generic payloads; optimise layout later. |
| Drop | `trait Drop`; compiler inserts drops at scope end (reverse order, drop flags); each droppable scope lowered inside `try/finally` | Drops also run during panic unwinding for free. |
| Panics | `throw Sabiko.Runtime.PanicException` | Maps onto CLR exceptions; C# can catch it. |
| Generics | Reified .NET generics, no monomorphisation in the compiler | C# can instantiate Sabiko generics. |
| Runtime borrow checking | `Cell<T>` / `RefCell<T>` in the runtime library | For cases static checking cannot prove (async, callbacks from C#). |
| Macros | None in v1; `Print`/`PrintLine` are prelude functions | Barebones first. |
| Source extension | `.sabi` | Short, pronounceable, no known collisions. `.sabiko` is the fallback. |
| Licence | Dual MIT / Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`). Code adapted from `rust-lang/rust` or its crates is attributed in `THIRDPARTY.md` | Mirrors the Rust ecosystem; upstream is dual-licensed the same way. |
| Naming | Sabiko follows .NET conventions: PascalCase for types, traits, functions, methods, fields, variants and consts; camelCase for locals and parameters. Names are emitted exactly as written; `#[dotnet(name = "...")]` overrides per item. Imported .NET names are kept as-is | No mangling, no reverse mapping, one name everywhere. Most .NET-hosted languages end up here. |
| Minimum .NET | .NET 10 (`net10.0`), current LTS | Managed pointers cannot be generic arguments, so `&T` in generic position lowers to a `ref struct` with a `ref T` field and generic types need `allows ref struct` (.NET 9+). |
| `unsafe` | None in v1. Pointer-touching primitives (`Box`, `Vec`, `Arena`) live in the C# runtime assembly | Every Sabiko-emitted assembly stays fully verifiable, so the `ilverify` gate stays meaningful. |
| Integer overflow | Panic in debug, wrap in release; explicit `wrapping_*` / `checked_*` | Rust semantics. Differs from C#'s unchecked default. |

Open questions to settle with the owner before Phase 5 (see §6).

---

## 2. Architecture

Cargo workspace, one crate per pipeline stage so each stage is testable in isolation:

```
crates/
  sabiko_span        source files, byte spans, line/col mapping
  sabiko_diagnostics error codes (E0xxx), rendering (ariadne), --error-format=json
  sabiko_lexer       logos-based tokenizer
  sabiko_ast         AST definitions
  sabiko_parser      hand-written recursive descent + Pratt expressions
  sabiko_resolve     modules, scopes, name resolution -> HIR-lite (DefIds)
  sabiko_types       type checking, local inference (unification), trait resolution
  sabiko_mir         MIR: basic blocks, places, rvalues, terminators; AST->MIR lowering
  sabiko_borrowck    move checking, NLL loans/liveness, mutability, drop elaboration
  sabiko_codegen     MIR -> CIL lowering, the CIL text model and .il emitter, metadata
                     (attributes, interop shapes); the planned separate sabiko_cil crate
                     was folded in, and the Phase 10 PE writer may split it out again
  sabiko_metadata    ECMA-335 reader for referenced assemblies (Phase 8)
  sabiko_driver      pipeline orchestration, session, options
  sabikoc            CLI binary
  sabiko (cli)       build/run/check/test front door (Phase 9)
runtime/
  Sabiko.Runtime/    C# library: PanicException, Box/Gc/Cell/RefCell helpers, Arena, attributes
library/
  core/              Sabiko-written prelude (Option, Result, slice helpers) once the language can express it
tests/
  ui/                compile-fail + diagnostics golden tests
  codegen/           .il golden tests
  run-pass/          compile, assemble, run, compare stdout
  interop/           C# xunit projects consuming Sabiko DLLs and vice versa
  differential/      programs valid in both Rust and Sabiko, compared against rustc
docs/
  PLAN.md, SPEC.md (grammar + semantics), DECISIONS.md (ADR log), THIRDPARTY.md
```

Pipeline: `source -> tokens -> AST -> resolved HIR -> typed HIR -> MIR -> (borrowck,
drop elaboration, MIR validation) -> CIL model -> .il -> ilasm -> .dll/.exe`.

Every MIR transformation is followed by a MIR validator pass in debug builds (as rustc
does), so internal invariant breaks are caught at the pass that introduced them.

---

## 3. Phases

Each phase ends with its exit criteria green in CI. Phases 1 to 4 produce the first
end-to-end "fn main, i32, structs, references" compiler; everything after widens it.

### Phase 0: Bootstrap and toolchain de-risking

Work
- Cargo workspace skeleton, `rustfmt.toml`, `clippy` config, `deny.toml`.
- `scripts/setup-dotnet.sh` (Linux/macOS) and `.ps1`: install .NET SDK, fetch `ilasm`,
  `ildasm` and `ilverify` from NuGet into `tools/`. Also a Claude Code SessionStart hook
  so cloud sessions get the toolchain (the current container has Rust but no .NET).
- GitHub Actions: `cargo fmt --check`, `clippy -D warnings`, `cargo test`, matrix on
  ubuntu/windows/macos.
- Spike: a hand-written `.il` hello world assembled with `ilasm`, verified with `ilverify`,
  executed with `dotnet`. Proves the whole output side before any compiler code exists.
- Dual licensing: rename `LICENSE` to `LICENSE-MIT`, add `LICENSE-APACHE`, update `README.md`.
- `docs/SPEC.md` skeleton, `docs/DECISIONS.md` with the table from §1, `THIRDPARTY.md`.

Exit criteria
- CI green on all three OSes, including the hand-written IL spike running under `dotnet`.

Status (2026-10-19): **complete.** CI run 3 green on ubuntu, macos and windows for both
the Rust checks and the IL spike. Findings:
- Claude Code cloud containers block `dot.net` / `builds.dotnet.microsoft.com` but allow
  `api.nuget.org`, so `setup-dotnet.sh` falls back to assembling the runtime from the
  `Microsoft.NETCore.App.Runtime.<rid>` pack plus the standalone muxer package.
- ILVerify caught a `.maxstack` bug in the first draft of the spike before the CLR did,
  which is exactly the role it will play in the codegen test suite.
- CI caught two portability bugs on the first run: a log line leaking into a command
  substitution on cold caches, and CRLF checkouts on Windows failing rustfmt's newline
  check (fixed with `.gitattributes`).

### Phase 1: Front end (lexer, parser, AST, diagnostics)

Language subset
- Items: `fn`, `struct` (named fields), `const`.
- Statements: `let`, `let mut`, assignment, expression statements, blocks.
- Expressions: integer/float/bool/char/string literals, arithmetic/comparison/logical
  operators, calls, `if`/`else`, `while`, `loop`, `break`, `continue`, `return`,
  struct literals, field access, `&e`, `&mut e`, `*e`, parenthesised, casts (`as`).
- Types: `i8..i64`, `u8..u64`, `isize`, `usize`, `f32`, `f64`, `bool`, `char`, `()`,
  named types, `&T`, `&mut T`.
- Comments, attributes (`#[...]`) parsed and stored for later phases.

Work
- `logos` lexer with spans; error recovery tokens.
- Recursive-descent parser with panic-mode recovery at statement/item boundaries so one
  error does not hide the rest.
- Diagnostics: stable error codes, primary/secondary labels, notes, `--error-format=json`.

Verification
- Lexer unit tests per token class; `cargo fuzz` target: lexer+parser never panic on
  arbitrary bytes.
- Parser snapshot tests (`insta`) of pretty-printed AST for every grammar production.
- Round-trip property: `parse(pretty(parse(src))) == parse(src)` with `proptest`
  generated programs.
- UI tests for syntax errors with expected `.stderr`.

Status (2026-10-19): **complete.** Crates `sabiko_span`, `sabiko_diagnostics`,
`sabiko_lexer`, `sabiko_ast`, `sabiko_parser`, `sabiko_driver` and the `sabikoc` CLI
(`--emit=tokens|ast|pretty`, `--error-format=human|json`). Test layers in place: lexer and
parser unit tests, `insta` snapshots over `crates/sabiko_parser/tests/fixtures`, the
pretty-print/parse round-trip property (strips `Paren` nodes; normalises the two
rustc rules below), a no-panic property over random text and token soup, the UI harness
with `//~ ERROR` annotations and golden `.stderr` files under `tests/ui/parse`, and a
libFuzzer target in `fuzz/` (needs nightly; not yet in CI). Decisions made while
implementing, all matching rustc: a block-like expression at statement start ends the
statement (so `{ a } - 1` is two statements, `{ a }.Foo()` one); a block-like statement
absorbs a following lone `;`; `break`/`return` do not take a `{` operand inside
conditions; an unterminated string or block comment is fatal for the file. Everything
rustc accepts that Sabiko does not yet implement is a dedicated `E0107` diagnostic naming
the phase that will add it. Identifiers are ASCII-only for now.
A post-implementation code review found ten issues, all fixed with regression tests:
a parser hang on stray `)`/`]` (recovery now always makes progress and skips a broken
construct to its own closer), struct literals wrongly rejected inside `[]` in conditions,
`x as i32 < y` (rustc-style diagnostic; the printer parenthesises the cast), `t.0.1`
tokenisation, a leading BOM, CRLF line continuations in strings, sign-prefixed hex
escapes, a leaking diagnostics source cache, the CLI counting warnings as errors, and
an unversioned toolchain download cache.

### Phase 2: Semantic analysis and MIR

Work
- Name resolution: scopes, shadowing, forward references to items, `DefId`s.
- Type checking: bidirectional inference with unification inside function bodies;
  integer literal defaulting; no inference across function boundaries (like Rust).
- Mutability checking: assignment to non-`mut` binding, `&mut` of non-`mut` place,
  assignment through `&T`, field mutation through immutable binding (Ferrite rule:
  field mutability follows the binding).
- MIR lowering: places (locals, fields, derefs), rvalues, basic blocks, terminators
  (`Goto`, `SwitchInt`, `Call`, `Return`, `Unreachable`), scope markers (`StorageLive`
  / `StorageDead`) needed by borrowck and drop elaboration.
- MIR validator and a `--emit=mir` pretty-printer.

Verification
- UI tests: type mismatch, unknown name, wrong arity, assign-to-immutable, `&mut` of
  immutable, mutate-through-`&`. Each diagnostic has an `//~ ERROR E0xxx` annotation in
  the source (compiletest style) and a golden `.stderr`.
- MIR golden tests (`--emit=mir` snapshots) for control flow lowering.
- Property: MIR validator passes on every program in the test corpus after lowering.

Status (2026-10-19): **complete.** Crates: `sabiko_resolve` (items visible everywhere,
`let` scoping and shadowing, one item namespace, E0200–E0207), `sabiko_types`
(bidirectional inference with unification, literal defaulting, `!` and `&mut T` to `&T`
coercions, divergence tracking, compile-time evaluation of `const` items with Rust's
semantics, E0206/E0208/E0300–E0317), `sabiko_mir` (data structures, AST to MIR lowering
in the rustc style with `as_operand`/`as_place`/`expr_into_dest`, scope-driven
`StorageLive`/`StorageDead`, a single return block, a validator run after every pass, a
simplifier that forwards gotos and drops unreachable blocks, and the `--emit=mir` printer),
and `sabiko_borrowck` (a reusable forward dataflow framework, maybe/definitely-initialised
analyses, place mutability classification, E0400–E0405). `sabikoc --stop-after=resolve|typeck`
runs the pipeline up to a stage. Decisions made while implementing (rows 16–22 of
`docs/DECISIONS.md`): the HIR is the AST plus side tables keyed on `NodeId` rather than a
separate tree; items share one namespace; `isize`/`usize` are 64-bit; integer literals that
do not fit their type are hard errors; no inference across item signatures and `_` rejected
in them, literal defaults `i32`/`f64`; MIR has no `Assert` terminators (overflow and
division checks are the backend's job); semantic analysis does not run on a file with parse
errors. Test layers: unit tests in each crate (35 hand-built MIR cases for the borrow
checker), UI tests with `//~ ERROR` annotations and golden `.stderr` files under
`tests/ui/resolve` (6 files), `tests/ui/typeck` (16) and `tests/ui/borrowck` (6), one
concept per file; MIR snapshot tests over `crates/sabiko_mir/tests/fixtures`; and a corpus
test that lowers every diagnostic-free program in the repository and asserts the validator
accepts it before and after simplification. Reviewing the first generated goldens caught
six type-checker bugs before they shipped (an `if` without `else` unchecked against the
return type, `&str` annotations in bodies rejected, `-1 as u32` and `*s` for `s: &str`
accepted, diverging `let` initialisers not falling back to `()`, duplicate reports), all
fixed with UI tests. Known gap carried into Phase 3: comparisons between references
(`&i32 == &i32`) are rejected; they need the by-value comparison lowering that `PartialEq`
on references implies. The semantics are written up in `docs/SPEC.md` §11.

### Phase 3: Borrow checker v1

Work
- Copy vs move types (primitives and `&T` are `Copy`; structs are move unless
  `#[derive(Copy)]`); use-after-move, move-out-of-borrow, partial moves, move in loop.
- Loans: checked by generating Polonius facts from the MIR (an origin per reference type
  in each local's type, loans at `&`/`&mut` rvalues, kills on reassignment and
  `StorageDead`, invalidations at writes/moves/conflicting borrows, variable liveness per
  point, subset constraints from assignments, call arguments and callee signatures) and
  solving them with the `polonius-engine` crate; an error is reported for every loan live
  at a point that invalidates it. Move checking, mutability and signature elision stay
  hand-written.
- Conflict rules: shared XOR mutable; writes/moves of a borrowed place; disjoint field
  borrows allowed (partial borrows, as in the Ferrite note); reborrows (`&mut *r`,
  `&*r`).
- Function boundaries without explicit lifetimes: Rust's elision rules (one input
  reference, or `&self`). Any signature elision cannot resolve is an error until
  Phase 9 adds explicit lifetimes.
- Returning references to locals rejected; references stored in struct fields rejected
  until Phase 9.
- Drop elaboration (insert `Drop` calls at `StorageDead` in reverse declaration order,
  drop flags for conditionally moved values) is deferred: no Phase 3 type needs drop glue
  (primitives, references and structs of them are trivially droppable), so it lands with
  `Box`/`String` in Phase 5.

Verification
- Compile-fail corpus (`tests/ui/borrowck/`): at least one test per rule above, in both
  the "must reject" and the adjacent "must accept" form (e.g. two shared borrows OK,
  mutable after last use of shared OK, disjoint fields OK).
- Differential testing against `rustc`: every `.sabi` program in `tests/differential/`
  is also valid Rust syntax (the harness prepends `#![allow(nonstandard_style, unused,
  dead_code)]` since Sabiko uses .NET casing). A harness runs `rustc --emit=metadata` on
  each and asserts "rustc rejects with a borrow error" iff "sabiko rejects with a borrow
  error"; a program either compiler rejects for any other reason fails the test outright,
  so the corpus stays valid in both languages. Phase 3 is not done while this disagrees
  on the subset.
- Diffing our Polonius facts against rustc's `-Znll-facts` dump was considered and
  rejected: the facts are indexed by MIR locations and our MIR is not rustc's MIR, so the
  two fact sets cannot be aligned without a lowering-preserving map that would itself
  need testing. The differential harness compares verdicts instead.
- Property: borrowck is deterministic and idempotent (same diagnostics across two runs,
  across statement reorderings that preserve semantics).

Status (2026-10-19): **complete.** `sabiko_borrowck` now runs four checks per body:
mutability and initialisation (Phase 2), move checking and loan checking. Moves
(`moves.rs`): a move-path forest cut at the first `Deref`, a maybe-moved dataflow, and
move-site recovery by a backward walk with dominator-based back-edge detection for the
"in previous iteration of loop" label; rustc's E0382/E0507 wording, deduplicated per set of
move sites (E0406/E0407). Loans (`loans.rs`, `loans/regions.rs`, `loans/facts.rs`): one
origin per reference type in every local, universal origins from the elided signature
(exactly one input reference, else E0416), Polonius facts generated from the MIR (loans,
kills on reassignment, invalidations by conflicting accesses with rustc's shallow/deep
and disjoint-field rules, variable liveness, subset constraints from assignments, calls
and reborrow chains) and solved by `polonius-engine`; errors map to E0408–E0415 with
"borrow later used here" labels found by a forward search over the CFG. Groundwork:
`#[derive(Copy)]` (E0417 when a field is not `Copy`), `&mut` reborrows at coercion sites,
temporary lifetime extension for `let r = &<value>;`, references in struct fields rejected
until Phase 9. Drop elaboration is deferred to Phase 5 (no Phase 3 type has drop glue).
Verification: 35 + 24 + 31 hand-built MIR unit tests, UI fixtures under `tests/ui/borrowck`
(mutability, moves, loans, accepted programs), and the differential harness
(`crates/sabiko_driver/tests/differential.rs`) over `tests/differential/` (40 programs in
`ok_`/`err_` pairs: 13 moves, 20 loans, 7 mutability/initialisation), which agrees with
rustc on every program: both accept the `ok_` files and both reject the `err_` files with a
borrow-family error. Integrating the parallel work found and fixed a dataflow bug (a block
whose entry state equalled the bottom value was never processed). Known gaps carried
forward: two-phase borrows (`v.Push(v.Len())`) wait for methods in Phase 5; comparisons
between references are still rejected; `check_loans` only runs for bodies that reached
MIR, so a signature-elision error inside a body with type errors is reported once the
type errors are fixed.

### Phase 4: CIL code generation v1 and the runtime library

Work
- MIR to CIL: locals table, stack-machine lowering per statement, `ldloca`/`ldarg`/
  `ldind`/`stind`/`ldflda`/`stfld`/`initobj`, branches from terminators, `.maxstack`
  computation, `.entrypoint`.
- One static class per module for functions; structs as `sequential sealed`
  `System.ValueType`s; `Drop` scopes as `try/finally`.
- Runtime library `Sabiko.Runtime.dll` (C#): `PanicException`, `Print`/`PrintLine`
  helpers bridging to `System.Console`.
- `sabikoc` CLI: `--emit=il|dll|exe|mir`, `-o`, `--runtime-path`, calls `ilasm`.
- Panic on integer overflow in debug builds (`add.ovf`), wrap in release.

Verification
- Codegen golden tests: `.il` snapshots per construct (small, reviewable).
- Run-pass tests: compile, `ilasm`, run with `dotnet`, compare stdout to `.expected`;
  also assert exit codes for panic tests.
- `ilverify` on every emitted assembly in CI. A failure is a bug: Sabiko must only emit
  verifiable IL.
- Drop-order tests: programs printing from `drop` verify reverse order, conditional
  drops, drops during unwinding.
- Differential run-pass: programs in `tests/differential/` compiled with both `rustc`
  and `sabikoc` must print identical output.

Status (2026-10-19): **complete**, with drop scopes deferred. `sabiko_codegen` lowers
the MIR to textual IL (SPEC §10): structs as `sequential` value types, functions as static
methods of a class named after the module, `in`/`ref` signatures, a stack-machine walk per
statement with Rust arithmetic semantics (checked in debug, wrapping in release; saturating
float casts; shift-amount checks), `.maxstack` computed exactly, and an entry-point wrapper
that reports an escaping exception Rust-style and exits with 101. `Print`, `PrintLine` and
`Panic` are builtins lowered to `Sabiko.Runtime`, a C# library compiled with `csc` from the
compilers-toolset package on the bare runtime (no SDK). `sabikoc --emit=il|dll|exe`, `-o`,
`--release`, `--runtime-path`, `--ilasm`. Verification: IL snapshot tests per construct
(`crates/sabiko_codegen/tests`), 20 run-pass programs that are compiled, verified with
ILVerify and run (`tests/run-pass`: stdout and exit codes, release-mode wrapping, user
functions shadowing the builtins, deep recursion), and 65 run-time differential programs
under `tests/differential/run` whose output and exit codes match the rustc-compiled program
(every operator per width, casts, float formatting, control flow, structs, references, and 30
panic programs covering each overflow kind, division by zero, `MIN / -1`, `MIN % -1`, bad
shift amounts and `-MIN`); CI runs all of it on Linux, macOS and Windows. Building the
corpus found one backend bug (narrow `MIN % -1` returned 0 instead of panicking), fixed. ILVerify is the gate with one documented
exception, `ReturnPtrToStack` (decision 30: ECMA-335 rejects every function returning a
reference; the borrow checker covers it). Deferred: `Drop` scopes as `try/finally` and the
drop-order tests go with drop elaboration to Phase 5 (nothing has drop glue yet, decision
26); `i128`/`u128`, `&&T` and `()`-typed parameters/fields are hard errors (E0500) until
their representations are decided; ref-to-ref and panic messages lack source positions.

**Milestone A** (end of Phase 4): the examples from both design notes (`add_one`,
`move_point`, partial borrows) compile, verify, run, and the rejection examples are
rejected with the intended diagnostics. Reached: `tests/run-pass/basics/hello.sabi` runs
both examples; `tests/ui/borrowck` holds the rejections.

### Hardening pass (between Phases 4 and 5)

Pulled forward from Phase 10 once the pipeline ran end to end: the hand-written corpora
were the only thing constraining four stages, and the next phase doubles the language
surface. Work and verification are the same thing here; see `docs/TESTING.md`.
- Random program generator for the Phase 4 subset (`crates/sabiko_testgen`), applied to
  both rustc oracles (compile-time verdicts, run-time output and exit codes) for a seed
  range per CI run and thousands of seeds nightly.
- Determinism and idempotence properties of the pipeline over generated programs.
- Panic messages compared against rustc in the run-time differential tests.
- Whole-pipeline fuzzing: libFuzzer targets over arbitrary text and token soup, a nightly
  budget, and a stable-Rust no-panic property over corpus mutations on every CI run.
- IL snapshots assembled and verified, not just compared.
- Mutation testing with `cargo mutants` on the borrow checker, type checker and backend;
  survivors triaged, tests added for the real gaps, scores recorded with thresholds.

Status (2026-10-19): **complete.** Generator (`sabiko_testgen`): 800 seeds across two
ranges agree with rustc on verdicts, output, exit codes and panic messages; the only
construct it still avoids is the implicit two-phase reborrow of a `&mut` local in argument
position (`Use(m, m.Field)`), which Sabiko rejects and rustc accepts (known gap since Phase
3, waits for methods). Fuzzing: three libFuzzer targets, no crashes after the three
never-type fixes it found; the proptest no-panic suite runs 256 cases per property on every
CI job and 4096 nightly. Mutation scores: borrowck 90.5%, codegen 95.5%, types 98.5%, with
thresholds enforced nightly. Nine compiler bugs were found and fixed in total (see
`docs/TESTING.md`); the largest fix makes MIR building copy place operands into temporaries
at operand-evaluation time, as rustc does (decision 36), which repaired a miscompile and
three false rejections at once.

Update (2026-10-21), after the first nightly run: it measured `sabiko_types` at 66.9%, not
the 100% recorded above, so that measurement was wrong. New tests bring it to 98.5% and the
threshold moves from 99 to 98, because the six survivors are equivalent mutants (listed in
`docs/TESTING.md`; decision 40). The run's two fuzz crashes (the pretty printer's fixpoint
on a malformed attribute, a const-eval panic on an error-typed operand) are fixed, as are
three bugs the new tests found: `f32` constants computed without rounding (a miscompile),
casts giving their type to integer variables instead of only to literal operands, and
`<<=`/`>>=` accepting floats. Each has a regression test. The fuzz job had never built its
targets (`cargo fuzz build` ran on the pinned stable toolchain); it now uses `+nightly`.

Update (2026-10-21), after the second nightly run: two more bugs, both fixed with
regression tests. Borrowing a diverging operand (`&break 4;`, `let q = &loop {};`) panicked
MIR validation, and a struct literal in the block of an `if` nested in a `while` condition
was rejected by the parser. Known gap left by the first: the type checker types `&e` as `!`
when `e` diverges, where rustc types it `&!`. Sabiko therefore accepts some programs rustc
rejects (`let q: &Point = &loop {};`, `Pass(&return 1)`, `fn Main() { &mut loop {} }`) and
rejects one rustc accepts (`*&mut return 9`, E0309). None of these miscompiles, since the
code after the borrow is unreachable. Typing it `&!` needs the backend to handle `&!`
locals, which it rejects today (E0500).

Update (2026-10-22), after the third nightly run: the fuzzer overflowed the stack on
`let r; r = &r;`, which unified a type variable with a reference to itself; unification now
has an occurs check (E0300, cyclic type of infinite size). The nightly runs measure
`sabiko_borrowck` at 94.0% (267 of 284 viable), not the 90.5% (267 of 295) recorded from a
local run: the same mutants are killed, and 11 survivors are unviable in CI. Its threshold
moves from 90 to 93.

Update (2026-10-25), after the fourth nightly run: the parser fuzzer overflowed the stack on
~1900 nested blocks. The parser had no nesting limit, and in a debug build the whole
parser alone overflowed a test thread's 2 MiB at 75 nested blocks. Nesting is now limited to
256 levels (fatal E0109, decision 41), and the driver runs the pipeline on a 64 MiB thread;
`crates/sabiko_driver/tests/nesting.rs` compiles every kind of nesting at the limit and one
past it. The same run's slow units (`&&&…&&1`) came from the type checker resolving types in
time roughly quartic in their depth (11 s for 255 `&` in debug); resolution is linear now.

Update (2026-10-29), type-checker gaps left by Phases 2–4 (#16, #21, #17): comparisons
between references (all six operators, with rustc's `&`/`&mut` mixing rules) type-check and
compare the referents; MIR reborrows each operand as a shared temporary and dereferences it
after both operands ran, as rustc's `PartialEq::eq(&l, &r)` does, and the generator produces
such comparisons. One verdict gap remains: rustc's borrow of a reference *variable* used as
an operand (`r == { r = &y; r }`), which needs `&&T` codegen. Non-BMP `char` literals are
the type checker's E0318 instead of a codegen E0500. The borrow of a diverging expression is
typed `&!` as in rustc, `&!` locals get no storage (decision 44), and a block with a tail is
typed by its tail even after a diverging statement; together these close the four known
divergences listed above.

Update (2026-10-29), borrow-checker and infrastructure gaps (#20, #23, #22): the
signature-elision check (E0416) runs per function from the signature, independently of
whether the body type-checked, so it appears next to body errors as rustc's E0106 does;
when any body has errors, borrow checking is still skipped for every body. A `&mut T`
passed where a `&T` is expected is now a shared reborrow (`&*r`), as in rustc: the old
mutable reborrow made Sabiko reject `F(&*r, r)`, which generated programs found.
Verification gains a property that the borrow-check verdict does not change when adjacent
independent statements are swapped; rustc judges every reordered program and borrow-clean
ones must run like the original, so the independence analysis is itself tested. Phase 0's
`deny.toml` is in place: licences, bans and sources on every CI run, advisories nightly
(decision 45).

Update (2026-10-29), Phase 4 leftovers (#19, #18): `i128`/`u128` compile to
`System.Int128`/`System.UInt128` with every operator, cast, constant and print (decision
42); the BCL's Int128-to-float conversion rounds twice, so it goes through the runtime, and
the `-2^127` literal (which evaluated to `MIN + 1`) is fixed. Every run-time panic reports
rustc's `file:line:column` and per-operator message, through explicit overflow checks in
debug builds instead of `.ovf` opcodes (decision 43). The run-time differential harness
compares full panic reports, including locations, and supports `//@ release`; the generator
emits 128-bit types; run-pass pins `.stderr`. Still E0500: `&&T` and `()`-typed parameters
and fields. Generated `u128` literals stay below 2^127.

Update (2026-10-29), benchmarks (decision 50): `benchmarks/` compares six Sabiko programs
with the same algorithms in hand-written C# (rustc as a reference) nightly, in
whole-process and in-process modes, reporting time, peak RSS and allocations with
p50/p90/p99 over 100 samples after warmup; allocations are gated against
`benchmarks/allocation-baseline.json`. Steady state is within about 6% of C# except
recursive fibonacci (1.39x); start-up is 1.6–3.8x C# on call-heavy programs because the IL
is temporary-heavy and an early-placed return block makes the JIT see a loop, and #28's
explicit checks slowed debug-build start-up further. The codegen fixes are #29.

### Phase 5: Data types

Work
Order (decision 46): tuples and `()` values, then `impl` blocks and methods (with two-phase
borrows), then drop elaboration, then enums and `match`, so that match lowering is written
against drops rather than retrofitted.
- Tuples; `()` as a value with real storage where it is a parameter or field type.
- `impl` blocks, methods with `self`, `&self`, `&mut self`, associated functions,
  method call syntax with auto-ref; two-phase borrows for autoref'd receivers and implicit
  `&mut` reborrows in argument position.
- Drop elaboration (moved here from Phase 3 by decision 26, given a phase by decision 46):
  drop flags, drops at scope end in reverse declaration order, each droppable scope lowered
  inside `try/finally`. `impl Drop for S` is the one trait impl accepted before Phase 6,
  special-cased as a language item so drops can be observed and diffed against rustc.
- `enum` with unit, tuple and struct variants, non-generic (generic enums come with Phase 6,
  decision 47); `match` with patterns (literals, bindings, wildcards, tuple/struct/enum
  destructuring, guards); exhaustiveness and reachability checking (usefulness algorithm,
  as in rustc).
- `[T; N]` (via `InlineArrayAttribute`), `&[T]` / `&mut [T]` as spans, indexing, `.len()`.
- `&str` as `ReadOnlySpan<char>`, string literals (`ldstr` + `AsSpan`, or RVA data),
  basic slicing. No owned string yet; `Gc<System::String>` moves to Phase 7 (decision 48).
- `&T` in generic position lowers to a `ref struct` `Ref<T>` with a `ref T` field;
  generic types that may hold it are emitted with `allows ref struct`. This also lowers
  `&&T`.

Verification
- Exhaustiveness UI tests (missing variant, unreachable arm, nested patterns).
- Enum layout tests from C#: a C# test reads tag/payload fields of a Sabiko enum and
  matches on it (the first `tests/interop` harness).
- Drop-order tests (from Phase 4): reverse declaration order, conditional drops through
  flags, drops during unwinding, compared against rustc.
- Index out of range: run-pass test expects a panic with rustc's message for `a[i]`.
- Differential tests and the program generator extended to every new construct.

### Phase 6: Traits and generics

Work
- `core` prelude written in Sabiko, moved here from Phase 5 (decision 47): `Option`,
  `Result`, `?` operator, `unwrap`, `expect`, and `.get(i) -> Option<&T>` on slices
  lowered to a length check. `?` converts errors through `From` once traits exist.
- `trait` with required methods and default methods (default methods become static
  helper methods, since .NET interfaces on structs cannot carry state).
- `impl Trait for Type`; coherence (one impl per trait/type pair).
- Generic functions and structs with bounds `T: Trait`; reified generics with
  `.NET` constraints; implicit `struct` constraint unless `T: ?Sized`-style opt out for
  `Gc<T>`.
- Static dispatch via `constrained. callvirt`; `dyn Trait` only through
  `Gc<dyn Trait>` (boxing explicit).
- Operator traits (`Add`, `Eq`, `PartialOrd`) on user types; `#[derive]` for `Copy`,
  `Clone`, `Debug`-like `fmt`, `PartialEq`.

Verification
- UI tests: unsatisfied bound, ambiguous method, implicit boxing rejected, missing impl
  method.
- Interop test: C# type implementing a Sabiko trait passed to a Sabiko generic function,
  and a Sabiko struct used where C# expects `where T : struct, ITrait`.
- `ilverify` across generic instantiations; run-pass tests for generic containers.

### Phase 7: Ownership runtime: Box, Gc, Cell/RefCell, arenas

Work
- `Box<T>` (`T: unmanaged`): `NativeMemory.Alloc`, deterministic free in `drop`,
  `Deref`/`DerefMut`. Compile-time `unmanaged` check (no GC references inside).
- `Gc<System::String>` with zero-copy `.as_str()`, moved here from Phase 5 (decision 48).
- Owned `String` (native UTF-16 buffer) and `Vec<T>` (`T: unmanaged`) with
  `.to_dotnet()` conversions; both implemented over runtime primitives so Sabiko IL
  stays verifiable.
- `Gc<T>` for .NET classes: handle wrapper, methods callable through `&`/`&mut`
  borrows; no deterministic free (GC owns it) and the type system says so.
- `Cell<T>` and `RefCell<T>` in the runtime: borrow counter, atomic ops, `BorrowError`
  panics; `Ref<'_, T>` / `RefMut<'_, T>` as `ref struct` guards that release in `drop`.
- `Arena` region allocator: bump allocation in native memory, freed when the arena
  drops; `&'arena T` outlives are checked by the Phase 9 lifetime machinery.
- Debug allocator that counts live allocations.

Verification
- Leak tests: run-pass programs assert `live_allocations() == 0` at exit; also under
  panic unwinding.
- `RefCell` conflict tests: double mutable borrow panics with `BorrowError`; shared then
  mutable after release succeeds.
- Stress test: a loop allocating and dropping millions of `Box`es shows flat memory
  (measured via `GC.GetTotalMemory` staying flat and process RSS bounded).
- GC-independence check: run with `GCSettings.LatencyMode` set to the most aggressive
  setting plus `GC.TryStartNoGCRegion`; Sabiko-only programs must still run to
  completion.

### Phase 8: C# interop, both directions

Work
- `extern "dotnet"` blocks (manual declarations, like Rust FFI) mapping `ref` to
  `&mut`, `in`/`ref readonly` to `&`, `out` to a write-only `&out` (or rejected),
  classes to `Gc<T>`, static methods to free functions, constructors to `new`.
- Metadata import: `sabiko_metadata` reads ECMA-335 metadata of referenced assemblies so
  `extern` blocks become optional; type and method signatures are checked against the
  real assembly.
- Export shape: `IsReadOnlyAttribute` on `in` params, `IsByRefLikeAttribute` on
  ref structs, `StructLayout(Sequential)`, `Drop` also implemented as `IDisposable`,
  XML doc comments from `///`, names emitted as written.
- Trust boundary: extern calls are marked `unsafe`-like in diagnostics (C# can alias
  `ref` parameters internally; Sabiko trusts the signature).

Verification
- `tests/interop/FromCSharp`: xunit project that references a Sabiko-built DLL and
  checks: structs are plain structs, `&mut` appears as `ref`, `&T` as `in`, `Drop`
  as `IDisposable`, generics with constraints instantiate.
- `tests/interop/ToCSharp`: Sabiko calling `System.Text.StringBuilder`, `Math`, a
  custom C# library with `ref`/`in`/`out` parameters.
- Zero-marshalling proof: `ildasm` the interop call sites and assert no `box`,
  `unbox`, `ldobj`-copy, or `Marshal.*` calls appear; microbenchmark with
  BenchmarkDotNet comparing a Sabiko `&mut Point` call against the C# `ref Point`
  equivalent (expected: same codegen, same timing).
- Round-trip layout test: a C# struct and the Sabiko struct with the same fields have
  identical `Marshal.SizeOf` and `Unsafe.SizeOf`.

**Milestone B** (end of Phase 8): the Physics/MathLib examples from the Ferrite note
work in both directions from a C# `dotnet test` project.

### Phase 9: Modules, lifetimes, tooling

Work
- `mod`, `use`, `pub`, multi-file crates, a project manifest (`Sabiko.toml`), referenced
  assemblies, output kinds (library/executable).
- Explicit lifetimes `'a` on functions and structs; structs with reference fields
  become `ref struct`s; region inference replaces the Phase 3 elision-only rule.
- `sabiko` CLI: `new`, `build`, `run`, `check`, `test` (`#[test]` functions run under
  `dotnet`), `fmt` (basic).
- Language server (optional, after the above): hover types, go to definition,
  diagnostics; re-use the span/diagnostic infrastructure.

Verification
- UI tests for lifetime errors (returning a reference to a shorter-lived value,
  struct outliving its referent) and privacy errors.
- Multi-crate run-pass tests: a Sabiko library consumed by a Sabiko executable.
- `sabiko test` dogfooded: the `core` prelude's own tests run through it.

### Phase 10: Hardening and 0.1 release

Work
- Direct PE/metadata emission (drop `ilasm`); keep the `.il` backend for golden tests.
- Polonius-style (Datalog) borrow checker as an alternative implementation, run in CI
  against the NLL one on the whole corpus (two independent checkers must agree).
- Performance: incremental parsing not needed, but compile-time budgets per phase
  tracked in CI with `criterion`.
- Enum layout optimisation (overlapping unmanaged payloads), niche optimisation for
  `Option<&T>`.
- `docs/SPEC.md` complete; tutorial; examples gallery; publish `Sabiko.Runtime` to NuGet
  and `sabiko` as a `dotnet tool` or standalone binary.

Verification
- Byte-for-byte: assemblies from the PE writer and from `ilasm` round-trip through
  `ildasm` to identical text.
- Full corpus green on both borrow checkers; `ilverify` clean; mutation scores from the
  hardening pass maintained or raised, with the direct PE writer added to the mutated set.
- Soak: the nightly fuzzing budget (hardening pass) extended to the PE writer, asserting
  `ilverify`-clean output for every accepted program.

---

## 4. Verification and testing strategy (cross-cutting)

| Layer | Technique | Where |
| --- | --- | --- |
| Lexer/parser | Unit tests, `insta` snapshots, `cargo fuzz`, `proptest` round-trip | `crates/*/tests`, `fuzz/` |
| Diagnostics | compiletest-style `//~ ERROR` annotations + golden `.stderr`; error codes stable | `tests/ui/` |
| Type and borrow checking | Must-reject and must-accept pairs; differential vs `rustc` on the shared subset | `tests/ui/`, `tests/differential/` |
| MIR | Validator pass after every transformation (debug builds); `--emit=mir` snapshots | `sabiko_mir` |
| Codegen | `.il` golden snapshots, assembled and verified; `ilverify` on every artifact | `crates/sabiko_codegen/tests`, `tests/ui/codegen`, CI |
| Runtime behaviour | Run-pass stdout comparison; panic exit codes; drop order; leak counters | `tests/run-pass/` |
| Memory model | Live-allocation counters, no-GC-region runs, RSS bounds | `tests/run-pass/memory/` |
| Interop | xunit projects in both directions; IL inspection for zero marshalling; layout equality; BenchmarkDotNet | `tests/interop/` |
| Compiler robustness | Whole-pipeline fuzzing (libFuzzer nightly, proptest on every run); generated-program differential and determinism properties; `cargo mutants` on checker and backend crates | `fuzz/`, `crates/sabiko_testgen`, `crates/sabiko_driver/tests/no_panic.rs`, nightly |
| Regressions | Every fixed bug adds a test in the matching layer | all |

CI gates (every PR): fmt, clippy, unit tests, UI/codegen/run-pass suites on Linux,
Windows and macOS, `ilverify` clean, differential suite, a small generated-program seed
range. Nightly (`.github/workflows/nightly.yml`): fuzzing budget, thousands of generated
seeds, mutation testing; benchmarks with trend tracking from Phase 8. `docs/TESTING.md`
describes every layer and how to update its expectations.

---

## 5. Milestones summary

| Milestone | After phase | Demonstrates |
| --- | --- | --- |
| A | 4 | Both design notes' examples compile, verify, run; rejections are diagnosed |
| B | 8 | Two-way C# interop with zero marshalling, deterministic `Box` freeing |
| C | 9 | Multi-module projects, explicit lifetimes, `sabiko test` dogfooding |
| 0.1 | 10 | Direct PE emission, two agreeing borrow checkers, published toolchain |

---

## 6. Decisions log and open questions

Settled by the owner (2026-10-19)
- `ilasm` first; direct PE emission in Phase 10.
- Dual MIT / Apache-2.0 licensing.
- Strings: `&str` = `ReadOnlySpan<char>` first, native UTF-16 owned `String` later,
  `System.String` only behind `Gc`.
- Naming: Sabiko itself uses .NET conventions (PascalCase); names export as written.
- Source extension `.sabi`.
- .NET 10 floor.
- No `unsafe` in v1; unverifiable code confined to the C# runtime assembly.
- Integer overflow: Rust semantics (panic in debug, wrap in release).

Settled by the owner (2026-10-29)
- Syntax fidelity: Sabiko stays a strict syntactic subset of Rust, casing aside
  (decision 49).
- Drop elaboration lands in Phase 5 with `Drop` special-cased (decision 46); `Option`,
  `Result` and `?` move to Phase 6 (decision 47); `Gc<System::String>` to Phase 7
  (decision 48).

Open, to decide before the phase that needs them
1. .NET exceptions crossing into Sabiko (Phase 8). Proposal: propagate as panics;
   opt-in `#[dotnet(catch)]` on extern declarations converts to `Result<T, Gc<Exception>>`.
2. Nullability of `Gc<T>` (Phase 7/8). Proposal: non-null; `Option<Gc<T>>` lowers to a
   nullable reference; imports read nullable annotations.
3. Module to assembly mapping (Phase 9). Proposal: crate = assembly, module = namespace,
   free functions in a static class named after the module.
4. Collections (Phase 7). Proposal: `Vec<T>` native for `T: unmanaged`; GC collections
   only via `Gc<List<T>>`.
5. Threads, `Send`, `Sync`. Proposal: out of scope for 0.1.
