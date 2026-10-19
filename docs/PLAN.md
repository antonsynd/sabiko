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
| Strings | `&str` = `ReadOnlySpan<char>` (Phase 5); owned `String` = native UTF-16 buffer freed deterministically (Phase 7); `System.String` only as `Gc<System::String>` with zero-copy `.as_str()` | Keeps every struct `unmanaged`; C# passes `string` to `&str` implicitly; one copy, no transcoding, to produce a .NET string. |
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
  sabiko_cil         CIL text model + emitter (.il), later PE writer
  sabiko_codegen     MIR -> CIL lowering, metadata (attributes, interop shapes)
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

### Phase 3: Borrow checker v1

Work
- Copy vs move types (primitives and `&T` are `Copy`; structs are move unless
  `#[derive(Copy)]`); use-after-move, move-out-of-borrow, partial moves, move in loop.
- Loans: shared and mutable, with NLL-style liveness so a borrow ends at last use, not
  scope end.
- Conflict rules: shared XOR mutable; writes/moves of a borrowed place; disjoint field
  borrows allowed (partial borrows, as in the Ferrite note); reborrows (`&mut *r`,
  `&*r`).
- Function boundaries without explicit lifetimes: Rust's elision rules (one input
  reference, or `&self`). Any signature elision cannot resolve is an error until
  Phase 9 adds explicit lifetimes.
- Returning references to locals rejected; references stored in struct fields rejected
  until Phase 9.
- Drop elaboration: insert `Drop` calls at `StorageDead` in reverse declaration order,
  drop flags for conditionally moved values.

Verification
- Compile-fail corpus (`tests/ui/borrowck/`): at least one test per rule above, in both
  the "must reject" and the adjacent "must accept" form (e.g. two shared borrows OK,
  mutable after last use of shared OK, disjoint fields OK).
- Differential testing against `rustc`: every `.sabi` program in `tests/differential/`
  is also valid Rust syntax (the harness prepends `#![allow(nonstandard_style)]` since
  Sabiko uses .NET casing). A harness runs `rustc --emit=metadata` on each and asserts
  "rustc rejects with a borrow error" iff "sabiko rejects with a borrow error". Phase 3
  is not done while this disagrees on the subset.
- Property: borrowck is deterministic and idempotent (same diagnostics across two runs,
  across statement reorderings that preserve semantics).

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

**Milestone A** (end of Phase 4): the examples from both design notes (`add_one`,
`move_point`, partial borrows) compile, verify, run, and the rejection examples are
rejected with the intended diagnostics.

### Phase 5: Data types

Work
- `enum` with unit, tuple and struct variants; `match` with patterns (literals, bindings,
  wildcards, tuple/struct/enum destructuring, guards); exhaustiveness and reachability
  checking (usefulness algorithm, as in rustc).
- Tuples, `[T; N]` (via `InlineArrayAttribute`), `&[T]` / `&mut [T]` as spans, indexing,
  `.len()`, `.get(i) -> Option<&T>` lowered to a length check.
- `&str` as `ReadOnlySpan<char>`, string literals (`ldstr` + `AsSpan`, or RVA data),
  basic slicing; `Gc<System::String>` with zero-copy `.as_str()`. No owned string yet.
- `&T` in generic position lowers to a `ref struct` `Ref<T>` with a `ref T` field;
  generic types that may hold it are emitted with `allows ref struct`.
- `impl` blocks, methods with `self`, `&self`, `&mut self`, associated functions,
  method call syntax with auto-ref.
- `core` prelude written in Sabiko: `Option`, `Result`, `?` operator, `unwrap`, `expect`.

Verification
- Exhaustiveness UI tests (missing variant, unreachable arm, nested patterns).
- Enum layout tests from C#: a C# test reads tag/payload fields of a Sabiko enum and
  matches on it.
- Index out of range: run-pass test expects `IndexOutOfRangeException` for `a[i]` and
  `None` for `a.get(i)`.
- Differential tests extended to enums/match/slices.

### Phase 6: Traits and generics

Work
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
- Full corpus green on both borrow checkers; `ilverify` clean; mutation testing with
  `cargo mutants` on `sabiko_borrowck` and `sabiko_types` with a target mutation score
  recorded in CI.
- Soak: fuzz the whole pipeline (not just the parser) for a fixed CI budget per run,
  asserting no compiler panic and `ilverify`-clean output for every accepted program.

---

## 4. Verification and testing strategy (cross-cutting)

| Layer | Technique | Where |
| --- | --- | --- |
| Lexer/parser | Unit tests, `insta` snapshots, `cargo fuzz`, `proptest` round-trip | `crates/*/tests`, `fuzz/` |
| Diagnostics | compiletest-style `//~ ERROR` annotations + golden `.stderr`; error codes stable | `tests/ui/` |
| Type and borrow checking | Must-reject and must-accept pairs; differential vs `rustc` on the shared subset | `tests/ui/`, `tests/differential/` |
| MIR | Validator pass after every transformation (debug builds); `--emit=mir` snapshots | `sabiko_mir` |
| Codegen | `.il` golden snapshots; `ilverify` on every artifact | `tests/codegen/`, CI |
| Runtime behaviour | Run-pass stdout comparison; panic exit codes; drop order; leak counters | `tests/run-pass/` |
| Memory model | Live-allocation counters, no-GC-region runs, RSS bounds | `tests/run-pass/memory/` |
| Interop | xunit projects in both directions; IL inspection for zero marshalling; layout equality; BenchmarkDotNet | `tests/interop/` |
| Compiler robustness | Whole-pipeline fuzzing; `cargo mutants` on checker crates | `fuzz/`, CI nightly |
| Regressions | Every fixed bug adds a test in the matching layer | all |

CI gates (every PR): fmt, clippy, unit tests, UI/codegen/run-pass suites on Linux,
Windows and macOS, `ilverify` clean, differential suite. Nightly: fuzzing budget,
mutation testing, benchmarks with trend tracking.

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

Open, to decide before the phase that needs them
1. .NET exceptions crossing into Sabiko (Phase 8). Proposal: propagate as panics;
   opt-in `#[dotnet(catch)]` on extern declarations converts to `Result<T, Gc<Exception>>`.
2. Nullability of `Gc<T>` (Phase 7/8). Proposal: non-null; `Option<Gc<T>>` lowers to a
   nullable reference; imports read nullable annotations.
3. Module to assembly mapping (Phase 9). Proposal: crate = assembly, module = namespace,
   free functions in a static class named after the module.
4. Collections (Phase 7). Proposal: `Vec<T>` native for `T: unmanaged`; GC collections
   only via `Gc<List<T>>`.
5. Syntax fidelity. Proposal: strict syntactic subset of Rust wherever possible (casing
   aside) so the differential tests against `rustc` keep paying off.
6. Threads, `Send`, `Sync`. Proposal: out of scope for 0.1.
