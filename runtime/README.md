# Sabiko.Runtime

The runtime support library that every Sabiko-compiled assembly references. It is written
in C# (sources in `Sabiko.Runtime/`, one file per type, namespace `Sabiko.Runtime`) and
built into `tools/runtime/Sabiko.Runtime.dll` (assembly name `Sabiko.Runtime`, version
`0.1.0.0`, unsigned). The committed probes under `tests/` pin its observable behaviour.

Generated code never touches `System.Console` directly: it calls into this assembly, which
owns the formatting rules, so a Sabiko program prints exactly what the same program compiled
by `rustc` prints (the differential tests depend on that).

## API

```csharp
namespace Sabiko.Runtime;

public sealed class PanicException : System.Exception
{
    public PanicException(string message);
    public PanicException(string message, string location);  // "file:line:column"
    public string? Location { get; }
}

public static class Panic
{
    public const int ExitCode = 101;                       // Rust's exit code for a panic
    [DoesNotReturn] public static void Fail(string message);            // throws PanicException
    [DoesNotReturn] public static void Fail(ReadOnlySpan<char> message); // same, for &str
    [DoesNotReturn] public static void Fail(string message, string location);
    [DoesNotReturn] public static void Fail(ReadOnlySpan<char> message, string location);
    public static int Unhandled(System.Exception exception);           // see below
}

public static class Checks
{
    // The failing side of the run-time checks (decision 43): each throws a PanicException
    // with Rust's message at `location` ("file:line:column"). [DoesNotReturn] and
    // [MethodImpl(NoInlining)], so the throw stays out of the generated methods.
    public static void AddOverflow(string location);        // attempt to add with overflow
    public static void SubtractOverflow(string location);   // attempt to subtract with overflow
    public static void MultiplyOverflow(string location);   // attempt to multiply with overflow
    public static void NegateOverflow(string location);     // attempt to negate with overflow
    public static void DivideByZero(string location);       // attempt to divide by zero
    public static void DivideOverflow(string location);     // attempt to divide with overflow
    public static void RemainderByZero(string location);    // ... the remainder with a divisor of zero
    public static void RemainderOverflow(string location);  // ... the remainder with overflow
    public static void ShiftLeftOverflow(string location);  // attempt to shift left with overflow
    public static void ShiftRightOverflow(string location); // attempt to shift right with overflow
    public static void Unreachable();                       // internal error: entered unreachable code
}

public static class Console
{
    // One Print (no newline) and one PrintLine ("\n", never Environment.NewLine) per type:
    //   sbyte short int long nint Int128  byte ushort uint ulong nuint UInt128
    //   float double  bool char  string ReadOnlySpan<char>
    public static void Print(T value);
    public static void PrintLine(T value);
}

public static class Arithmetic
{
    // Debug-build overflow checks for 64- and 128-bit integers (decision 43): the wrapped
    // result goes to `result`, the return value says whether the exact one overflowed. One
    // overload per type: long ulong nint nuint Int128 UInt128.
    public static bool AddOverflows(T a, T b, out T result);
    public static bool SubOverflows(T a, T b, out T result);
    public static bool MulOverflows(T a, T b, out T result);

    // `i128`/`u128` `as f32`/`as f64`, rounded once to nearest-even like Rust (the BCL's
    // Int128 -> float conversion rounds twice, through double).
    public static float ToSingle(Int128 value);
    public static float ToSingle(UInt128 value);
    public static double ToDouble(Int128 value);
    public static double ToDouble(UInt128 value);
}
```

`Panic.Unhandled` is called by the generated entry-point wrapper when an exception escapes
the user's `Main`. It writes `thread 'main' panicked at <location>:\n<message>\n` to stderr
(`thread 'main' panicked:\n<message>\n` when the exception has no location) and returns
`101`, which the wrapper returns from `Main` as the process exit code. Generated code only
throws `PanicException`s with a location (decision 43); the other rows cover exceptions
from .NET code. The message is:

| exception | message |
| --- | --- |
| `PanicException` | its `Message` |
| `System.OverflowException` | `attempt to perform arithmetic with overflow` |
| `System.DivideByZeroException` | `attempt to divide by zero` |
| any other `System.ArithmeticException` | `arithmetic error: <message>` |
| anything else | `<full type name>: <message>` |

IL signatures, as the code generator emits them (`[Sabiko.Runtime]` is the
`.assembly extern Sabiko.Runtime { .ver 0:1:0:0 }` reference):

```
call void  [Sabiko.Runtime]Sabiko.Runtime.Console::PrintLine(int32)
call void  [Sabiko.Runtime]Sabiko.Runtime.Console::Print(valuetype [System.Runtime]System.ReadOnlySpan`1<char>)
call void  [Sabiko.Runtime]Sabiko.Runtime.Panic::Fail(valuetype [System.Runtime]System.ReadOnlySpan`1<char>, string)
call void  [Sabiko.Runtime]Sabiko.Runtime.Checks::AddOverflow(string)   // then ldnull; throw
call bool  [Sabiko.Runtime]Sabiko.Runtime.Arithmetic::AddOverflows(int64, int64, int64&)
call int32 [Sabiko.Runtime]Sabiko.Runtime.Panic::Unhandled(class [System.Runtime]System.Exception)
```

Parameter types in IL: `int8 int16 int32 int64 native int uint8 uint16 uint32 uint64
native uint float32 float64 bool char string`,
`valuetype [System.Runtime]System.Int128`, `valuetype [System.Runtime]System.UInt128` and
`valuetype [System.Runtime]System.ReadOnlySpan`1<char>`.

## Formatting rules (Rust `Display`)

- Integers (128-bit ones included): decimal, `-` sign, invariant culture.
- `bool`: `true` / `false`.
- `char`: the UTF-16 code unit itself.
- `f64` / `f32` (`Format.cs`): the shortest digit string that round-trips (for `f32`, in
  `float` precision, so `0.1f32` prints `0.1`), never exponent notation (`1e21` prints
  `1000000000000000000000`, `1e-7` prints `0.0000001`: the shortest digits followed by
  zeros, not the exact binary expansion), no fractional part for integral values (`1`, not
  `1.0`), `-0` for negative zero, `NaN`, `inf`, `-inf`.
- Output goes through `System.Console.Out` as UTF-8 (the output encoding is switched to
  UTF-8 if it is anything else) with the default auto-flush, so stdout and stderr keep
  their relative order.

## Building

No .NET SDK is needed: `scripts/setup-dotnet.sh` (or `.ps1`) downloads the Roslyn compiler
(`csc.dll` from the `microsoft.net.compilers.toolset` NuGet package, which runs on the
bare runtime) into `tools/csc/`, writes the `tools/bin/csc` wrapper, and then runs
`scripts/build-runtime.sh` (`.ps1`), which compiles the sources with

```
csc -nologo -noconfig -nostdlib -target:library -optimize+ -deterministic
    -nullable:enable -langversion:latest -warnaserror
    -out:tools/runtime/Sabiko.Runtime.dll -r:<every dll in tools/ref> runtime/Sabiko.Runtime/*.cs
```

and then verifies the result with ILVerify; an unverifiable runtime fails the build. Re-run
`scripts/build-runtime.sh` after editing the C#. `tools/env.sh` exports
`SABIKO_RUNTIME_DIR` (`tools/runtime`).

## Tests

`scripts/check-runtime.sh` (`.ps1`) assembles the probes in `tests/` with `ilasm`, verifies
them with ILVerify (passing `Sabiko.Runtime.dll` as a reference), copies the runtime next to
the output and runs them:

- `format.il` calls every `Console` overload with awkward values; its stdout must equal
  `format.expected`, which was generated by `rustc` from the equivalent Rust program.
- `panic.il` panics at a location inside the entry-point wrapper shape; the exit code must
  be 101 and stderr must end with `thread 'main' panicked at src/probe.sabi:7:5:` and
  `boom`.
- `checks.il` fails an overflow check the way generated code does (`ldstr` location, `call
  Checks::AddOverflow`, `ldnull`, `throw`); the exit code must be 101 and stderr must end
  with `thread 'main' panicked at src/probe.sabi:3:9:` and `attempt to add with overflow`.

CI runs them on every platform before `cargo test`.
