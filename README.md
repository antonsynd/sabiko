# Sabiko

**Sabiko** (錆 *sabi* "rust" + 子 *-ko* "child") is a small Rust-inspired language for
.NET. It compiles to verifiable CIL and checks ownership, borrowing and mutability at
compile time. The goal is to free Sabiko-owned memory deterministically, without the
garbage collector, and to call to and from C# with no marshalling.

```rust
enum Shape {
    Circle { Radius: f64 },
    Rect(f64, f64),
}

impl Shape {
    fn Area(&self) -> f64 {
        match *self {
            Shape::Circle { Radius } => 3.0 * Radius * Radius,
            Shape::Rect(w, h) => w * h,
        }
    }
}

fn Main() {
    let shapes = [Shape::Rect(3.0, 4.0), Shape::Circle { Radius: 2.0 }];
    PrintLine(shapes[0].Area() + shapes[1].Area());   // 24
}
```

Sabiko is a syntactic subset of Rust; the one difference is .NET naming (PascalCase items
and fields). That lets rustc act as the test oracle. Structs and enums become value types,
`&T`/`&mut T` become `in`/`ref` managed pointers, arrays become inline arrays, and slices
and `&str` become spans.

## Status

Phases 0–5 of [`docs/PLAN.md`](docs/PLAN.md) are done. **Phase 6, traits and generics, is
in progress.**

Supported today:
- Primitive types, tuples, structs, enums, arrays, slices and `&str`.
- Methods and constants.
- `match` with exhaustiveness checking, `if let`, `let`-`else`, let chains and `if let`
  guards.
- Drops at scope end and on unwinding.
- Rust's overflow semantics: a panic in debug builds, wrapping with `--release`.
- rustc's constant-propagation lints.
- A borrow checker built on Polonius, using facts derived from MIR.

Still to come: `Option`/`Result`, `Box`/`Gc`/`Cell`, and C# interop (Phases 6–8).

## Building

You need the pinned Rust toolchain and the .NET 10 runtime, but no .NET SDK.
`scripts/setup-dotnet.sh` (`.ps1` on Windows) fetches `ilasm`, ILVerify, the reference
assemblies and `csc` into `./tools`, and builds the runtime library.

```sh
scripts/setup-dotnet.sh && source tools/env.sh
cargo run -p sabikoc -- examples/shapes.sabi --emit=exe -o out/shapes.dll && dotnet out/shapes.dll
cargo run -p sabikoc -- examples/shapes.sabi --emit=mir   # or il, mir-elaborated, ast, pretty, tokens
cargo test --workspace
```

Other flags: `--emit=dll` builds a library, `--release` makes overflow wrap, and `-A <lint>`
allows a lint.

## Testing

`cargo test` runs four kinds of tests:
- UI golden tests and MIR/IL snapshots.
- Run-pass programs, verified with ILVerify and executed.
- Differential tests: each program is built with both Sabiko and rustc, and their
  verdicts, lints and output must match.
- Generated programs, also checked against rustc.

The nightly build adds fuzzing, mutation testing and [benchmarks against
C#](benchmarks/README.md). See [`docs/TESTING.md`](docs/TESTING.md), and for the compiler's
stages, the [plan](docs/PLAN.md), [decisions](docs/DECISIONS.md) and
[spec](docs/SPEC.md).

## Licence

Dual-licensed under MIT or Apache-2.0, at your option. See `THIRDPARTY.md` for notices.
