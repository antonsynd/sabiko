# Sabiko

**Sabiko** (錆 *sabi* "rust" + 子 *-ko* "child") is a small Rust-inspired language for
.NET. It compiles to verifiable CIL, enforces ownership, borrowing and mutability at
compile time, frees Sabiko-owned memory deterministically without the garbage collector,
and interoperates with C# with zero marshalling.

Status: **Phase 4 complete.** The compiler runs end to end: lexer, parser, name resolution,
type checking with local inference, `const` evaluation, MIR lowering, the borrow checker
(mutability, initialisation, moves, and loans solved by the Polonius engine from
MIR-generated facts), and a CIL backend that emits textual IL for `ilasm`. Programs compile
to verifiable .NET assemblies that run on .NET 10 against the small C# runtime library
`Sabiko.Runtime` (printing, panics). Integer arithmetic has Rust semantics: panics on
overflow in debug builds, wrapping with `--release`. Two differential harnesses check every
program in `tests/differential/` against `rustc`: compile-time verdicts (accept versus
borrow error) and, for programs under `run/`, identical output and exit codes.

```rust
struct Point { X: i32, Y: i32 }

fn MovePoint(p: &mut Point, dx: i32, dy: i32) {
    p.X = p.X + dx;
    p.Y = p.Y + dy;
}

fn Main() {
    let mut cursor = Point { X: 10, Y: 10 };
    MovePoint(&mut cursor, 5, -2);
    PrintLine(cursor.X);
}
```

```sh
sabikoc hello.sabi --emit=exe && dotnet hello.dll   # prints 15
```

Compiles to a `sequential` value type and a static method taking `ref Point`, callable
from C# as `Hello.MovePoint(ref cursor, 5, -2)` (functions live in a static class named
after the module).

## Building

Requires the pinned Rust toolchain (`rust-toolchain.toml`) and the .NET 10 runtime.
`scripts/setup-dotnet.sh` fetches everything else from NuGet into `./tools`: `ilasm`,
`ildasm`, ILVerify, the reference assemblies and the C# compiler, and builds
`Sabiko.Runtime.dll` with it (no .NET SDK needed).

```sh
scripts/setup-dotnet.sh        # installs the .NET toolchain into ./tools (see --help)
source tools/env.sh
cargo build
scripts/check-spike.sh         # assembles, verifies and runs the hand-written IL spike
scripts/check-runtime.sh       # pins the runtime library's formatting and panic behaviour
cargo run -p sabikoc -- examples/hello.sabi --emit=exe -o out/hello.dll && dotnet out/hello.dll
cargo run -p sabikoc -- examples/hello.sabi --emit=il    # print the CIL module
cargo run -p sabikoc -- examples/hello.sabi --emit=mir   # print MIR
cargo run -p sabikoc -- examples/hello.sabi --stop-after=typeck   # resolve and type-check only
```

`--emit=dll` builds a library; `--emit=exe` adds the entry point and a
`runtimeconfig.json`; both place `Sabiko.Runtime.dll` next to the output. `--release`
switches integer arithmetic from panicking to wrapping on overflow. `sabikoc` finds
`ilasm` and the runtime in `./tools` (or via `--ilasm`, `--runtime-path`, `SABIKO_ILASM`,
`SABIKO_RUNTIME_DIR`).

Testing: `cargo test` runs everything: the UI golden tests under `tests/ui` (see
`tests/ui/README.md`; `BLESS=1` regenerates expectations), the IL and MIR snapshot tests
(`INSTA_UPDATE=always` or `cargo insta review` to accept changes), the run-pass tests under
`tests/run-pass` (compiled, verified with ILVerify, run, compared with `.stdout` files and
exit codes), and the differential tests against `rustc` under `tests/differential`
(compile-time verdicts, and identical output for the programs under `run/`). The tests
that need the .NET toolchain or `rustc` print a notice and skip when they are missing;
CI sets `SABIKO_REQUIRE_DOTNET=1` so they must run there. Beyond the hand-written corpora,
a seeded program generator is checked against rustc on every run, a proptest suite asserts
the pipeline never panics on mutated programs, and a nightly workflow adds libFuzzer
targets, thousands of generated seeds and mutation testing with enforced scores. See
`docs/TESTING.md`.

On Windows use `scripts/setup-dotnet.ps1` instead.

## Layout

- `crates/` the compiler, one crate per pipeline stage
- `runtime/` the C# runtime library `Sabiko.Runtime` and its IL probes
- `spike/` hand-written CIL used to validate the toolchain
- `scripts/` toolchain setup and helpers
- `docs/` plan, decisions log, language spec

## Licence

Dual-licensed under MIT or Apache-2.0, at your option. See `THIRDPARTY.md` for notices.
