# Decisions log

Architecture decision records for Sabiko. Each entry: the decision, the reason, and
whether it is a one-way or two-way door. Dates are when the owner settled them.

| # | Date | Decision | Why | Door |
| --- | --- | --- | --- | --- |
| 1 | 2026-10-19 | Compiler written in Rust | Repo is a Cargo project; the Rust ecosystem has the lexing/parsing crates and the Polonius reference | Two-way early, one-way after Phase 2 |
| 2 | 2026-10-19 | Target ECMA-335 CIL, minimum .NET 10 | Managed pointers cannot be generic arguments; `&T` in generic position needs `ref struct` wrappers and `allows ref struct` (.NET 9+) | One-way for emitted libraries |
| 3 | 2026-10-19 | Emit textual `.il` and assemble with `ilasm` first; direct PE writer in Phase 10 | Readable output, golden tests, fastest bring-up | Two-way (the `.il` backend is kept for tests) |
| 4 | 2026-10-19 | Borrow checking on a MIR control-flow graph, NLL-style | Same architecture as rustc; AST-level checking does not scale to flow-sensitive loans | One-way |
| 5 | 2026-10-19 | Structs are CIL value types, `StructLayout(Sequential)`; `&T` = `in T`, `&mut T` = `ref T`; structs holding references are `ref struct`s | Zero-cost, byte-identical to C#, CLR backstops escape safety | One-way for exported APIs |
| 6 | 2026-10-19 | Heap ownership via `Box<T>` over `NativeMemory`, `T: unmanaged`; GC objects only through explicit `Gc<T>` | Deterministic free; GC is opt-in for interop | One-way |
| 7 | 2026-10-19 | Strings: `&str` = `ReadOnlySpan<char>`; owned `String` is a native UTF-16 buffer; `System.String` only as `Gc<System::String>` | Keeps every struct `unmanaged`; C# passes `string` to `&str` implicitly; one copy, no transcoding, to produce a .NET string. A GC-backed `string` primitive would make most structs non-`unmanaged` | UTF-16 vs UTF-8 is one-way; decide before Phase 5 (decided: UTF-16) |
| 8 | 2026-10-19 | Naming follows .NET conventions: PascalCase for types, traits, functions, methods, fields, variants, consts; camelCase for locals and parameters. Names export exactly as written; `#[dotnet(name = "...")]` per-item override; imported .NET names kept as-is | No mangling, one name in stack traces, reflection and re-import | One-way for published APIs |
| 9 | 2026-10-19 | Source extension `.sabi`; manifest `Sabiko.toml` | Short, pronounceable, no known collisions; `.sabiko` is the fallback | Two-way until public release |
| 10 | 2026-10-19 | Dual MIT / Apache-2.0 | Mirrors the Rust ecosystem; permits adapting rustc code | One-way |
| 11 | 2026-10-19 | Traits are .NET interfaces; generics are reified with constraints; `dyn Trait` only via explicit `Gc<dyn Trait>` | Shared with C#; boxing is never implicit | One-way |
| 12 | 2026-10-19 | `Drop` lowers to `try/finally`; panics are `PanicException` | Drops run during unwinding for free; C# can catch panics | Two-way |
| 13 | 2026-10-19 | No `unsafe` in v1; pointer-touching primitives live in the C# runtime assembly | Every Sabiko-emitted assembly stays fully verifiable, so the ILVerify gate is meaningful | Two-way (can add `unsafe` later) |
| 14 | 2026-10-19 | Integer overflow: panic in debug, wrap in release; explicit `wrapping_*` / `checked_*` | Rust semantics; differs from C#'s unchecked default | Two-way |
| 15 | 2026-10-19 | Enums are tagged value types with per-variant payload fields; overlap only when all payloads are unmanaged | Correct for generic payloads; layout optimised later | Two-way until exported |

## Open

See `docs/PLAN.md` §6 for the open questions and their current proposals.
