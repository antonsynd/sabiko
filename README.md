# Sabiko

**Sabiko** (錆 *sabi* "rust" + 子 *-ko* "child") is a small Rust-inspired language for
.NET. It compiles to verifiable CIL, enforces ownership, borrowing and mutability at
compile time, frees Sabiko-owned memory deterministically without the garbage collector,
and interoperates with C# with zero marshalling.

Status: **Phase 2 complete.** The front end (lexer, parser, diagnostics), name
resolution, type checking with local inference, compile-time evaluation of `const` items,
AST to MIR lowering with a validator, and the mutability and initialisation checks are in
place; nothing is emitted for .NET yet (Phase 4). `sabikoc file.sabi --stop-after=resolve|typeck`
runs the pipeline up to a stage and reports that stage's diagnostics; `--emit=mir` prints the
MIR of every function.

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

Compiles to a `sequential` value type and a static method taking `ref Point`, callable
from C# as `Program.MovePoint(ref cursor, 5, -2)`.

## Building

Requires a stable Rust toolchain and the .NET 10 runtime plus `ilasm`/ILVerify.

```sh
scripts/setup-dotnet.sh        # installs the .NET toolchain into ./tools (see --help)
source tools/env.sh
cargo build
scripts/check-spike.sh         # assembles, verifies and runs the hand-written IL spike
cargo run -p sabikoc -- examples/hello.sabi --emit=ast   # parse a file, dump the tree
cargo run -p sabikoc -- examples/hello.sabi --stop-after=typeck   # resolve and type-check only
cargo run -p sabikoc -- crates/sabiko_mir/tests/fixtures/control_flow.sabi --emit=mir   # print MIR
```

Testing: `cargo test` runs everything, including the UI golden tests under `tests/ui`
(see `tests/ui/README.md`; `BLESS=1` regenerates expectations) and the snapshot tests
(`INSTA_UPDATE=always` or `cargo insta review` to accept changes).

On Windows use `scripts/setup-dotnet.ps1` instead.

## Layout

- `crates/` the compiler, one crate per pipeline stage
- `spike/` hand-written CIL used to validate the toolchain
- `scripts/` toolchain setup and helpers
- `docs/` plan, decisions log, language spec

## Licence

Dual-licensed under MIT or Apache-2.0, at your option. See `THIRDPARTY.md` for notices.
