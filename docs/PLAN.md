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
| Traits | .NET interfaces where C# needs them; bounds are checked by the compiler and calls dispatch statically to the impl (decision 82) | Monomorphisation resolves every trait call to a direct `call`. |
| Dynamic dispatch | Only through explicit `Gc<dyn Trait>` | Boxing is never implicit. |
| Enums | Tagged value type: `tag` field + per-variant payload fields (overlap only when all payloads are unmanaged) | Correct for generic payloads; optimise layout later. |
| Drop | `trait Drop`; compiler inserts drops at scope end (reverse order, drop flags); each scope with something to drop on a panic path lowered as a `try`/`fault` region, a panic during unwinding aborts (decisions 55, 56) | Drops also run during panic unwinding for free. |
| Panics | `throw Sabiko.Runtime.PanicException` | Maps onto CLR exceptions; C# can catch it. |
| Generics | Monomorphised in the compiler after type and borrow checking, which run once on the generic code (decision 82; supersedes reified .NET generics) | References as type arguments, traits on primitives and drops of `T` need no run-time support; every Sabiko type is a value type, so the JIT specialises each instantiation anyway. |
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
  sabiko_mir         MIR: basic blocks, places, rvalues, terminators; AST->MIR lowering;
                     drop elaboration and copy forwarding after borrow checking
  sabiko_borrowck    move checking, NLL loans/liveness, mutability
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
  interop/           C# programs (csc, no SDK) consuming Sabiko DLLs; later vice versa
  differential/      programs valid in both Rust and Sabiko, compared against rustc
docs/
  PLAN.md, SPEC.md (grammar + semantics), DECISIONS.md (ADR log), THIRDPARTY.md
```

Pipeline: `source -> tokens -> AST -> resolved HIR -> typed HIR -> MIR -> (borrowck,
lints, monomorphisation, drop elaboration, copy forwarding, MIR validation) -> CIL model ->
.il -> ilasm -> .dll/.exe`.

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
libFuzzer target in `fuzz/` (needs nightly; the nightly workflow runs it). Decisions made while
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
their representations are decided (since decided: decisions 42, 52 and 63); ref-to-ref and panic messages lack source positions.

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

Update (2026-11-06), after nightly run 18 (decision 67, #64): `sabiko_types`'s mutation
job had tested 827 of its 1209 mutants (634 the night before) when it hit its 180-minute
limit, and `sabiko_codegen` took 137 minutes. The run also showed
that cargo-mutants' baseline tests only the mutated package, so each mutant's test
timeout was the 60-second floor, shorter than the suites it runs: every codegen mutant
that the fast tests missed was cut off in `generated` and counted as caught. The nightly
now passes an explicit 300-second test timeout and runs each crate as four shards; a score
job judges each crate on the four shards' combined counts (`docs/TESTING.md`). The first
sharded run (run 20) took at most 69 minutes per shard and measured `sabiko_borrowck` at
94.8%, `sabiko_codegen` at 95.2% (32 survivors that the 60-second limit had counted as
timeouts) and `sabiko_types` at 98.4%.

Update (2026-11-06), after nightly run 20, the first with that timeout: `sabiko_codegen`
measured 95.2% (640 of 672 viable), just above its threshold of 95. Of the 32 survivors,
seven changed the IL of corpus or generated programs and now have killing tests. Two of
them miscompiled a shape no test reached: a two-way branch whose targets both come
before it in the layout (`if c { continue; }` ending a loop body) fell through into the
next block (`run/control/branch_targets`). The other five change only the IL (the IL
fixture `edge_shapes`). The remaining 25 are equivalent (`docs/TESTING.md`). That is 647
of 672, 96.3%, and the threshold stays at 95, which leaves room for eight new survivors.

Update (2026-10-29), type-checker gaps left by Phases 2–4 (#16, #21, #17): comparisons
between references (all six operators, with rustc's `&`/`&mut` mixing rules) type-check and
compare the referents; MIR reborrows each operand as a shared temporary and dereferences it
after both operands ran, as rustc's `PartialEq::eq(&l, &r)` does, and the generator produces
such comparisons. One verdict gap remained: rustc's borrow of a reference *variable* used
as an operand (`r == { r = &y; r }`), since closed by a shallow fake borrow (decision 63).
Non-BMP `char` literals are
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
and fields (lowered since, decisions 52 and 63). Generated `u128` literals stay below 2^127.

Update (2026-10-29), benchmarks (decision 50): `benchmarks/` compares six Sabiko programs
with the same algorithms in hand-written C# (rustc as a reference) nightly, in
whole-process and in-process modes, reporting time, peak RSS and allocations with
p50/p90/p99 over 100 samples after warmup; allocations are gated against
`benchmarks/allocation-baseline.json`. Steady state is within about 6% of C# except
recursive fibonacci (1.39x); start-up is 1.6–3.8x C# on call-heavy programs because the IL
is temporary-heavy and an early-placed return block makes the JIT see a loop, and #28's
explicit checks slowed debug-build start-up further. The codegen fixes are #29.

Update (2026-10-29), IL shaped for the JIT (#29, decision 51): blocks are laid out in
reverse postorder, so the only backward branches are loop back edges, and a jump to a
return-only block is the return itself; failed checks call cold `[DoesNotReturn]` throw
helpers in `Sabiko.Runtime.Checks`; copy forwarding (`sabiko_mir::forward`) runs after
borrow checking, and codegen keeps single-use values on the IL stack and drops unused
locals. Release start-up is now 1.07–1.18x C# for fibonacci, primes and nbody (was
1.6–3.8x); debug start-up for mandelbrot and lcg_hash is within 1.08x (was 3.3x and 1.8x).
Steady state is at parity except checked fibonacci (1.32x): decision 43's 64-bit check
helpers keep `FibRecursive` too large to inline into itself. Gaps: struct literals still
use `initobj`+`stfld`; the shapes for Phase 5 constructs (part 3 of #29) are tracked on
#6, #8, #9 and #12.

Update (2026-10-29), tuples (#6, decision 52): tuple types, expressions, `.0` access
(`t.0.1` split as in rustc) and tuple patterns in `let` and parameters (nested, `_`,
`mut`, annotated) go through every stage. Tuples without references are
`System.ValueTuple` (nested in `Rest` past seven elements), tuples holding references a
generated `ref struct`, both built by `newobj`; `()` parameters, struct fields and tuple
elements are `Sabiko.Runtime.Unit`, lifting decision 35's E0500 for them. Tuple elements
are ordinary field projections in MIR, so moves, partial moves, re-initialisation,
disjoint loans and mutability work on them as on struct fields; tuples of references carry
their loans (`RegionTy::Tuple`) and count for elision. Default binding modes are
implemented for tuple patterns (`let (a, b) = &mut t;` binds `&mut` elements; `mut x`
there binds by value, as in Rust 2021; E0337 since the move to edition 2024, decision 74), and temporary lifetime extension reaches through
tuple elements of a `let` initialiser. All 31 staged compile-time and 16 of the 17 staged
run-time programs agree with rustc; the generator produces tuples (types, literals, element
reads and writes, destructuring by value and by reference, parameters, returns, fields)
and 2000 seeds at each of the bases 1200000, 4400000 and 8800000 agree with rustc
(verdicts, output, panics, determinism, reorderings). A `tuples` benchmark (pairs by value,
`System.ValueTuple` in the C#) runs at 0.99–1.00x C# in process and 1.03–1.05x per launch,
allocating nothing; the other benchmarks did not move beyond noise. Gaps: comparing tuples (`==`, `<`, ...) is
E0107 (the comparison traits are Phase 6; `tests/pending/tuples/known_gap/equality.sabi`);
destructuring assignment (`(a, b) = (b, a)`) was E0107 too (since #50, decision 70); `&&T` and `&()`, also as tuple elements,
stay E0500 until `Ref<T>` (`known_gap/refs_through_double_ref.sabi`; since lowered, #11); the two-phase
reborrow `Use(m, m.0)` waits for #15; a tuple holding a reference cannot be a struct field
(lifetimes, Phase 9); constants cannot be tuples (E0315; since #50 they can); literal and other refutable
patterns are E0107 until `match`. Pre-existing then: `let _ = place;` read the place where
rustc only mentions it, so Sabiko rejected `let _ = p; Take(p);`; #37 fixed it: the
statement is a MIR `PlaceMention`, a liveness use of the local and nothing else, as in
rustc. The generator does not produce tuples of references, `()` values or tuple
comparisons.

Update (2026-10-29), methods and two-phase borrows (#7, #15, decision 53): inherent `impl`
blocks (any number per struct, before or after it) hold associated functions, methods
taking `self`, `mut self`, `&self` or `&mut self`, reached as `Type::Name` (two-segment
paths; `Self` and `Self::Name` inside a block) or with method syntax. The type checker
finds a method with rustc's receiver probe for inherent impls (by value, then `&`, then
`&mut` at each auto-deref step; a reference matched by value is reborrowed); path calls
adjust nothing. MIR building evaluates the receiver before the arguments (decision 36:
`c.Plus(Change(&mut c))` passes the old `c`), and a `&self`/`&mut self` method's output
lifetime is `self`'s whatever its other parameters. Every function of an impl block is a
`public static` method of the struct's own value type with the receiver as first
parameter (decision 32's lowering), called directly; the IL of a method call is a plain
`call`, which the JIT inlines as it does any small static method. Two-phase borrows (decision 54) cover rustc's two cases, autoref'd
`&mut self` receivers and the implicit reborrow of a `&mut` call argument: MIR marks them
(`BorrowKind::Mut { two_phase }`), and the loan facts treat them as reserved (a shared
borrow that also conflicts with other mutable loans) until the call activates them. New
codes: E0110 (misplaced `self` parameter), E0210 (duplicate associated function, rustc's
E0592), E0211 (impl for a non-struct, E0390), E0212 (unknown associated function, E0599),
E0213 (`self` outside a method, E0424), E0319 (unknown method, E0599), E0320 (method used
as a field, E0615). All 76 staged programs (50 verdicts, 13 run-time, 11 UI) and the tuple
known gap `ok_two_phase_reborrow_tuple_field` agree with rustc and moved into their
harnesses; `self_type.sabi` included, since `Self` is supported. The generator produces
methods (impl blocks, receivers of every kind, associated constructors, method syntax on
places, references, call results and temporaries, chains, and the two-phase shapes
`v.Add(v.Get())` and `Use(m, m.X)`), which removed its workaround for the implicit
two-phase reborrow; 2000 seeds at each of three bases (2300000, 6100000, 9400000) agree
with rustc on all five properties. A `methods` benchmark (structs with methods in a hot
loop; instance methods on structs in the C#) runs at parity with C# in release builds
(p50 in-process 1.00x, process 1.06x) and allocates nothing; checked builds are 1.23x
in-process, the cost of 64-bit overflow checks through runtime helpers (decision 43) that
checked `fibonacci` shows too, not of the calls. The other benchmarks did not move. Gaps:
associated constants (`tests/pending/methods/known_gap/associated_consts.sabi`, E0107;
since done, #48),
`&&P` and `&mut &mut P` receivers at run time (`known_gap/double_ref_receivers.sabi`: borrow
checking accepts them, code generation still rejects `&&T` with E0500 until `Ref<T>`; since
lowered, #11),
trait impls (Phase 6, with `impl Drop` first, decision 46), generic impls, `self: Type`
receivers, methods on primitives, tuples and references (E0211), methods returning `!`
(`!` cannot be written in a signature yet), and C#-facing instance methods (Phase 8 export
shape). `return.Foo()` parses as a method call on an operand-less `return` (E0319, rustc's
E0599, as `(return).Foo()` already was): `return` and `break` take an operand only when
the next token can begin an expression, rustc's `can_begin_expr` (#50; it did not parse
before).

Update (2026-10-29), drops (#12, #37, decision 55): `impl Drop for S { fn drop(&mut self) }`
is accepted for the structs of the file, the only trait impl so far, with rustc's checks
(new codes E0214 target, E0215 duplicate impl, E0216 missing `drop`, E0217 other member,
E0321/E0322 signature, E0323 explicit call, E0324 `Copy` with a destructor, E0418 move out
of a `Drop` type, rustc's E0509, now in the differential harness's borrow family). MIR
building emits a `Drop` at every scope end, early exit and statement end (edition 2021
temporary scopes then, edition 2024's since decision 74: condition temporaries, extended
`let r = &temp`, tail temporaries before the block's locals) and nests cleanup scopes over the blocks where a
panic must drop something. The borrow checker sees those drops (a drop invalidates loans
but is not a use of the references a value holds). Drop elaboration, a new MIR pass after
borrow checking, makes them exact from initialisation dataflow: drop flags where a value
is maybe initialised, per field of plain structs and tuples after a partial move,
`old = move p; p = new; drop(old)` for assignments, and the cleanup scopes' drops at the
points that may panic. Code generation emits drop glue as one static method per type and
each cleanup scope with drops as a `.try`/`fault` region; functions without drops on a
panic path have no exception handling, and the benchmarks' IL is unchanged apart from the
storage markers now removed after copy forwarding. A panic while unwinding aborts as in
Rust (decision 56): cleanup handlers mark the thread, and the entry point's exception
filter, which runs before any handler, fails fast when the mark is set. #37 landed first:
`let _ = place;` is a MIR `PlaceMention`, a liveness use only. All staged programs (26
verdict pairs, 18 run-time programs, 9 UI programs) and both known gaps
(`let_underscore_place`, `panic_in_drop_while_unwinding`) agree with rustc and moved into
their harnesses (the run harness now compares aborts: exit 134 on every platform and the
first panic report); `move_out_of_drop_type` became three E0418/E0509 verdict pairs. The
generator gives half of its non-`Copy` structs an `impl Drop` that prints the struct's name
and a field, never moves out of them, and counts drops as effects when reordering; 2000
seeds at each of three bases (3100000, 5700000, 8300000) agree with rustc on all five
properties. The runs found two compiler bugs and a generator one, all fixed: the reorder
analysis treated a shared reborrow of a `&mut` (`let d = &*m;`) as a read-only alias and
moved a read of the borrowed local past a use of `d`; temporaries were dropped in
declaration order rather than in the order their values were completed (`Make(1).Next();`
dropped `Make(1)` first; `run/drops/temporary_order.sabi`), and, pre-existing since
Phase 4, the backend folded `-C` for a constant `C` holding the type's minimum with
wrapping arithmetic, so a debug build did not panic; MIR building now folds a negated
literal into one constant, as rustc does, and only a constant item is negated at run time
(`run-pass/panics/negate_const_min.sabi`). Gaps: other trait impls (Phase 6);
`std::mem::drop` and `Box`/`String` (Phase 7, decision 48; a by-value function parameter
drops early meanwhile); generic and
`Drop` types holding references (Phase 9: their drop would then be a use); the abort's
standard error after the first report differs from rustc's (.NET's fail-fast message);
Windows' fail-fast exit statuses are mapped to 134 from documentation, not yet observed in
CI; a C# caller catching a panic (Phase 8) must clear the cleanup mark. Performance: the
other benchmarks have no drop glue and stay free of exception handling; the new `drops`
benchmark allocates nothing but runs at about 2.5 times the C# time (p50, in-process,
release and checked). Most of it is one protected region: stripped from the IL by hand,
the program takes 400 ms instead of 920 ms for 10^8 iterations (350 ms with no drops at
all). With it, the JIT copies a struct literal's operand temporary (live in a cleanup
handler) into the aggregate through memory, two 8-byte stores then a 16-byte load, which
stalls store forwarding; building struct literals in place (decision 51 kept
`ldloca`+`initobj`+`stfld`) is the likely fix.

Update (2026-10-31), after nightly runs 11 and 12: drop elaboration emitted the entry
block's argument drop flag writes in `HashMap` order, so the IL differed between two
compilations of the same program (generated-determinism seed 891); the drop paths are a
`BTreeMap` now. The reorder property swapped a write into part of a value with drop glue
past a statement that may panic, which the unwind's `drop` observes (seeds 438 and 498, a
bug in the property's independence analysis, not the compiler). The three mutation scores
had dropped below their thresholds, mostly through tuples; new tests bring them to 94.9%,
95.6% and 98.8% (`docs/TESTING.md`), and one survivor listed as equivalent turned out to
be killable.

Update (2026-11-05), enums and `match` (#8, decisions 57 to 60): `enum` items with unit,
tuple and struct variants and explicit discriminants, variant constructors and literals,
`as` casts of fieldless enums, `impl` blocks and `impl Drop` on enums; patterns (literals,
ranges, bindings with `ref`/`ref mut`/`@`, wildcards and `..`, tuple, tuple-variant and
struct patterns, or-patterns) in `match` with guards, `if let`, `while let`, `let`-`else`,
`let` and parameters; exhaustiveness and reachability by a port of rustc's usefulness
algorithm (decision 58). Enums are value types with a tag and one payload field per variant
(decision 57, C#-facing names by decision 59, checked from C# by `tests/interop/enum_layout`);
matches lower to sequential candidates with rustc's guard semantics (fake borrows,
bindings by reference until the guard holds; a guard may move the scrutinee, decision 60),
and drop elaboration follows partial moves into variants behind drop flags and the
discriminant read at the drop (decision 91).
Everything staged in `tests/pending/enums/` landed (30 run-time programs in
`differential/run/enums/`, verdict pairs in `differential/{moves,loans,enums}/`, UI tests
per stage); the generator covers every construct (paragraph below) and mutation testing on
the changed lines leaves only equivalent survivors (the paragraph after). Benchmark
`enums`: Sabiko takes 1.5 to 1.9 times as long as C# in-process and allocates nothing; the
gap is the candidates re-reading the tag where a decision tree (as in rustc) would switch
once. Known gaps (`tests/pending/enums/known_gap/`): reference patterns `&p` and half-open
ranges (since done, #49), primitive associated constants in patterns (since done, #48), `==`
on enums (Phase 6), a match
through `&&T` (`Ref<T>`; since lowered, #11), string literal patterns (with
`&str`, #10), float range patterns (rejected with a misleading message; since supported,
#49), code that patterns
make unreachable was not borrow-checked (rustc's false edges), and an arm that tests a
scrutinee a failed guard moved out was E0406 (rustc tests once); both fixed by #47
(decision 72, update below). Moving a non-`Copy` field out
of a `Drop` enum was reported at the binding (E0418) where rustc reports E0509 at the
scrutinee; since #50 it is reported as rustc does, at the scrutinee, once per moved-from
place (each arm's variant is its own place), with a label at each binding that moves (each
by-value binding records the scrutinee's span, `LocalDecl::match_span`, rustc's
`opt_match_place`). C# still rejects names Sabiko allows side by side in one type (a field
and a method of one name; an enum method named like a payload field or `Tag`).

Update (2026-11-05), generator for enums and `match` (#8): `sabiko_testgen` declares
enums (unit, tuple and struct variants over primitives, tuples, structs and earlier enums,
some `Copy`, some with an `impl Drop` that matches on `self`, fieldless ones with explicit
discriminants cast with `as`, enum methods, enums in struct fields and tuples) and
generates `match` as a statement, a `let` initialiser and inline, `if let`, `while let`
over a state local, `let`-`else` and destructuring `let`s, with nested variant, literal,
range, or-, `..`, shorthand, `mut`/`ref`/`ref mut`/`@` patterns and guards, on places,
references and temporaries, moving out of the scrutinee where the scope model allows;
every `match` is exhaustive by construction. With the fixes below, 2000 seeds at each of
three bases (1000000, 2000000, 3000000) agree with rustc on all five properties. The runs
found six compiler bugs, all fixed with regression programs: a guard on a match through a reference made the
fake borrow's `&&T` temporary an E0500 (`run/enums/guards_through_references.sabi`), and
so did a guard reading a top-level binding of a `&T` or `&mut T` scrutinee (now a shared
reborrow of its target, `run/enums/guards_on_mut_references.sabi`); a `let` or `let`-`else` with a `ref`/`ref mut` binding did not extend
its initialiser's temporary (a false E0413), and a failing `let`-`else` ran its `else`
before dropping the initialiser's temporaries (`run/enums/if_let/let_else_temporaries.sabi`);
casting a fieldless enum that implements `Drop` was accepted (`ui/typeck/enum_casts.sabi`),
and so was `-(-128i8)`, whose literal rustc reads as `128i8` (only the outer of two
negations counts; `ui/typeck/literal_ranges.sabi`).
The reorder analysis now counts a `ref` binding as taking its scrutinee's address. Gap
found and not fixed: rustc borrow-checks code that patterns make unreachable (false edges
between match candidates, the `else` of an irrefutable `if let`/`let`-`else`) and Sabiko
does not, so an assignment to an immutable binding there is E0384 in rustc only
(`pending/enums/known_gap/unreachable_arms_borrowck.sabi`, seeds 2001893 and 3000551,
where a move in an unreachable arm made a later use an error in rustc only); MIR building
needs false edges. The generator makes no pattern unreachable (the arms before a cover
are guarded, a cover's arms are disjoint, an `if let` or `let`-`else` pattern surely
refutes; a validity test checks that Sabiko reports no unreachable pattern) and skips no
check after a `match`'s first arm or in an `else`. Since #47 (decision 72) MIR building has
the false edges and the generator makes unreachable arms and `else`s again.

Update (2026-11-05), mutation testing of enums and `match` (#8) on the lines the branch
changed (`docs/TESTING.md`): before the new tests, 81.4% (`sabiko_types`), 88.7%
(`sabiko_mir`), 87.3% (`sabiko_codegen`) and 80.0% (`sabiko_borrowck`), after them
97.3%, 99.1%, 94.5% and 100%, with only equivalent survivors left. The survivors found four
bugs, now fixed: a repeated variant name with explicit discriminants panicked
the type checker, a local as a range pattern bound was accepted and its function never
built (a `MissingMethodException` at run time; E0315 now), `match r.F` through a reference
`r` could omit an uninhabited variant rustc requires, and a pattern with an unresolved path
was also reported as non-exhaustive, refutable or unreachable. A guard may now move the
scrutinee out, as in rustc (decision 60). Gap: an arm after such a guard that tests the
moved scrutinee again is E0406, which rustc accepts (fixed by #47, decision 72: the
program is now `run/enums/move_scrutinee_in_guard_then_test.sabi`). Also noticed:
a float range pattern (`0.0..=1.0`, valid Rust) is rejected as "only `char` and numeric
types are allowed in range patterns", which names `f64` as not numeric.

Update (2026-11-05), arrays and slices (#9, decision 61): `[T; N]` types with constant
lengths (literals, `const` items, constant arithmetic; constants are now evaluated before
item signatures, so signatures use them, and a local in a length is E0220), array literals
`[a, b]`/`[]`, repeats `[x; N]` (`Copy` operand for `N >= 2`, E0334), indexing through any
number of references with a `usize` index (E0335, E0336), `.len()`, `&[T]`/`&mut [T]` with
unsizing coercions from `&[T; N]`/`&mut [T; N]` at every coercion site and `&mut [T]` to
`&[T]`. MIR has `Index` projections, `Len`, `Repeat`, array aggregates and pointer
coercions; the borrow checker never compares indices (`&mut a[0]` and `&mut a[1]`
conflict, as in rustc), treats an element as part of its array for mutability and
initialisation, and rejects moves out of elements (E0420, rustc's E0508, now in the
differential harnesses' borrow family). Arrays are inline-array value types and slices
spans; every element access checks its index explicitly and panics with rustc's message and
location, in release builds too, before the span's own check, which the JIT then removes in
`while i < s.len()` loops (one compare per iteration, `DOTNET_JitDisasm` checked). Arrays
with drop glue drop their elements first to last, also when one of them panics. Everything
staged in `tests/pending/arrays/` landed: 50 verdict programs in
`tests/differential/{loans,moves,mutability}/`, 24 run-time programs in
`tests/differential/run/arrays/` plus the known gap `arrays_of_enums` (it only needed #8),
14 UI programs in `tests/ui/{typeck,resolve}/`; slice patterns are now E0107. The `arrays`
benchmark (a sieve over `&mut [bool]`, insertion sort over `&mut [i32]`, a 64×64 matrix
product) runs at 0.96x (release) and 0.99x (checked) of C#'s time in process and 1.24x per
launch, allocating only for printing an `f64`. Known gaps (`tests/pending/arrays/known_gap/`):
range indexing and slicing (ranges are not expressions yet), slice patterns, `for` loops,
`==`/`<` on arrays and slices (Phase 6), `const` items of array type (E0315), slice
methods other than `len()`, arrays and slices of references (E0500 then; lowered since
#43, decision 77), and rustc's deny-by-default
`unconditional_panic` lint for constant out-of-bounds indices, which Sabiko did not run
(it had no constant-propagation lints, like `arithmetic_overflow`; both since #44, decision
73): such programs compiled
and panicked at run time. `.get(i)` moved to Phase 6 with `Option` (decision 47).

Update (2026-11-05), generator for arrays and slices (#9): `sabiko_testgen` declares
`[T; N]` of primitives, tuples, structs (`Copy` or not, with `impl Drop` or not), enums
and arrays, with lengths 0 to 4 spelt as literals or `usize` constants, as locals, fields,
enum payloads, tuple elements, parameters and returns, and `&[T]`/`&mut [T]` parameters,
returns and annotated `let`s coerced from references to arrays; it generates array
literals, `[]`, `[x; N]`, element reads, writes, compound assignments, borrows, method
calls and scrutinees with constant and computed indices (sometimes out of bounds or
overflowing), `.len()` and `while i < s.len()` loops (docs/TESTING.md). With the fixes
below, 2000 seeds at each of three bases (7000000, 8000000, 9000000) agree with rustc on
all five properties. The runs
found four compiler bugs, all fixed with regression programs: `*s` for a slice reference
was E0208 even where it is only borrowed, indexed or measured (`&*s`, `(*s)[i]`,
`(*s).len()`; `run/arrays/slice_places.sabi`); an out-of-bounds index under a borrow
(`&mut a[i]`) or in the initialiser of a `let` with a `ref` binding panicked at the `&` or
the pattern instead of at `a[i]` (`run/arrays/index_out_of_bounds_borrow.sabi`,
`index_out_of_bounds_ref_binding.sabi`); `[x; 0]` dropped `x` at the end of its statement
instead of at once (`run/arrays/drops_zero_repeat.sabi`); and a failing `let`-`else` with a
`ref` binding left its initialiser's cleanup scope open, so a value with drop glue made in
its `else` block crashed MIR building (`run/enums/if_let/let_else_ref_binding_return.sabi`,
not specific to arrays). The generator leaves out the known gaps (slice patterns, ranges,
array comparison, arrays of references, `for` loops).

Update (2026-11-05), mutation testing of arrays and slices (#9) on the lines the branch
changed (`docs/TESTING.md`): before the new tests, 75.7% (`sabiko_types`), 83.3%
(`sabiko_mir`), 82.0% (`sabiko_codegen`) and 65.2% (`sabiko_borrowck`), after them 100%,
98.8%, 96.6% and 91.3%, with only equivalent survivors left. The survivors found three
bugs, now fixed: `let s: &[_] = &a;` panicked the compiler (the coercion's target kept the
unresolved `_`), and two bounds checks were lost, so an index out of bounds did not panic:
an element read whose value is discarded (`a[i];`, deleted by copy forwarding) and the
read of a `()` element. Also noticed, and fixed: E0420's note named an element behind a
reference `*s[_]` where rustc writes `s[_]`.

Update (2026-11-05), strings and range slicing (#10, decision 62): ranges `a..b`, `a..`,
`..b`, `..`, `a..=b` and `..=b` parse as the whole index of `e[...]` (elsewhere E0107;
range values need `Range<T>`, Phase 6). Indexing a `&str`, an array or a slice by a range,
through references, is a `str` or `[T]` place that can be borrowed (`&s[1..]`, `&mut
a[..2]`), indexed (`a[1..][0] = 5`) or measured (`s[2..].len()`). MIR lowers it as rustc
lowers its `Index::index`/`IndexMut::index_mut` call: a borrow of the base (mutable where
the place is written, borrowed `&mut` or bound `ref mut`; not two-phase), unsized from an
array, then the bounds, then the new `Rvalue::Subslice`, whose reference borrows what the
base does, so the borrow checker needed no new rule. The backend checks the bounds inline
in rustc's order and slices the span; a failure panics with rustc's message ("start byte
index 7 is out of bounds for string of length 5", "range end index 9 out of range for
slice of length 5", ...) located at the `[`, as rustc locates its `Index` call, in release
builds too. `.len()` works on `&str`, the six comparisons on two `&str` (runtime
`Str.Equal`/`Str.Compare`, by UTF-16 code unit), and string literal patterns in `match`,
`if let`, `while let` and `let`-`else` (the usefulness port already handled them).
Strings stay UTF-16: lengths and indices count code units where rustc counts UTF-8 bytes,
so the corpora keep strings ASCII. Also fixed: a string literal holding a NUL was cut short
there by `ilasm`; it is emitted as a `bytearray` now (`run/strings/len`). Everything staged
in `tests/pending/strings/` landed and agrees with rustc: 39 verdict programs in
`tests/differential/{loans,moves,mutability}/` (among them `&mut a[a.len() - 1..]`, E0502,
the borrow being taken before the bounds), 48 run-time programs in
`tests/differential/run/{strings,arrays}/` (every panic message and location), 9 UI
programs in `tests/ui/{parse,typeck}/`. Known gaps: range values (`let r = 1..3;`),
comparing `str` places (`*s == *t`, E0208) and `&&str` operands (E0500 until #11, since
lowered), `&str`
in structs and enums (lifetimes, Phase 9) and arrays (since #43, decision 77), and every other `str` and slice method (`chars()`,
`is_empty()`, ..., #45).

Update (2026-11-05), generator for strings and range slicing (#10): `sabiko_testgen`
generates `&str` locals, parameters and returns (`fn F(s: &str) -> &str` by elision),
ASCII string literals with escapes, range places of strings, arrays and slices in all six
forms with constant and computed bounds (sometimes out of range, reversed or overflowing:
about 3.5% of borrow-clean programs end in a slicing panic), borrowed, indexed, measured,
sliced again and walked in loops, `.len()` and the six comparisons of `&str`, and `match`,
`if let`, `while let` and `let`-`else` on strings (docs/TESTING.md). 2000 seeds at each of
three bases (10000000, 11000000, 12000000) agree with rustc on all five properties. The
runs found two compiler bugs, neither specific to strings or ranges and both older than
#10, which the generator avoids with TODOs (#54): while unwinding, an evaluated operand with drop
glue of a block's tail is dropped after the block's locals instead of before them (`let a
= D { .. }; G { A: D { .. }, B: 10 / k }`), and an evaluated call or aggregate operand with
drop glue is dropped after a later operand's statement temporary instead of before it
(`Use(D { .. }, Get(&D { .. }, 0))`). Both look like an operand dropped in an enclosing
temporary scope rather than its own. They also found three generator bugs, fixed. Both
compiler bugs are fixed (decision 71): an operand with drop glue goes into a scope of the
call, aggregate or assignment that consumes it, as in rustc, and the generator's TODOs are
gone (`tests/differential/run/drops/unwind_tail_operand.sabi`,
`unwind_operand_temporary.sabi`). With the shapes generated again, 2000 seeds at each of
the bases 12000000, 17200000 and 22000000 agree with rustc; base 12000000 found one more
case of the same kind in destructuring assignment (seed 12001627,
`unwind_destructuring_assign_place.sabi`), fixed too.

Update (2026-11-05), mutation testing of strings and range slicing (#10) on the lines the
branch changed (`docs/TESTING.md`): before the new tests, 77.8% (`sabiko_types`), 90.7%
(`sabiko_mir`) and 91.7% (`sabiko_codegen`), with `sabiko_parser` and `sabiko_borrowck` at
100%; after them 100% everywhere but `sabiko_codegen`, whose two survivors are
equivalent. Among the killing tests: slicing is a point that may panic for drop
elaboration (`run/strings/drops_unwind_slice`).

Update (2026-11-05), the last comparison verdict gap (decision 63): rustc's
`PartialEq::eq(&r, ..)` borrows the left operand's reference *itself*, so
`r == { r = &y; r }` is E0506 there and was accepted here. MIR building now adds a shallow
fake borrow of a left place operand (`&fake shallow r`), mentioned once the right operand
ran; it conflicts with writes, moves and mutable borrows of the place and its prefixes, not
with accesses through it (the reborrow `&*r` checks those), and emits no code, so the IL of
every run-pass, run-time differential and benchmark program is unchanged but for local
names. `&str` operands, which are copied rather than reborrowed, get it too
(`s == { s = "b"; s }`). Twenty `tests/differential/loans/*compare*` programs agree with
rustc 1.99.1. No known comparison verdict gap remains.

Update (2026-11-05), references to references and to `()` (#11, decision 64): `&&T`,
`&mut &T`, `&mut &mut T` and deeper chains are lowered with two wrappers generated into
the module, ``'Ref`1'<byreflike T>`` and ``'RefMut`1'<byreflike T>``, `readonly ref
struct`s holding one `initonly !T&` field `Value`. A reference local that a borrow takes
the address of is declared as its wrapper (a borrowed reference parameter is copied into
one on entry), every reference element of a tuple is a wrapper field, and the referent of
a `&&T` is therefore a wrapper: a read is `ldfld Value` on its address, a write (a
retarget through `&mut &T`) stores a new wrapper. References nobody borrows stay bare
managed pointers, so programs without `&&T` compile to the same IL as before; `&&str` and
`&&[T]` point at the span value and need no wrapper. A `&()` (or `&&()`) is lowered as
`()` is: no storage, `Unit` as a parameter or tuple element, `void` as a return. MIR
already dereferenced comparison operands, so `&&T` and `&&str` comparisons needed no new
lowering (the shallow fake borrow of a compared reference, decision 63, is a `&&T`
temporary too, but emits nothing and gets no slot); the three known gaps staged by #6, #7 and #8 (`&&` receivers, a match through
`&&Enum`, auto-deref through `&&(..)`) now run. Everything staged in `tests/pending/refs/`
outside `known_gap/` landed and agrees with rustc: 27 run-time programs in
`tests/differential/run/refs/` and 23 verdict programs in
`tests/differential/{loans,moves,mutability}/`. `tests/interop/ref_wrappers` builds
`Ref<int>`, `Ref<Ref<long>>` and `RefMut<int>` in C# and passes them to Sabiko; C# cannot
read `Value` (CS0570), so wrappers are opaque to it. The generator produces double
references in about 45% of borrow-clean programs (docs/TESTING.md); 2000 seeds at each of
the bases 4000000, 5000000 and 6000000 agree with rustc on all five properties. They found
two borrow-check divergences, both fixed since: `&mut m` was rejected while a reborrow
through `m`'s shared deref lived (`let d = &*m; let a = &mut m;`, accepted by rustc
because a loan behind a shared reference does not conflict with `m` itself; #56, decision
69), and a guard fake-borrowed a scrutinee no pattern reads (`match p { _ if g => .., _
=> .. }` while `p` is mutably borrowed; older than #11; #57, decision 65). Known gaps (#58 unless noted): deref
coercion (`&&i32` to `&i32`, E0300), `'static` promotion of `&7` retargeted through `&mut &i32` (#106), explicit lifetimes on double
references (Phase 9), and arrays and slices of references (since #43, decision 77). Mutation testing of the
changed codegen lines (docs/TESTING.md) caught 91 of 98 viable mutants before the new tests and 94 after them;
the 4 survivors are equivalent, and the killing tests found no bug.

Update (2026-11-06), guard fake borrows follow rustc's set (decision 65, #57): a guard used to
fake-borrow the whole scrutinee, so `let r = &mut p; match p { _ if g => 1, _ => 2 };
r.A += 1;` was E0408 where rustc, whose patterns there inspect nothing of `p`, accepts it
(found by generator seed 5001979 while #11 was written, though older than it). MIR building now
collects rustc's set across every candidate of the match: each place a test reads, the
places before a `Deref` in it, and the places before a `Deref` in each binding's place;
every guard of the match takes all of them, and a match whose patterns test nothing takes
none. The loans are shallow, as rustc's are: they do not cover the places below their own
(a guard may write the payload of an enum whose variant it tested), and taking one is no
access: in rustc it conflicts with no loan, so a guard may write a tested place through a
`&mut` taken before the match (fixed after generator seed 12800375 found that Sabiko made it
a read). A range covering
its whole integer type (`0u8..=255`) now tests and reads nothing, as in rustc. Besides the
false errors, the old borrow missed one rustc error: the guard of `match *v { y if { v =
&b; .. } => .. }` may not retarget `v` (E0510), which Sabiko accepted. Twenty-four programs in
`tests/differential/loans/*guard*` agree with rustc 1.99.1; the MIR fixture
`guard_fake_borrows.sabi` pins the sets. The staged
`tests/pending/refs/known_gap/guard_fake_borrow_of_unread_scrutinee.sabi` is now
`tests/differential/loans/ok_guard_wildcard_scrutinee_mutably_borrowed.sabi`, and the
generator no longer withholds guarded arms from a mutably borrowed or moved scrutinee;
2000 seeds at each of the bases 10600000, 11700000 and 12800000 pass all five properties.

Update (2026-11-06), guards over a scrutinee reached through a reference (#61, after
decision 65): `match *m { E::A(n) if { m = &z; true } => n, _ => 0 }` printed `0` before
decision 65 and is E0419 since; #61 adds rustc-checked programs for the variant test, a
nested `**mm` (`*mm = ..`) and a tuple field `*t.0` (`t = ..`), a UI test and hand-built
MIR tests (`differential/loans/*match_guard*`, `ui/borrowck/guard_assigns_deref_scrutinee_ref.sabi`).
An access that conflicts with several of one guard's fake borrows (`&mut m` with those of
`*m` and `m`) is now reported once, as rustc does, not once per fake borrow.

Update (2026-11-06), after nightly run 18 (decision 66, #63): generator seed 960 failed the
run and reorder-run properties on an assignment of an array through an empty slice,
`d[7 % d.len()] = [..]`. MIR building used to evaluate the whole array into a temporary
before the place, so the panic in `%` dropped the array's elements in element order. rustc
evaluates the elements, then the place, and only then builds the array in it, so it drops
the elements as separate temporaries, in reverse. An array, repeat or tuple on the
right-hand side of an assignment is now built in the place after the place is evaluated, as
in rustc. A struct or variant literal is still built first, which is also rustc's order.
`run/drops/unwind_assign_{array,tuple,struct}.sabi` pin the three shapes. The MIR of
`p = (p.1, p.0 + p.1)` loses one temporary.

Update (2026-11-07), benchmark trend (decision 68): the nightly benchmark reports used to
live only in job summaries and 90-day artifacts. On Mondays the nightly benchmarks job now
also appends each benchmark's p50s to `benchmarks/results/history.json` with
`benchmarks/trend.py` and commits it with `benchmarks/results/trend.svg`: Sabiko/C# ratios
over time, steady state and end to end, in both configurations, as a geometric mean and per
benchmark. The other nights commit nothing. Gaps: no alert on a time regression (time is
still never gated), and the history starts with the first weekly run, so the earlier
hand-measured reports (`results/latest.md` and the paragraphs above) are not in it.

Update (2026-11-07), match borrow checking as rustc's (#47, decision 72): MIR has rustc's
`FalseEdge` terminator. After each candidate's tests a false edge leads on to its guard and
bindings and, for the borrow checker only, to the next candidate (for the last candidate of
an `if let`, `while let` or `let`-`else`, to the no-match path), so an arm or alternative
after an irrefutable one, and the `else` of an irrefutable `if let` or `let`-`else`, are
borrow-checked as in rustc: assigning an immutable binding there is E0384, a double move
E0382, a move there makes a later use E0382, and a local initialised only in the body of an
irrefutable `if let` or `while let` is possibly uninitialised after it (E0381). Drop
elaboration first turns false edges into `goto`s and simplifies, so that code never reaches
code generation. Within a match each place is now tested once along a path: a discriminant
read, and the value of a place several candidates compare, goes into a temporary that a
later candidate reuses when every path to it made the read. An arm after a guard that moved
the scrutinee out and failed therefore no longer reads it (it was E0406; rustc accepts),
while one testing a place some path has not read yet still reads it, as rustc does (E0382
on both sides). A `match` on an enum loads its tag once instead of once per arm (#51). The
staged `unreachable_arms_borrowck.sabi` became twelve verdict programs in
`differential/{moves,mutability}/*unreachable*`, `*irrefutable*` and `*guard_moves*` (two
of them `ok_`), and `move_scrutinee_in_guard_then_test.sabi` a run-time program in
`run/enums/`. The generator now makes early unguarded arms (sometimes irrefutable, so every
later arm is unreachable), possibly irrefutable `if let` and `let`-`else` patterns, repeated
string literals and unguarded string bindings before the last arm; 1500 seeds at each of
the bases 47000000, 48000000 and 49000000 pass all five properties.

Update (2026-11-07), constant-propagation lints (#44, decision 73): rustc's deny-by-default
`arithmetic_overflow` and `unconditional_panic` are now E0600 and E0601, reported when code
is generated, as rustc does, and allowed with `sabikoc -A <lint>` (run-pass programs that
overflow on purpose say `//@ allow: arithmetic_overflow`). `sabiko_mir::lint` ports rustc's
`KnownPanicsLint` onto Sabiko's MIR; where the two MIRs differ, MIR building moved closer to
rustc's (conditions, guards and the left operand of `&&`/`||` are control flow rather than
a `bool` temporary; `4..=4` is one test; literal tests order their edges as rustc's switch)
and the walk carries facts from switch edges so that a test a candidate repeats is decided
as rustc's decision tree decides it. A new generated-program property builds each program
with rustc with both lints denied: over 10000 seeds at four bases (1, 1000000, 12000000,
47000000) Sabiko reports no lint rustc does not, and misses at least one of rustc's in 34
programs; the hand-written corpus (`tests/differential/lints/`, `tests/ui/lints/`, checked
by `crates/sabikoc/tests/const_lints.rs`) agrees exactly. Gap: those misses, all where
Sabiko's MIR walks in another order than rustc's or keeps a value rustc's drops a block
earlier (documented in decision 73). Found on the way: `(_, _) = (x, [])`, which rustc rejects
(E0282, the type of `[]` is not inferred; generator seed 1002197), passed Sabiko's type
check and hit an internal error in MIR building; it is now E0313 on the `[]`, and the
generator writes a value assigned to `_` with a type of its own (#78).

Edition 2024 (#81, decision 74): the language and every rustc oracle moved from edition
2021 to 2024. Tail-expression temporaries are dropped before the block's locals (#82),
`if let` scrutinee temporaries before `else` (#83), `mut`/`ref` bindings under a
by-reference default binding mode are E0337 (#84), and the never type falls back to `!`
(#85). Two differential programs that tested 2021's by-value `mut` binding were removed
(now UI tests), and new ones pin 2024's drop order and the tail temporary that can no
longer escape its block. Temporary lifetime extension through a block's tail (#88, both
editions) and let chains (#86) landed later (decisions 78 and 79, below).

Operands of type `!` (#87, decision 75): `1 == return true` was accepted, because the type
checker coerced a `!` operand to the other operand's type; rustc rejects it in every
edition (no `i32: PartialEq<!>`). Operators now judge `!` operands as rustc's trait
selection does (SPEC 11.2, Operators); 92 of 93 probe programs over every operator agree
with rustc (the other compares tuples, not yet supported;
`tests/ui/typeck/never_operands.sabi`, `differential/never/ok_never_operands.sabi`).

Associated constants (#48, decision 76): `const` items in inherent `impl` blocks of structs
and enums, reached as `Type::Name` and `Self::Name` in expressions, patterns, array lengths
and other constants, and the primitive types' constants (`i32::MAX`, `u8::BITS`,
`f64::EPSILON`, `f32::NAN`; `char::MAX` is E0318 by decision 31). An impl constant is a
`DefKind::Const` member like an associated function is a `DefKind::Fn` one, so the constant
evaluator, patterns and MIR treat it as any `const` item; a primitive one gets a definition
of its own the first time it is used, typed and valued from a table. The two staged
programs moved to `differential/run/` with six more, the UI tests
`resolve/associated_consts` and `typeck/associated_consts` agree with rustc line for line
(but `char::MAX`, and a cycle reported where it closes), and the generator writes the
boundary of an integer type as `T::MIN`/`T::MAX` half the time. That found a bug as old as
constant patterns: a constant pattern looked through a reference like a literal, so
`match &mut k { Z => .. }` was accepted (rustc: E0308, constants do not adjust); it is E0300
now (`typeck/const_patterns_through_refs`). Sweeps of 400 seeds at bases 1, 3000000 and
52000000 agree with rustc.

Arrays and slices of references (#43, decision 77): `[&T; N]`, `[&mut T; N]`, `[&str; N]`,
`&[&T]`, `&mut [&mut T]`, `&[&str]` and arrays of tuples and slices holding references are
lowered. An element that holds a reference is stored as a reference place is (decision 64:
a reference in its `Ref<T>`/`RefMut<T>` wrapper, a span as it is), the array is the same
inline array marked as a `ref struct`, and a slice of such elements is a generated
`Slice<T>`/`SliceMut<T>` view with the span's members, since `Span<T>` cannot hold a `ref
struct`; both were checked in IL on .NET 10 first (decision 64 had assumed an inline array
could not hold one). The staged `arrays_of_references` moved to `differential/run/arrays/`
with six more programs (slices of references as parameters, sub-slices, repeats, nested
arrays, `&&T` and tuple elements, drops through unwinding, and the bounds and range
panics), all agreeing with rustc; `*a[i]` now locates its bounds check at `a[i]`, as rustc
does. The generator writes elements of temporary arrays of references (`["a", s][i]`, `[&x,
&*r][i]`, `[a, b, c][1..][k]`); arrays and tuples of references as locals and parameters
are #92. Its first sweep found a generator bug (a `&mut` local given where a shared
reference was wanted mixed the element types, seed 7000161). Nightly run 23's fuzzer found
a type checker bug from #87: an operation on a diverging variable decided after its result
was used (`let r = y + 1; let _: i64 = y;` with `r` returned as `i32`) was accepted and
built ill-typed MIR; it is E0300 now (`typeck/never_operand_result_used_first`).

Temporary lifetime extension (#88, decision 78): the operand of `&` in any of rustc's
extending positions of a `let` initialiser now lives as long as the bindings: the tail of
a block (`let r = { &Make(1) };`), the branches of an `if`/`else` or `if let`, the arms of
a `match`, the elements of a tuple or array, and the operand of an extending `&` (`let d =
&&&x;`, #58's second item; its staged program moved to `differential/run/refs/`). Before,
only `let r = &<rvalue>;` and tuples of such borrows were extended, and every other case was
E0413. A temporary extended under a block, `if` or `match` is declared, with its cleanup
scope, before that expression, so cleanup scopes still nest with one entry; one written on
a single branch gets a drop flag. New programs: ten compile-time `ok_`/`err_` ones in
`differential/loans/` (including the non-extending positions: a `break` value, a call
argument, an assignment, a block's local), six run-time ones on drop order and unwinding
in `differential/run/drops/`, `refs/` and `arrays/`, and a MIR snapshot; the generator
writes `let r = { &e };`, `if c { &a } else { &b }` and `match` forms in about half of its
programs. 1000 seeds at each of the bases 1, 8800000 and 61000000 found a reordering-analysis
bug (an extended temporary with drop glue swapped past a panicking statement, seed 8800445;
a borrow of a value with drop glue now counts as an effect) and two older lint
divergences, now #95 (a `match` arm a known variant rules out is still visited when a
pattern has `..`, seeds 8800051 and 8800299); everything else agrees with rustc. Two older
coercion gaps found on the way are #94 (an unsized block element of a coerced tuple fails MIR validation, and
`&[&[1, 2], &[3]]` as `&[&[i32]]` is E0300).

Status (2026-11-08): **Phase 5 complete** (#4, exit gate #14). Every exit criterion holds:
exhaustiveness UI tests (missing variants, unreachable arms, nested and constant patterns),
the C# enum layout test (`tests/interop/enum_layout`), drop-order tests against rustc
(reverse order, drop flags, unwinding), rustc's message for an out-of-range `a[i]`, and
differential programs and generator support for every construct of the phase, or a TODO
naming the issue for what the generator avoids (#92: aggregates of references as locals
and parameters; #58: three-level references). The fuzzers' token soup has `enum`,
`match`, `impl`, `=>` and `..`, and their corpora are seeded from the new programs.
Mutation scores re-measured by nightly run 23 (on `799a45b`): `sabiko_borrowck` 95.0%
(342 + 4 / 364, threshold 93), `sabiko_codegen` 95.8% (643 + 4 / 675, threshold 95) and
`sabiko_types` 97.4% (1173 + 1 / 1205), below its 98: 14 more survivors than run 20, in
code added since (associated and tuple constants, `!` operands). New tests kill 10 of the
31, which gives 1184 of 1205, 98.3%; the 21 left are equivalent or older pattern-checking
survivors (docs/TESTING.md, Mutation testing). Writing them found two bugs. A path
pattern naming an associated or primitive constant (`S::K`, `i32::MAX`, #48) was lowered
for the exhaustiveness check as a wildcard, so `match x { S::K => 1 }` passed as
exhaustive and reached the `unreachable` terminator at run time (a panic, not undefined
behaviour); it is E0328 with rustc's witnesses now. And the never-type fallback (#85)
bound a diverging tuple element to `!` after an integer literal had decided it (`let mut
u = (return 1, 2); u.0 = 5;` built MIR with a `5` of type `!`); it now leaves literal
variables to their defaults. The run's fuzz failure was #93's crash,
fixed before this status. Open items carried past the phase: #45 (slicing patterns, `for`,
slice methods), #49 (reference and half-open range patterns), #51 (two benchmark gaps),
#58 (deref coercion), #92, #94, #95, and let chains (#86, since landed: below).

Let chains (#86, decision 79), after the phase: `if let Some(x) = a && x > 0 && let Ok(y)
= F(x) { .. }`, and the same in `while`, were E0100. A `let` is now an expression inside
an `if`/`while` condition, and every one must be an operand of the condition's outermost
`&&` chain (E0102 otherwise, rustc's "expected an expression, found `let` statement");
each binds for the operands after it and for `then`. MIR building gives chains their own
lowering (a lone `if let` keeps its own): one scope for the condition, holding each
`let`'s scrutinee temporaries and bindings through `then`, every other operand branched
on in a scope of its own, and each failure dropping what the condition holds before
`else` or the loop's exit, which is rustc's order on every path, unwinding included. New
programs: nine compile-time ones in `differential/let_chains/` (scopes, moves and borrows
across operands, `mut` bindings, a `while` chain's borrows ending each iteration), five
run-time ones in `differential/run/let_chains/` (drop order on success and on each
failure point, `return`/`break`/`continue` out of a chain, a value-producing chain with
extended borrows, `else if` chains, a diverging scrutinee, three unwinding programs), UI
tests for the rejected shapes, scopes and operand types, and a MIR snapshot. The
generator writes chains in half of its `if let` and a third of its `while let`
statements. 1200 seeds at each of the bases 8600000 and 62000000 agree with
rustc except one: seed 8600015, an older borrow-checker gap the shifted random stream
reached, now #100 (a `match` guard reading the scrutinee while a `ref mut` binding holds
it is accepted). On the way, the generator stopped writing `&mut (d[i])` as an extended
borrow of a temporary (a place borrow; seed 279) and stopped binding a bare `&mut` or
`&mut &mut` local whole inside a loop it was declared outside (a move per iteration; seed
8600601). Open: `if let` guards in `match` arms, which rustc 1.99 accepts (#98), and a
constructor pattern against a `!` scrutinee, E0300 in Sabiko in every pattern position
(#99).

Match guards bind as rustc does (#100, decision 80): seed 8600015 showed Sabiko accepting
a guard that reads the scrutinee while a `ref mut` binding the arm uses holds it (rustc's
E0503). A matrix of 26 guard shapes found two more of the kind: a guard writing a place a
`ref` binding covers, or a place a by-value binding will copy (E0506). The bindings are
now made before the guard's fake borrows; a `ref`/`ref mut` one takes its arm's reference
then (the guard sees a `ref mut` one through `&&mut`), and a by-value one's reference
lives to the guard's end. All 26 shapes now agree with rustc. New programs: eleven in
`differential/loans/` and a run-time one (`run/matches/guard_ref_bindings`, a failed guard
on one or-pattern alternative before the next binds the same names); a `RefMut` case in
the `guard_fake_borrows` MIR snapshot. 1200 generated seeds at each of the bases 8600000
and 63000000 agree with rustc except one constant-propagation lint, seed 63000676, an older
divergence of #95's kind (a condition that is itself an `if` lets the lint visit a dead
branch), added to #95.

Coercions at block tails and through `&` (#94): a block or `if` whose tail an unsizing
coercion changed kept its own type (`&[i32; 2]`), so as an element of a coerced tuple or
array, or as a call argument, it was lowered into a temporary of that type and failed MIR
validation (a debug-build panic; `({ &a }, 1)` as `(&[i32], i32)`, `F({ &a })`, `[{ &a }]`);
it now has the coerced type, as a `match` already had. And an array literal borrowed where
a `&[T]` is expected now has `T` elements (rustc's rvalue hint), so `let c: &[&[i32]] =
&[&[1, 2], &[3]];` is accepted instead of E0300. New: a run-time program
(`run/arrays/coerced_block_and_nested_elements`) and a `CoercedTails` case in the
`arrays_coercions` MIR snapshot.

Constant-propagation lints Sabiko reported and rustc did not (#95). Two causes, both in
which locals hold values. A borrow in an arm an earlier arm subsumes (`Waiting(_, ..)`
before `Waiting(s, ref mut k)`) made the scrutinee unknown, so the dead first arm was
linted; rustc's decision tree has no such arm when it lints, so the borrow now counts only
in the blocks a walk reaches when repeated tests are decided by earlier ones alone
(`kept_by_tests`; no value decides an edge there, since rustc keeps both arms of `if
false`). And a shared borrow of a constant temporary holding a repeat expression (`[1.5f32;
2]`) is promoted by rustc, which Sabiko's promotion model did not know. The issue's own
minimal reproducers had been reduced with `--emit=metadata`, which skips rustc's MIR
lints; the seeds were regenerated with the generator at the commit that found them and
checked with full compilation. New: `differential/lints/ok_borrows_rustc_removes_or_promotes`
(also the cases both compilers still lint: a guard on the first arm, a borrow in `if
false`). 2000 generated seeds at each of the bases 8800000 and 63000000 agree.

Patterns against a diverging scrutinee (#99): `match return 1 { E::A(b) => b, E::B => 2 }`,
`if let E::A(b) = return 1`, a literal pattern, `while let` and let chains were E0300 (the
pattern checked against `!`), while rustc gives the scrutinee a fresh type the patterns
decide. They now follow rustc's `demand_scrutinee_type`, including its exception: with an
explicit `ref` binding the scrutinee's own `!` is matched, so `let E::A(ref b) = return 1
else { .. };`, which Sabiko accepted, is E0300 like rustc's E0308. A `while let` with a
diverging scrutinee had no lowering (it was never type-correct before) and now leaves at
the scrutinee, as `if let` does. New: `run/control/diverging_scrutinee_patterns` and the
UI test `typeck/never_scrutinee_ref_binding`.

The last verdict divergences of #26: `Print`/`PrintLine` now take references to printable
values, through any number of references (`PrintLine(&x)`, `&&s`, `&mut f`), as rustc's
`Display` for `&T` and `&mut T` does; the argument is dereferenced when the call is
lowered, so validation and codegen see the value as before. Constant promotion (`&4` as a
`'static` reference) needs static storage and moved to its own issue, #106. The `!`
fallback rows went to #85 and #87 earlier, and `(&return 1).X`'s message already names
`&!`. New: `run/strings/print_through_references`.

`if let` guards (#98, decision 81), the last open piece of the edition 2024 pivot (#81):
`p if let q = e && c =>`, which rustc 1.99 accepts, was E0102. A guard is now parsed as a
condition and lowered by the let-chain lowering in a scope of its own; where it holds,
what it holds is handed to the arm and lives to the arm's end, dropped after the arm's
locals and before the pattern's bindings; where it fails, it is dropped before the next
candidate. New: eight compile-time programs in `differential/let_chains/` (the arm's
bindings seen through references, `ref mut` bindings, moves only when the arm is taken),
three run-time ones in `differential/run/matches/` (drop order on every path, or-pattern
alternatives, two unwinding programs) and a MIR snapshot. The generator writes `if let
<literal> = <place>` guards, sometimes followed by `&&` and a comparison. 1200 seeds at
base 66000000 found a generator regression from #86: `stmt_if_let` took its snapshot of
moved locals before generating the scrutinee, so a scrutinee that moved something (an
inline `match sum { .., d => .. }`) left it usable in `else` (seed 66000512); the snapshot
is taken after the scrutinee again.

Generator coverage of aggregates of references (#92, in part): `[&T; N]`, `&[&T]`, `(&T,
U)` and `[&mut T; 2]` locals of primitive referents, in a block of their own (read,
written through, sliced, destructured). 1200 seeds at each of three bases found two older
constant-propagation lint divergences: the lint computed with an `f32` literal's `f64`
value (`(100.0f32 / 0.001f32) as u32` was 100000, rustc's 99999; fixed, with
`differential/lints/ok_f32_constants`), and #109 (a string-literal arm after a failed guard
is visited, as rustc's tree does not), and one older generator gap: a range of a range
whose bounds moved something was still measured, which repeats the base's text and moved
it twice (seed 69001088). Parameters of these types, longer-lived locals and `&str`/`&&T`
elements remain open under #92.

String-literal arms after a failed guard (#109): the constant-propagation lint linted `"b"
=> 4usize - b` in `match s { "a" if false => 1, "b" => .., _ => 0 }`, which rustc does
not, because its decision tree never tests `"b"` where `s == "a"` and the path it takes
wipes `b` first. The lint's facts now cover string-literal tests (`==`/`!=` against a
string constant, keyed per walk), so a later candidate's string test that an earlier one
decides takes only the edge rustc's tree would. New: `differential/lints/
ok_string_match_after_guard`. 3000 generated seeds at each of three bases agree except
seed 71001629, an older divergence in the order blocks are visited (#111).

Pattern gaps (#49, decision 89): reference patterns `&p` and `&mut p` (`&Cell::Value(n)`,
`let &(a, b) = &t;`, in parameters, `if let`, `while let`, `let`-`else`, nested and under
`@`), half-open ranges `a..`, `..=b` and `..b`, and float ranges, all as rustc 1.99 with
edition 2024 has them. A `&` pattern matches a reference of its own mutability without
looking through references (E0300 otherwise), resets the default binding mode, and is
E0337 under a by-reference one where the referent is a reference; MIR matches its
subpattern against the dereferenced place, so a by-value non-`Copy` binding under it is
E0407 (rustc's E0507), `ref mut` through a `&` is E0404 (E0596) and its loans and the
guard's fake borrows are those of `*r`. A half-open range compares its one bound; `..MIN`
is E0332 (E0579), `a..=` E0102 (E0586) and `&0..=5` E0100 ("ambiguous interpretation").
Float ranges compare as floats and use rustc's `F64Range` in the usefulness port: they
never make a match exhaustive, but a literal or range inside an earlier one is
unreachable; a NaN constant in a pattern (also inside a tuple constant) is E0332. A `..`
outside a tuple or tuple-struct pattern (`&..`, a whole pattern `..`, accepted before) is
E0100. Borrow-checker messages no longer name a place rooted in an unnamed local (a
parameter's pattern): "cannot move out of a shared reference", "cannot borrow data in a
`&` reference as mutable", "value" elsewhere, as rustc words them (`describe_any_place`);
a variant field behind a reference is named `r.0`, not `*r.0` (rustc's E0507 says "`r` as
enum variant `Full`"). A parameter's pattern binds from a mutable argument, as in rustc,
so `(ref mut a, b): (i32, i32)` is accepted (it was E0402). `'\0'..` tests nothing. The two staged programs
moved to `differential/run/enums/` with three more (every position of a `&` pattern,
half-open ranges of every integer type and `char`, float ranges with NaN and infinities);
18 verdict programs in `differential/{moves,loans,mutability,enums}/`, one lint program,
nine UI tests and a MIR snapshot. The generator writes half-open ranges (in covers, always
at the open ends of `usize`/`isize`, which it could not cover before), float literal and
range patterns (floats are now scrutinees too) and an explicit `&p`/`&mut p` at the top of
a pattern through a single reference. Moving the generator's draws showed that four
coverage thresholds in `sabiko_testgen`'s validity tests sat above their long-run rates
(measured over 3000 seeds) and had passed on the first 500 seeds by luck; they were set
from those rates. 400 seeds per property at each of the bases 4900000 and 7700000 agree with rustc (verdicts, runs, constant-propagation lints, reorderings).

Drop flags of enum variants (#133, decision 91): an enum dropped field by field (it holds a
value on some paths only, or was partly moved) had each variant's fields flagged from the
discriminant when the enum was written, so a write through a reference went unseen: `let r
= if c { &mut E::A(D) } else { &mut E::A(D) }; *r = E::B;` (generator seed 50241), or a
deferred `let mut x;` changed by `f(&mut x)` or a `&mut self` method, dropped a zeroed `D`
at the end of the scope, and an enum filled through a reference leaked its new fields. The
flags now only say "written and not moved out", and each drop of a part inside a variant
reads the discriminant when it runs, also in cleanup handlers, as rustc's open drop does.
The extended temporaries of the report were one route in; the bug was in drop elaboration.
New: `differential/run/drops/extended_enums_written_through` (`if`/`else`, `match`, `if let`,
nested blocks, tuples and arrays of borrows, nested enums, `&`, no drop glue),
`differential/run/enums/{,unwind_}drop_flags_written_through` (all three failed before) and
the MIR fixture `drop_variants`. 400 generated seeds at bases 50000, 7000 and 123456 agree.
After review: `drop_flags_{moves_and_rewrites,nested_and_generic}` (moves, reassignments
and writes in any order and in loops; nested and generic enums, enums in struct fields,
tuples and arrays) joined them, `CleanupDrop::places` makes every pass that walks, counts
or substitutes places see the guards too, and the validator checks each guard's
discriminant against the variant the dropped place selects. Left, as for #51's tag loads:
each part of an open drop reads its enum's discriminant again, and a `fault` handler loads
the tag once per guarded drop; one switch per enum, as rustc's `open_drop_for_multivariant`
makes, would read it once (a size and speed cost only where a drop is already open).

### Phase 5: Data types

Work
Order (decision 46): tuples and `()` values, then `impl` blocks and methods (with two-phase
borrows), then drop elaboration, then enums and `match`, so that match lowering is written
against drops rather than retrofitted.
- Tuples; `()` as a value with real storage where it is a parameter or field type. Done
  (#6, decision 52; see the status note above). Tuple constants (#50): `const` items may be
  tuples of constants, with `.N` fields in constant expressions (also array lengths); a
  use is a `ConstValue::Tuple` operand that codegen builds like a tuple aggregate, and a
  pattern tests it element by element and counts as its tuple pattern for exhaustiveness,
  as rustc expands constants into patterns (guards fake-borrow the tested elements).
  Arrays in constants wait for #45. With it, `let r = &A;` for a constant `A` extends the
  temporary holding `A`'s value (it was E0413: the path was taken for a place); rustc
  promotes it to `'static` instead, which only differs once the reference outlives its
  block (#26). Destructuring assignment (#50, decision 70): the parser reads a
  destructuring left side of `=` as a pattern whose places are `PatKind::Assignee`s, the
  resolver gives each a fresh local (or, for a unit struct or variant path, a path
  pattern), and type checking and MIR building treat it as rustc's desugaring `{ let
  (x, y) = e; a = x; b = y; }`, so every later pass sees an ordinary `let` and ordinary
  assignments. New codes: E0111 (`_` outside an assignee), E0112 (a struct literal's `..`
  without a base, rustc's E0797), E0113 (a second `..` in a tuple pattern, which `let`
  and `match` patterns had accepted). Not done: slice patterns on the left (`[a, b] =
  ..`, E0107 with slice patterns); E0418 on a destructuring `let` or assignment still
  points at the binding and names the right side's temporary (`_1.X`), where rustc points
  at the right side as it does for `match` (#71 did only `match`). The generator
  produces `(p, q) = (e, f);` over any two writable places (a place the rules forbid by
  its turn becomes `_`).
- `impl` blocks, methods with `self`, `&self`, `&mut self`, associated functions,
  method call syntax with auto-ref; two-phase borrows for autoref'd receivers and implicit
  `&mut` reborrows in argument position. Done (#7, #15, decisions 53 and 54; see the
  status note above), and associated constants (#48, decision 76).
- Drop elaboration (moved here from Phase 3 by decision 26, given a phase by decision 46):
  drop flags, drops at scope end in reverse declaration order, each droppable scope lowered
  inside `try/finally`. `impl Drop for S` is the one trait impl accepted before Phase 6,
  special-cased as a language item so drops can be observed and diffed against rustc.
  Done (#12, decisions 55 and 56, with `try`/`fault` regions; see the status note above).
- `enum` with unit, tuple and struct variants, non-generic (generic enums come with Phase 6,
  decision 47); `match` with patterns (literals, bindings, wildcards, tuple/struct/enum
  destructuring, guards); exhaustiveness and reachability checking (usefulness algorithm,
  as in rustc). Done (#8, decisions 57 and 58, with `if let`, `while let`
  and `let`-`else`; see the status notes above); match borrow checking as rustc's (#47,
  decision 72) done; `&p` patterns, half-open and float ranges done (#49, decision 89).
- `[T; N]` (via `InlineArrayAttribute`), `&[T]` / `&mut [T]` as spans, indexing, `.len()`.
  Done (#9, decision 61; see the status note above), range slicing with #10, and arrays
  and slices of references (#43, decision 77); slice patterns, `for` loops and slice
  methods other than `len()` remain (#45).
- `&str` as `ReadOnlySpan<char>`, string literals (`ldstr` + `AsSpan`, or RVA data),
  basic slicing. No owned string yet; `Gc<System::String>` moves to Phase 7 (decision 48).
  Done (#10, decision 62: slicing with ranges in index position, also of arrays and
  slices, `.len()`, comparisons, string literal patterns; see the status note above);
  range values wait for generics (Phase 6).
- `&T` in generic position lowers to a `ref struct` `Ref<T>` with a `ref T` field;
  generic types that may hold it are emitted with `allows ref struct`. This also lowers
  `&&T`. Done for `&&T`, `&()` and references in tuples (#11, decision 64; see the status
  note above): `Ref<T>`/`RefMut<T>` are generated per module with a `byreflike`
  parameter, ready to be a generic argument once Phase 6 has generic types. Arrays and
  slices of references too (#43, decision 77): a byref-like inline array of wrappers or
  spans, and generated `Slice<T>`/`SliceMut<T>` views where `Span<T>` cannot hold them.

Verification
- Exhaustiveness UI tests (missing variant, unreachable arm, nested patterns).
- Enum layout tests from C#: a C# test reads tag/payload fields of a Sabiko enum and
  matches on it (the first `tests/interop` harness). Done: `tests/interop/enum_layout`,
  run by `crates/sabikoc/tests/interop.rs` (docs/TESTING.md, Interop; decision 59).
- Drop-order tests (from Phase 4): reverse declaration order, conditional drops through
  flags, drops during unwinding, compared against rustc. Done (#12):
  `tests/differential/run/drops/` and the generator.
- Index out of range: run-pass test expects a panic with rustc's message for `a[i]`.
  Done: `tests/differential/run/arrays/index_out_of_bounds*.sabi` compare the full panic
  report with rustc's (#9).
- Differential tests and the program generator extended to every new construct. Done
  (#14; see the Phase 5 status above), with mutation scores re-measured (nightly run 23).

### Phase 6: Traits and generics

Tracking issue #118. Order (decision 82 for the design):
1. Generic functions and monomorphisation (#114). Done; see the status below.
2. Generic structs and enums (#115): `Pair<A, B>`, `Opt<T>`, `impl<T>` blocks; an
   instance holding a reference is a `ref struct`, as a tuple holding one is. Done; see
   the status below (decision 83).
3. Traits, impls and bounds (#116): `trait` with required and default methods and
   associated constants, `impl Trait for Type` (primitives included), coherence, `T:
   Trait` and `where`, supertraits, method resolution through bounds, `Copy`/`Clone`, and
   `Drop` as an ordinary trait (retiring decision 46's special case). Monomorphisation
   resolves each trait call to its impl's method. First part done (decision 84; see the
   status below), associated constants too (decision 85), and `Copy`/`Clone`/`Drop` as
   traits (decision 86). Done.
4. `core` prelude written in Sabiko (#13, decision 47): `Option`, `Result`, `?`, `unwrap`,
   `expect`, and `.get(i) -> Option<&T>` on slices. `?` converts errors through `From`.
   First part done: the prelude, `Option`, `Result`, `unwrap`/`expect` (decision 90; see
   the status below). Left: `?` with `From`, `.get(i)`.
5. Operator traits, comparisons and derives (#117): `Add`, `Sub`, ..., their `*Assign`
   forms, `PartialEq`/`Eq`/`PartialOrd`/`Ord` on user types and in generic code,
   arithmetic on references, `#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]`.

Moved out: `dyn Trait` goes to Phase 7, with `Gc`; the C# test that implements a Sabiko
trait goes to Phase 8, which may add opt-in reified exports (decision 82).

Verification
- UI tests: unsatisfied bound, ambiguous method, missing impl item, conflicting impl.
- Run-time differential tests for generic containers and trait dispatch; ILVerify across
  instances.
- An interop test in which C# consumes concrete instances (`Pair<i32, bool>`,
  `Option<i32>`).
- Differential tests and the generator cover every new construct, or each construct the
  generator avoids has a TODO naming its issue; mutation scores re-measured.

Status (2026-11-08): generic functions (#114, decision 82). `fn F<T, U>(..)` on free
functions and in `impl` blocks, called with inferred arguments or a turbofish (`Id::<i32>`,
`x.M::<T>()`, `_` allowed); generic arguments parse on any path segment and after a cast,
and the resolver reports them where nothing takes them (E0223) or in the wrong number
(E0222), with E0221 for a duplicate parameter. A type parameter is `Ty::Param`: it unifies
only with itself, is not `Copy` (a `T` used twice is E0406, as in rustc), cannot be
printed or used with operators until bounds exist, matches only bindings and wildcards,
and may have drop glue. MIR is built, borrow-checked and linted once per generic function;
each `Call` carries its generic arguments, and the borrow checker gives each argument one
region type that every occurrence of its parameter shares, so references flow through `T`
(`let r = Id(&y); y = 5; *r` is E0410, as rustc's E0506). `mono::monomorphize` then
replaces the generic bodies by the instances the non-generic functions reach, with drops,
cleanup scopes and moves fixed for the concrete types; an endless chain of instances is
E0502 at depth 128 or at 65536 type nodes of arguments, and a generic `Main` is E0501. Each instance is a static method named
after it (`'Swap<bool, &i32>'`). Tests: ten compile-time programs in
`differential/generics/`, seven run-time ones in `differential/run/generics/` (tuples,
arrays, `&str`, `&mut` arguments, a borrowed `T = &i32`, drops and unwinding through `T`,
generic methods), a lint one (`lints/ok_generic_functions`: each lint once per generic
body, as rustc), UI tests in `resolve/generic_params`, `typeck/generic_functions` and the
two codegen ones, a MIR and an IL fixture (`generics`), and the generator, whose programs
now and then get the helpers `GenId`, `GenPick` and `GenFirst` and wrap values and shared
references in them. Not yet: generic structs and enums (#115), bounds (#116).

Status (2026-11-08): generic structs and enums (#115, decision 83). `struct Pair<A, B>`,
`enum Opt<T>` and `impl<A, B> Pair<A, B>` blocks, whose parameters alias the type's own
(blocks for one instance or with repeated parameters are E0107 until traits); arguments
are required in types (E0222 "missing generics") and inferred or written after the type
or variant segment in expressions and patterns; an unused parameter is E0340.
Literals, constructors, unit variants and patterns instantiate the type, field types are
read at the instance's arguments everywhere (type checking, exhaustiveness, MIR
building, drop elaboration, the lints, code generation), and `#[derive(Copy)]` holds at
`Copy` arguments. The borrow checker's `RegionTy::Adt` gives an instance's fields its
arguments' origins, so a reference stored in `Opt::Some(&x)` or a struct field keeps its
loan through moves, matches and field reads. Code generation emits each instance as a
value type of its own (`'Pair<i32, bool>'`), a `ref struct` with wrapped reference fields
when it holds a reference, with the instances of its functions as static methods, and
monomorphisation makes a generic `impl Drop`'s `drop` per instance type. Tests: eight more
compile-time programs in `differential/generics/` (references in enums and structs,
moves, derived `Copy`, a returned instance), five run-time ones (constructors and
methods, drops and a generic `impl Drop`, unwinding, references in instances and in
arrays and tuples of instances, methods with their own parameters, `Self`, associated
constants, nested instances), UI tests `resolve/generic_types`, `typeck/generic_types`
and `typeck/generic_enum_exhaustiveness`, a MIR and an IL fixture (`generic_types`), and
the generator's `GenOpt<T>`, `GenBox<T>` and `GenOr` helpers. Open: the instance type
names (`'Pair<i32, bool>'`) are not C# identifiers, so the exit gate's C# interop test
needs Phase 8's naming of exported instances, or a C#-friendly mangling earlier.

Status (2026-11-08): traits, impls and bounds, first part (#116, decision 84). `trait`
items with required and provided methods and associated functions (any receiver, type
parameters of their own, `Self` in signatures), supertraits, trait `impl` blocks for
structs and enums of the file, their instances, primitives, tuples, arrays and type
parameters (`impl<T: Show> Loud for T`), with parameters of their own and bounds; bounds
`T: A + B` and `where T: A` on functions and `impl` blocks. The resolver gives a trait an
implicit `Self` parameter and each trait `impl` block a definition; `Type::Name`,
`Self::Name`, `T::Name` and `Trait::Name` reach trait members. The type checker collects
the blocks, checks them against their traits (E0216 missing, E0217 extra, E0321/E0322/
E0343/E0344 signatures, E0215 overlap, E0341 supertraits), finds methods by rustc's probe
order through impls and bounds (E0342 ambiguous, E0319 with a hint), judges each use's
bounds at the end of the body (E0341), lets a bound pick an integer literal's type when
one block fits, and requires sized type arguments (E0208, a gap since #114) and, in
provided methods, `Self` behind references (E0208, as rustc without `where Self:
Sized`).
Monomorphisation resolves every trait call at concrete types to the block's method or the
provided method at `Self`; code generation emits them in the module class as
`'<i32 as Show>::Show'`. Tests: ten run-time programs in `differential/run/traits/`,
eight compile-time ones in `differential/traits/` (references returned by trait methods,
by-value receivers, bounds), UI tests `resolve/traits`, `typeck/trait_bounds` and
`typeck/trait_impls` and `typeck/trait_self_sized`, a MIR and an IL fixture (`traits`), and the generator's
`TRAIT_HELPERS` (a blanket `GenKeep` with a provided method returning `&Self`, `GenSign`
for `i32` and `bool`, bounded `GenKept` and `GenSigns`), whose seeds found an older
borrow-checker gap, fixed here: a `match` on a field of a moved struct was not a use.

Status (2026-11-09): associated constants of traits (#116, decision 85). Traits declare
required and provided constants, trait `impl` blocks define and override them (E0216
missing, E0217 for a method where the trait has a constant or the reverse, E0321 for
another type), and `T::N`, `Self::N` and `Type::N` name them (`Trait::N` is E0347). A
constant is evaluated at the type a use names, as rustc does: at a concrete type in any
constant context or when a body's MIR is built, at a type parameter by monomorphisation
for each instance (`ConstValue::Unevaluated` in generic MIR); a failure is reported once
per constant and type with the instance named (E0316; E0503 at monomorphisation, like
E0502 only reported when code is generated), and a length or pattern depending on a
type parameter is E0345/E0346. Tests: the run-time programs
`differential/run/traits/associated_consts.sabi` and `generic_impl_consts.sabi`,
compile-time ones in
`differential/traits/` (`ok_trait_const_*`), UI tests `parse/const_without_value`,
`typeck/trait_consts`, `borrowck/trait_const_evaluation` and
`codegen/trait_const_evaluation`, MIR and IL fixtures (`trait_consts`), and the
generator's `GenSign`, which gained a required and a provided constant read as
`T::BIAS` in `GenSigns` and as `i32::UNIT`/`bool::BIAS` in bodies. The constants of a
generic struct's or enum's `impl` block are evaluated per instance the same way, so they
may read a bounded parameter's constants (`impl<T: Shape> W<T> { const M: u32 =
T::SIDES; }`; `differential/run/traits/generic_impl_consts.sabi`). Not yet: `<T as
Trait>::N` (a parse error).

Status (2026-11-09): `Clone`, `Copy` and `Drop` are traits (#116's last part, decision 86,
retiring decision 46; #116 is done). The resolver defines them as language traits after
the builtins (a user item of the same name shadows one); `impl Drop` is an ordinary
trait `impl` block whose `drop` code generation still emits in the type's class, for the
type's own parameters in order (E0348) and without bounds (E0349). The solver knows
rustc's built-in `Copy` and `Clone` (primitives, `()`, `!`, `&T`, tuples and arrays) and
reads `#[derive(Copy, Clone)]` as impls bounded on every parameter; user `impl Clone` and
`impl Copy` blocks for structs and enums of the file are checked (E0214 for other types,
E0215 with a derive, E0341 for `Copy` without `Clone`, E0417, E0324). Copy decisions
take the body's bounds, so `T: Copy` copies and a trait with `Copy` as supertrait has a
`Sized` `Self`. `.clone()` calls the type's `impl Clone`, and monomorphisation turns the
built-in or derived `clone` of a `Copy` type into a copy; that of a type that is not
`Copy` is E0500 until #117's derives generate field-by-field bodies. Tests:
`differential/run/traits/copy_clone_drop.sabi` and `copy_impls_and_clone_probe.sabi`,
compile-time programs
`traits/ok_copy_bound_parameter_used_twice`, `ok_clone_then_move` and
`err_clone_bound_parameter_moved_twice`, UI tests `typeck/copy_clone_drop_impls` and
`codegen/clone_not_copy` (the `Drop` UI tests now go through the trait machinery;
`drop_duplicate_impl` moved to `typeck/`), and the generator's `GenCopied<T: Copy>`,
`GenCloned<T: Clone>` and `.clone()` of `Copy` values. Not yet, after #116: generic traits
(#117), associated types, blocks for types holding references or unsized types, `where`
clauses on types other than parameters, `where Self: Sized`, bounds on struct and enum
parameters, `<T as Trait>::` paths, the derived or built-in `clone` of a type that is not
`Copy` (#117).

Status (2026-11-11): the `core` prelude, first part (#13, decision 90). `library/core/`
holds `option.sabi`, `result.sabi` and `fmt.sabi`, compiled into `sabikoc`
(`sabiko_parser::prelude`) and parsed after every program's file; the resolver gives the
prelude a namespace of its own and shows its `pub` items and `pub use` imports to the
user's file under the user's own items, so `Option`, `Some`, `None`, `Result`, `Ok` and
`Err` are in scope everywhere and a user item of the same name shadows one. Library files
may write `pub use Enum::{..};` (variants) and `!`; a lone identifier naming an imported
unit variant compares in a pattern. Methods with rustc's names and messages: `is_some`,
`is_none`, `unwrap`, `expect`, `unwrap_or` on `Option`; `is_ok`, `is_err`, `ok`, `err`,
`unwrap`, `expect`, `unwrap_err`, `expect_err`, `unwrap_or` on `Result`, whose panicking
methods print the other variant with a private `Debug` (integers, `bool`, `()`; E0341 with
a note for the rest) through the intrinsic `PanicDebug`, lowered to
`Panic::FailDebug`. `#[track_caller]` works on user functions and methods too: a hidden
last `string` argument carries the caller's location, which every panic in the body uses
(`Call::fn_span` gives the method name's position, as rustc reports it); on a trait's
method or `Drop::drop` it is E0107, on `Main` E0501. Instances of prelude types are named
`'core::Option<i32>'` in IL, so a user `Option` never collides; `--emit=mir` leaves the
prelude's bodies out; compiling a program does not borrow-check or lint them
(`check_core_prelude` does, once, in a test). After review: the resolver rewrites an
identifier pattern naming a constant, unit struct or variant (and such an assignee) into a
path pattern, so no stage special-cases it (a guard after a bare `None` arm could assign
the scrutinee: E0510 in rustc, now E0419); the orphan rule holds for the prelude's types
(E0211, E0214 at the user's `impl`, without follow-on errors from the rejected block);
E0502 through a prelude helper points at the user's call, once per chain; the harnesses
assert no diagnostic points only into the prelude; `#[track_caller]`/`#[intrinsic]` on a non-function is
E0224; diagnostics name `Option<..>`, IL `core::Option<..>`; a file `option.sabi` keeps
its module class `Option`. Cost: about 0.7-0.9 ms per compilation in a release build (a
hello-world from about 2.1 to 2.9 ms). Tests: run-time programs in
`differential/run/prelude/` (both types' methods, `Option<&T>`/`Option<&mut T>`/`Result<&T,
E>`, drops and unwinding, shadowing, and seven panics whose message and location must match
rustc's, through generic code and chains of `#[track_caller]` functions, an overflow and
an index out of bounds included), compile-time ones in `differential/prelude/` (loans held
by `Some(&x)`, moves by `unwrap`, copies of `Option<i32>`), UI tests
`typeck/prelude_debug_bound`, `typeck/prelude_variants`, `resolve/prelude_private_items`,
`resolve/track_caller_unsupported`, `resolve/prelude_orphan_impls` and
`codegen/exe/main_is_track_caller`, MIR and IL
fixtures (`prelude`), and the generator, whose programs with exactly one of the generic and
trait helper sets wrap values in `Some(e).unwrap()`, `Some(a).unwrap_or(b)`,
`None::<T>.unwrap_or(b)`, `Ok::<T, u8>(e).unwrap()` and the like, and shared references in
`Some(&*r).unwrap()` (1600 seeds at four base seeds; the failures were three older bugs,
filed as #132, #133 and #134). Not yet: `?` and `From`, `.get(i)` on slices (#13's second
part), `==` on options (`#[derive(PartialEq)]`, #117), `#[derive(Debug)]` (#117), `Debug`
for floats, `char`, `&str` and references, a bound naming `Debug` (Phase 9 paths), more
methods (`map`, `and_then`, `take`, ... need closures or `&mut self` patterns), and
`#[track_caller]` on trait methods.

Update (2026-11-11), mutation scores after the traits work (#125): nightly run 25 measured
`sabiko_types` at 92.0% over three of its four shards (the fourth lost its runner, and
four shards no longer fit the job limit, so the crate now runs as eight). New tests kill 56
of the 97 survivors, chiefly `sabiko_driver`'s `run_corpus`, which compiles every run-time
program to IL in process (the trait programs were only built by `sabikoc`, which this
crate's mutants do not run). That gives 96.6% on those shards. The 41 left are 37
equivalent mutants and 4 not yet killed (`docs/TESTING.md`, Mutation testing). At about
1620 viable mutants, 98% was not reachable with 37 equivalent ones. So the equivalent
mutants are now excluded in `.cargo/mutants.toml`, and dead `Ref`/`Slice` arms in
`could_be`, `match_ty` and `unify` are removed (no trait block's type holds a reference
or a slice). The threshold stays at 98.

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
- `#[track_caller]` functions (decision 90) take the caller's location as a hidden last
  `string` parameter, which an exported one shows C# today; rustc's caller location is
  invisible to callers, so decide the export shape (a wrapper without it passing `null`,
  or the parameter documented as optional).
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
seeds, mutation testing, benchmarks; Monday's nightly records the benchmarks' trend
(`benchmarks/results/trend.svg`, decision 68). `docs/TESTING.md`
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
