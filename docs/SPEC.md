# Sabiko language specification (skeleton)

Status: skeleton. Sections are filled in as each phase lands; the plan in `PLAN.md`
names which phase owns which section.

## 1. Lexical structure (Phase 1)
- Source encoding, whitespace, comments
- Identifiers and naming conventions (PascalCase items, camelCase locals)
- Keywords
- Literals: integer, float, bool, char, string
- Punctuation and operators

## 2. Syntax (Phase 1, extended by 5, 6, 9)
- Items: `fn`, `struct`, `enum`, `trait`, `impl`, `const`, `mod`, `use`, `extern "dotnet"`
- Outer attributes as in Rust: `#[path]`, `#[path(tokens)]` (or `[...]`, `{...}`) and
  `#[path = tokens]`, with delimiters that nest and match (E0100 otherwise); only
  `derive(Copy, Clone)` is interpreted so far
- Inherent `impl` blocks (Phase 5, #7): `impl Type { fn ... }`, any number per type,
  before or after the struct. The members are functions (attributes, doc comments and
  `pub` as on items); a member may take `self`, `mut self`, `&self` or `&mut self` as its
  first parameter (a method). A `self` parameter outside an `impl` block, or not first, is
  E0110. Not yet supported (E0107): generic impls, trait impls (`impl Trait for Type`)
  other than `impl Drop for Type` (§7), associated constants and types, `self: Type`,
  `&'a self`
- Paths: `Type::Name` (exactly two segments) names an associated function; `Self` (in a
  type, a struct literal, a path or as a unit-struct value) and `self` (a value) begin a
  path like an identifier. Longer paths and `::` in types and struct literals are E0107
  until modules (Phase 9)
- Method calls `e.Name(args)`; a struct literal may be a receiver outside `if`/`while`
  conditions (`Math { Base: 3 }.Fact(5)`), and `5.Triple()` lexes as `5`, `.`, `Triple`
- Statements and expressions
- Nesting is limited to 256 levels (`sabiko_parser::MAX_NESTING`, clang's default bracket
  depth); deeper input is a fatal E0109, where rustc has no fixed limit. A level is a
  nested expression (parentheses, a block's statement, a call argument, an `else if`), a
  prefix operator, each binary operator or cast of a chain, each postfix call, index or
  field access, or a `&` in a type; `&&` counts twice
- Patterns. Implemented (Phase 5, #6): `x`, `mut x`, `_` and tuple patterns `(p, ..)`
  (`(p,)` for one element, `()`), nested, in `let` and in parameters; `(p)` is `p`. Literal,
  `ref`, `&`, struct and enum patterns are E0107 until `match` arrives
- Types: primitives, references, tuples (`(A,)`, `(A, B)`, ...; `(A)` is `A`), arrays,
  slices, named types, generics
- Tuple expressions `(a,)`, `(a, b)`; `(e)` is a parenthesised expression. Element access
  `t.0` takes a decimal index as written; `t.0.1` (lexed as `t`, `.`, the float `0.1`) is
  two accesses, as in rustc, and a suffixed index (`t.0u8`) is a syntax error (E0100)

## 3. Names and modules (Phase 2, 9)

Associated items (#7): the functions of a struct's `impl` blocks live in the struct's own
namespace, not in the file's item namespace: `Point::New(1, 2)`, `p.Dot(&q)`. Unqualified,
they are not in scope, inside the block either (E0202, with a note naming `Point::New`).
Two of one name for the same struct are E0210, also across two blocks (reported, as rustc
does, at the later definition within one block and at the earlier one across blocks). A
field and a method may share a name (`c.Count` and `c.Count()`). Inside an `impl` block,
`Self` is the struct; outside one it is E0201. `self` is a value only in a method with a
`self` parameter (E0213 elsewhere, rustc's E0424).

## 4. Types (Phase 2, 5, 6)
- Primitive mapping to the .NET Common Type System
- Structs and layout
- Enums and layout
- References and the `ref struct` rule
- `unmanaged` and `Copy`
- Traits and interfaces, generics and constraints

## 5. Ownership and borrowing (Phase 3, 9)
- Moves and copies
- Loans, liveness, conflicts
- Partial borrows
- Lifetime elision, explicit lifetimes

## 6. Mutability (Phase 2)

## 7. Drop and panics (Phase 4, 5)

Panics (Phase 4): a panic is a `Sabiko.Runtime.PanicException`. `Panic(msg)` throws one;
integer overflow in a debug build, division or remainder by zero, `MIN / -1`, `MIN % -1`,
and a shift by an amount outside `0..bits` (negative amounts included) panic too, with
Rust's messages: "attempt to add with overflow" (`subtract`, `multiply`, `negate`),
"attempt to divide by zero", "attempt to divide with overflow", "attempt to calculate the
remainder with a divisor of zero", "attempt to calculate the remainder with overflow",
"attempt to shift left with overflow" (`right`). Every check is explicit (decision 43).
Each panic carries its source location, as Rust's do: the start of the expression whose
operator panicked (`a + b` in `(a + b) * c` is located at the `(`: parentheses widen the
span, as in rustc), or of the `Panic(...)` call. The file is named as it was given to
`sabikoc`; lines are 1-based; the column is rustc's display column, 1-based, counting a
tab as 4 columns, East Asian wide characters as 2 and combining marks and zero-width
characters as 0 (`SourceFile::rust_column`).
An executable's entry point runs `Main` inside a `try`: an exception that escapes is
reported on standard error as `thread 'main' panicked at file:line:column:` followed by
the message (`thread 'main' panicked:` for a panic without a location, which generated
code does not raise), and the process exits with code 101 (Rust's). Standard output
written before the panic is kept.
Release builds (`--release`) wrap integer arithmetic instead of panicking; division, shift
and explicit panics behave the same in both.

Drop (#12, decisions 46 and 55). A struct of the file may implement `Drop`, the one trait
impl so far: `impl Drop for S { fn drop(&mut self) { .. } }`, defining `drop` with exactly
that signature and nothing else (§11.1, §11.2). The method keeps Rust's lowercase name. A
type has *drop glue* when it is such a struct, or a struct or tuple with a field (element)
that has drop glue; such a type is never `Copy` (E0324, E0417). Values with drop glue are
dropped as rustc drops them (edition 2021):

- A value's own `drop` runs first, with its fields intact (it may read and change them);
  then its fields with drop glue are dropped in declaration order (not the order of the
  struct literal), recursively. A tuple drops its elements first to last.
- Locals are dropped at the end of their block, the last declared first (by declaration,
  not by initialisation: in `let a: D; let b = ..; a = ..;`, `b` goes first); parameters
  after the body's locals, the last parameter first. Shadowing drops nothing. `return`,
  `break` and `continue` drop the locals of every scope they leave, innermost first; the
  value they carry is moved out first. A loop body's locals are dropped every iteration.
- A moved-out value is not dropped where it was declared, but where it went: at the end of
  the callee for a by-value argument (`self` included), with the struct or tuple it was
  moved into. Where a value is initialised or moved on some paths only, it is dropped on
  the paths where it holds one (a drop flag), also for each field of a struct without
  `Drop` or element of a tuple: after `let b = p.B;` only `p`'s other fields are dropped,
  and assigning `p.B` again makes it count again.
- Assigning to a place that holds a value drops the old value after the new one is
  computed and stored: `x = Make(2);` makes 2, then drops the old `x`. Nothing is dropped
  where the place holds nothing (first or deferred initialisation, after a move). If the
  old value's `drop` panics, the place already holds the new value, and unwinding drops
  it. Assigning a field drops only the old field; assigning a partially moved struct as a
  whole drops only the fields still in it.
- Temporaries are dropped at the end of their statement, in the reverse order their
  values were completed: a call's result before the temporaries made for its receiver and
  arguments (`Make(1).Next();` drops `Next`'s result, then `Make(1)`'s). Except:
  an `if` or `while` condition's temporaries (and those of a `&&`/`||` operand) are
  dropped before the branch or body runs; `let r = &temp;` and `let t = (&temp, ..);`
  extend the temporary to the end of the block (`r = &temp;` does not: E0413, rustc's
  E0716); a block's tail expression's temporaries are dropped after the block's locals
  (a function body's after its locals and before its parameters); the operand of
  `return` is not a tail, its temporaries go first.
- `let _ = <value>;` drops the value at once, `let _x = ..;` at the end of the block, and
  `let (_, y) = (..);` drops the unbound element at the end of the `let`; an expression
  statement drops its value at its end. `let _ = place;` does nothing (§11.4).
- `drop` cannot be called (E0323, rustc's E0040), and there is no `std::mem::drop` yet:
  passing a value to a function that takes it by value drops it there.
- The implicit drop needs no `mut` binding. A drop at a scope end invalidates every loan
  of the value (E0413), an assignment that drops the old value every loan of the place
  (E0410), but a drop is not a use of the references a value holds: dropping a `(&T, D)`
  does not keep its loan alive. No non-`Copy` field may be moved out of a value whose type
  implements `Drop` (E0418, rustc's E0509); `Copy` fields can be read. Inside `drop`,
  `self` is `&mut S`.

Unwinding: a panic drops every value that holds one, frame by frame: the callee's locals
and parameters, then the caller's temporaries and locals, by the rules above. Operands
already evaluated for an unfinished struct literal, tuple or call are dropped, the last
first; the unfinished struct's own `drop` does not run. A `drop` that panics at a normal
scope end is an ordinary panic: that value's fields are still dropped, then everything
else live, as rustc's drop glue does. A panic while unwinding (a `drop` called during
cleanup that panics) aborts the process, as Rust does (decision 56): standard output is
flushed, both panics are reported on standard error, followed by `panic in a destructor
during cleanup` and `thread caused non-unwinding panic. aborting.`, and the process ends
with `Environment.FailFast` (SIGABRT on Unix; exit status 134 in a shell) without running
any further drop. One difference: rustc prints a panic's report when the panic starts,
before the drops; Sabiko prints it after them. Both go to standard error, so standard
output is the same.

## 8. Memory model (Phase 7)
- `Box<T>`, `Gc<T>`, `Cell<T>`, `RefCell<T>`, `Arena`
- Strings and collections

## 9. .NET interop (Phase 8)
- Import mapping (`ref`/`in`/`out`, classes, exceptions, nullability)
- Export shape (attributes, `IDisposable`, naming)

## 10. Lowering to CIL (Phase 4 onward)

Implemented by `sabiko_codegen` (textual IL assembled by `ilasm`; decision 3).

Methods (decision 53). Every function of a struct's `impl` blocks, method or not, is a
`public static` method of the struct's own value type, its receiver the first parameter:
`&self` is `[in] valuetype P&` with `IsReadOnlyAttribute`, `&mut self` a `valuetype P&`,
`self` a `valuetype P`, as any parameter of those types (decision 32). A call, method
syntax or `P::Name(..)`, is a direct `call ... 'P'::'Name'(..)`. A field and a method of
one name coexist (CLI member names are per kind). `Main` is the entry point only as a free
function.

Assembly and module layout. One source file is one assembly and one module, named after
the file stem; the assembly references `System.Runtime` and `Sabiko.Runtime`. Each `struct`
is a top-level `public sequential ansi sealed` value type extending `System.ValueType`
with one public field per Sabiko field in declaration order (an empty struct gets
`.size 1`). Functions are `public static` methods of one `public abstract sealed` class
named after the module in PascalCase (`hello.sabi` gives `Hello`; `Module` is appended if a
struct has that name). `const` items have no representation: their values are inlined.
Every user identifier is quoted in the IL. An executable additionally has a private
`<EntryPoint>` method (§7) that calls `Main`, which must take no parameters and return
`()` (E0501).

Types. `i8 i16 i32 i64 isize` are `int8 int16 int32 int64 native int`; `u8 u16 u32 u64
usize` are `uint8 uint16 uint32 uint64 native uint`; `i128`/`u128` are the value types
`System.Int128`/`System.UInt128` (decision 42); `f32`/`f64` are `float32`/`float64`;
`bool` is `bool`; `char` is `char` (a UTF-16 code unit, decision 31); a struct is
`valuetype Name`. `&T` and `&mut T` are managed pointers `T&`; in signatures `&T` carries
`modreq(System.Runtime.InteropServices.InAttribute)` and the parameter (or the return
value) the `IsReadOnlyAttribute` custom attribute, so C# sees `in T` and `ref readonly T`;
`&mut T` is a plain `ref T`. `&str` is `System.ReadOnlySpan<char>` by value. A tuple
(decision 52) is a `System.ValueTuple` instantiation, as in C#: arities 1 to 7 hold their
elements in `Item1`..`Item7`, a longer tuple is the eight-parameter `ValueTuple` whose
`Rest` holds the tuple of the remaining elements (`t.7` is `t.Rest.Item1`); a tuple with a
reference anywhere among its elements (also `&str`, or a nested tuple holding one) cannot
be a generic argument and is instead a `ref struct` (`IsByRefLikeAttribute`) generated
into the module, named after the Sabiko type (`'(&mut i64, &i64)'`), with fields
`Item1`..`ItemN` and a constructor taking the elements. `()` is `void` as a return type;
where a `()` value must be stored (a parameter, a struct field, a tuple element) it is a
`Sabiko.Runtime.Unit`, an empty struct; locals and temporaries of type `()`, `!`, a
reference to `!` (`&!`, `&&!`, ...) or a tuple with such an element have no storage
(decisions 32, 44 and 52), and assignments to them are dropped. No `()` value is ever
loaded or stored; a call or tuple-constructor argument of type `()` loads a zero-initialised
`Unit` local instead. Not lowered yet (E0500): `&&T` and `&()`, also as tuple elements.

Statements and expressions. After borrow checking, copy forwarding (decision 51,
`sabiko_mir::forward`) replaces a compiler temporary that holds a copy of a place
(`_t = copy p`, which MIR building makes for every operand, decision 36) by the place
itself at its single use when that use is later in the same block and nothing in between
can write the memory the place reads: no assignment to `p`'s local, `p`'s local never
borrowed, and, for a place read through a reference, no assignment through a reference or
to a borrowed local in between; calls end blocks, so none can run in between. Copies
nothing reads are deleted. `--emit=mir` prints the MIR before this pass, as the borrow
checker sees it; `--emit=mir-elaborated` after drop elaboration (decision 55), the pass
before it. The backend then walks the MIR (§11); every statement and terminator
becomes an instruction sequence. MIR locals become IL locals (`.locals init`, listing only
the locals the code uses) and MIR arguments IL arguments, except that a user local or
temporary assigned once (not by a struct literal; a tuple is fine) and read once by a whole-local operand,
never borrowed or projected, keeps its value on the evaluation stack when the statement
that reads it starts by loading it (decision 51): the `stloc`/`ldloc` pair is dropped.
Otherwise the value is stored after all (`stloc`, or `dup; stloc` when the reader loads it
twice, as a division's checks do). Values waiting on the stack are taken last-in
first-out, so `F(a) + G(b)` keeps both results there; the stack is otherwise empty between
statements and at block labels, except that values may stay on it into a block entered
only by falling through from its single predecessor (a call's continuation). An assignment
to the return place followed only by the return leaves its value for the `ret`. Nothing is
computed or read at a different time than without these steps. Blocks are emitted in
reverse postorder of a depth-first walk from `bb0` (a `switchInt`'s `otherwise` successor
falling through), so the only backward branches are loop back edges: the JIT compiles a
method with a backward branch as a loop (instrumented Tier0, OSR). A jump to a block that
only returns is emitted as `ldloc; ret` in place. A place is accessed through its address: `ldloca`/`ldarga` for a local or
argument, `ldflda` for a field (a tuple element past the seventh first goes through
`ldflda Rest`), the pointer value itself for `*r`; the final load or store is
`ldloc`/`stloc`, `ldfld`/`stfld` or `ldind.*`/`stind.*` (`ldobj`/`stobj` for structs and
tuples). The fields of a `ValueTuple` instantiation are referenced by their generic
declaration, as C# does (`ldfld !1` of `Item2`, whose declared type is the second type
parameter).
`&place` is that address; `&mut *r` is `r` itself. A struct literal is built in its
destination local with `initobj` and one `stfld` per field (none for a `()` field), or
staged through a temporary when the destination is a projection or is read by one of the
fields. A tuple is built on the stack, as Roslyn builds one: its elements are pushed left
to right and `newobj` calls the tuple type's constructor (an eight-parameter `ValueTuple`
first builds its `Rest` the same way); every element is read before the tuple is stored,
so `p = (p.1, p.0)` reads both elements first. Calls are `call`
with the callee's full signature; `Print`/`PrintLine` select the runtime overload by the
argument's type and `Panic` calls `Sabiko.Runtime.Panic::Fail`; a call that cannot return,
and `unreachable`, are followed by `call Sabiko.Runtime.Checks::Unreachable()`, `ldnull`,
`throw`. `if` is `brfalse`, other `switchInt`s a `beq`
chain; `return` loads the return local and `ret`.

Arithmetic follows Rust. In release builds `+`, `-`, `*` are the plain opcodes and
values narrower than `int32` (`i8 i16 u8 u16`), which compute at `int32` width, are
truncated after every operation with `conv.*`; negation is `neg`. In debug builds every
`+`, `-`, `*` and unary `-` (as `0 - x`) is checked explicitly (decision 43; a negated
integer literal is one constant already in MIR, as in rustc, while `-C` for a constant
`C` holding the type's minimum panics like any other negation): narrow types
compute exactly at `int32` width and `i32`/`u32` at `int64` width (operands widened with
`conv.i8`/`conv.u8`), and the result must equal its own truncation to the type (`dup; dup;
conv.i1; beq`, with `conv.i8` after the truncation for the 32-bit types); `i64`, `u64`,
`isize`, `usize`, `i128` and `u128` call `Sabiko.Runtime.Arithmetic::AddOverflows`,
`SubOverflows` or `MulOverflows`, which return whether the exact result overflowed and
store the wrapped one through a `T&` to a scratch local. A failed check, here and for
division, remainder and shifts below, loads its location and calls the check's throw
helper in `Sabiko.Runtime.Checks` (`AddOverflow`, `SubtractOverflow`, `MultiplyOverflow`,
`NegateOverflow`, `DivideByZero`, `DivideOverflow`, `RemainderByZero`,
`RemainderOverflow`, `ShiftLeftOverflow`, `ShiftRightOverflow`, each `(string location)`),
which throws `PanicException(message, location)` with Rust's message; the call is
followed by `ldnull; throw`, never reached, which ends the block for the verifier and
marks it cold for the JIT (`ldstr`, `call`, `ldnull`, `throw`: 12 bytes, decision 51). No
`.ovf` opcode is emitted. Division and remainder are `div`/`rem` (`.un` for unsigned) preceded by
explicit checks that throw a `PanicException` with Rust's message: a zero divisor, and for
signed types `MIN` divided by `-1`; a check is dropped when a constant operand rules it out
(`x % 7` needs none). The checks run in release builds too, as in Rust, and are needed
because the CLR computes narrow types at `int32` width, where `i8::MIN % -1` is 0. `!` on `bool` is `ceq 0`, on integers `not` followed by
re-narrowing for unsigned types. Shifts convert the amount to `int32`; debug builds first
compare it unsigned against the left operand's bit width and throw a `PanicException`
("attempt to shift left with overflow") when it is not smaller, release builds mask it with
`bits - 1`; `>>` is `shr` for signed and `shr.un` for unsigned left operands, and `<<` on a
narrow type truncates. Comparisons are `ceq`/`clt`/`cgt` with the `.un` forms for unsigned
integers, `bool` and `char`; `!=`, `<=` and `>=` negate the opposite comparison, using the
unordered forms on floats so that any comparison with NaN but `!=` is false. A comparison of
references is the comparison of the referents: MIR building evaluates each operand to a
shared reference temporary (a place operand is reborrowed, `&*r`, so a `&mut` is not
moved), and the comparison reads `*l` and `*r` (`ldind.*`) after both operands ran, as
rustc's `PartialEq::eq(&l, &r)` does. The referents therefore stay borrowed while the right
operand runs (`r == { *r = 3; s }` is E0410, rustc E0506). One divergence remains: rustc
also borrows a reference *variable* used as an operand, so `r == { r = &y; r }` is E0506
there and accepted by Sabiko (comparing the old referent with the new one); expressing that
loan needs a `&&T` temporary, which code generation does not support yet. Casts: integer
to integer `conv.*` chosen by the target, with sign extension decided by the source
(`conv.i8` from signed, `conv.u8` from unsigned sources); integer to float `conv.r4`/`conv.r8`
preceded by `conv.r.un` for unsigned sources; float to `i32`/`i64`/`isize` and their
unsigned counterparts rely on the CLR's saturating conversions (NaN gives 0); float to a
narrow type clamps with `System.Math.Clamp` first, because the CLR only saturates to 32
bits. Constants are `ldc.i4`/`ldc.i8` (with `conv.i`/`conv.u` for the native widths),
`ldc.r4`/`ldc.r8` (non-finite values by bit pattern), and `ldstr` converted with
`String::op_Implicit` for `&str`. `.maxstack` is computed exactly.

128-bit integers (decision 42). An `i128`/`u128` constant is `newobj
Int128::.ctor(uint64 upper, uint64 lower)`. Every operator is a `call` to the operator
method of `System.Int128`/`System.UInt128`: `op_Addition`, `op_Subtraction`,
`op_Multiply` and `op_UnaryNegation` in release builds (debug builds check them with the
`Sabiko.Runtime.Arithmetic` helpers, as for 64-bit integers);
`op_Division`/`op_Modulus` behind the same explicit zero and `MIN / -1` checks as the other
widths (comparing with `op_Inequality`); `op_BitwiseAnd`, `op_BitwiseOr`, `op_ExclusiveOr`,
`op_OnesComplement`; the six comparison operators. Shifts check (debug) or mask (release)
the amount as for the other widths, a 128-bit amount being compared as `u128` before it is
truncated to `int32`, then call `op_LeftShift`/`op_RightShift` (arithmetic for `Int128`,
logical for `UInt128`). Casts call `op_Implicit` (a widening that cannot lose information:
any integer into `Int128`, unsigned ones into `UInt128`) or `op_Explicit` (signed into
`UInt128`, which sign-extends; 128-bit to a narrower type, which truncates; `i128` to and
from `u128`, which reinterprets; floats into 128-bit, which saturates with NaN as 0).
128-bit to `f32`/`f64` calls `Sabiko.Runtime.Arithmetic::ToSingle`/`ToDouble`, which
round once to nearest-even (the BCL's `Int128` to `float` goes through `double` and can
round twice). `bool` and `char` convert as `uint8` and `char`.

Drops (decisions 55, 56). A struct's `drop` is the static method `'Drop.drop'` of its
type, taking the receiver as `S&` like any `&mut self` method. A type whose drop does more
than call its own `drop` gets drop glue, a static method `'<Drop>T'(T&)` of the module
class, emitted on first use: it calls the struct's `drop` (if any), then each field's
glue in declaration order, every step but the last inside a `.try` whose `fault` handler
runs the steps after it. A MIR `Drop` loads the place's address and calls the glue (or
`'Drop.drop'`). Drop flags are `bool` locals. Each MIR cleanup scope with something to
drop is a protected region: its blocks are laid out contiguously, in reverse postorder
with the region's entry first, inside `.try { .. } fault { .. }`, nested as the scopes
nest; the handler calls `Sabiko.Runtime.Panic::BeginCleanup()` and then drops the scope's
places, each behind its flag. Control leaves a region only by `leave` (a conditional
branch out of one goes to a `leave` trampoline, and a `return` inside one is a `leave` to
the return block), and no value waits on the evaluation stack across a region boundary.
A function that drops nothing on a panic path has no exception handling at all. An
executable's entry point catches through a filter, `Panic::Filter`, which runs before any
handler: it records the panic and accepts it, or aborts when the thread is already
cleaning up after one.

Verifiability. Every emitted assembly passes ILVerify with `ReturnPtrToStack` ignored
(decision 30): ECMA-335 has no rule that lets a function return a managed pointer it
received, so that error fires for every `fn ... -> &T`; the borrow checker (§11.4) ensures
such a function only returns a pointer into its parameter's referent. The run-pass tests
verify every assembly they run; a verification error is a compiler bug.

## 11. Static semantics (Phase 2 subset)

What the compiler enforces today for the Phase 1 language subset (`fn`, `struct`, `const`,
no generics, enums or arrays; tuples since #6, inherent `impl` blocks and methods since
#7). This section is folded into §3, §4 and §6
as they fill in. Error codes are in `crates/sabiko_diagnostics`; each rule has a UI test
under `tests/ui/resolve` or `tests/ui/typeck`.

### 11.1 Name resolution

- Items (`fn`, `struct`, `const`) are visible everywhere in the file, including before
  their definition.
- Items share one namespace: a struct, function and constant may not have the same name
  (E0200). Field names must be unique within a struct (E0204) and parameter names within
  a parameter list (E0205).
- A name may be bound once per pattern: `let (x, x) = t;` is E0209 (rustc's E0416), and
  in a parameter list, across all parameters' patterns, E0205.
- A `let` binding is visible from the statement after it to the end of its block and
  shadows earlier bindings and items of the same name. Its initialiser is resolved before
  the binding is introduced, so `let x = x + 1;` reads the outer `x`. A local is not
  visible before its `let` or outside its block (E0202).
- Parameters live in the function's outermost scope, which the body block shares.
- Primitive type names (`i32`, `bool`, ...) resolve last, so `struct i32;` shadows the
  primitive within the file. A primitive name is never a value (E0202).
- A name used in the wrong position (a function as a type, a constant in a struct literal,
  a non-unit struct as a value) is E0203. `_` as a type is allowed only inside bodies
  (E0207). A path `A::B` in value or call position names the associated function `B` of
  the struct `A` (E0201 or E0203 when `A` names no type, E0212 when the struct, or a
  primitive, has no such function); other paths with `::` are rejected until modules
  arrive (E0107).
- An `impl` block is for a struct of the file (E0211 for a primitive, a reference, a
  tuple or `()`; E0201/E0203 for an unknown name or a non-type). Its functions are
  resolved like free functions, with `Self` naming the struct and, in a method, `self`
  bound as the first parameter (mutable for `mut self`).
- `impl Drop for T` (§7): `T` must be a struct of the file (E0214 for a primitive, a
  tuple, a reference or `()`, rustc's E0117/E0120; E0201 for an unknown name), with one
  such impl (E0215 at the second, rustc's E0119), which defines `drop` (E0216, E0046) and
  nothing else (E0217, E0407; `fn Drop` gets a note that the name is lowercase). Its
  members are not associated functions: `T::drop` and `t.drop()` find an inherent `drop`
  first and the destructor otherwise (then E0323). An item of the file named `Drop`
  shadows the trait: `impl Drop for T` is then E0203 (rustc's E0404).

### 11.2 Types and inference

- Types: `i8`–`i128`, `isize`, `u8`–`u128`, `usize` (`isize`/`usize` are 64-bit), `f32`,
  `f64`, `bool`, `char`, `()`, `str` (only behind a reference, E0208, also not as a tuple
  element), named structs, tuples `(A,)`, `(A, B)`, ..., `&T`, `&mut T`. A struct field
  may be a tuple, but not one holding a reference (references in fields wait for Phase 9,
  E0107). A struct may not contain itself by value, directly or through other
  structs (E0206). `!` is the type of `break`, `continue`, `return` and of a `loop`
  without `break`; it coerces to any type. The borrow of a diverging expression (`&loop {}`,
  `&mut return 1`) has type `&!` (`&mut !`), as in rustc: a borrow is not a coercion site,
  so `&!` does not become `&T` (`let q: &P = &loop {};` and `F(&return 1)` are E0300),
  while `*&return 9` is `!` again. `&!` values can be compared with each other and printed
  (rustc's `!: PartialEq + Display`); the code after one is unreachable.
- Inference runs per function body and per constant initialiser, by unification; nothing
  is inferred across item signatures. Expected types flow down from `let` annotations,
  parameter types, return types, struct field types and assignment targets; the types of
  subexpressions flow back up.
- A `char` is one UTF-16 code unit (decision 31), so a `char` literal outside the Basic
  Multilingual Plane (above U+FFFF, such as `'\u{1F600}'`) is an error (E0318), in
  constants too; a string literal holds any character. This is a deliberate divergence
  from Rust, where `char` is a Unicode scalar value and such literals are valid, so the
  differential corpus and the generator avoid them.
- An unsuffixed integer literal has type `{integer}`, unifies with any integer type and
  defaults to `i32`; a float literal is `{float}` and defaults to `f64`. A literal that
  does not fit the type it ends up with is an error (E0314), including after negation
  (`-129` as `i8`) and when the type comes from a cast (`300 as u8`).
- A `let` whose type nothing constrains is an error (E0313). Deferred initialisation
  (`let x; x = 1;`) is allowed; the type comes from the assignment.
- Tuples. `(a, b)` has type `(A, B)`. Against an expected tuple type of the same arity
  (an annotation, a parameter, a return type, an assignment target), each element is
  checked and coerced against its expected element type, so the elements are coercion
  sites (`let p: (&i32, u8) = (&mut x, 1);` coerces, and a `&mut` place element is
  reborrowed, not moved); without one an element keeps its own type, and an element of
  type `!` gets a fresh type falling back to `()`, as a diverging `let` initialiser does.
  An arity mismatch is E0300 with a note "expected a tuple with N elements, found one with
  M elements". `t.i` is element `i` of a tuple, auto-dereferencing through references;
  an index that is out of range, not plain decimal (`t.01`, `t.0x1`) or used on a
  non-tuple is E0301 ("no field `2` on type `(i32, bool)`"). Tuples are not printable,
  have no arithmetic and do not cast (E0300, E0306/E0307, E0308). Comparing tuples
  (`==`, `<`, ... also through references) is valid Rust but not yet supported (E0107):
  the comparison traits arrive in Phase 6, as for structs. `()` compares. Destructuring
  assignment (`(a, b) = (b, a)`) is E0107 too. A `const` cannot be a tuple yet (E0315).
- Patterns. A tuple pattern matches a tuple of its arity (E0300 otherwise: "expected
  `(i32, i32, i32)`, found a tuple with 2 elements"), `()` matches `()`, `_` matches
  anything and binds nothing, and an identifier binds the whole value. A tuple pattern
  matched against a reference to a tuple (`let (a, b) = &t;`) looks through the reference
  and binds references to the elements, `&` or `&mut` like the reference (a `&` anywhere
  outside makes every inner binding `&`): Rust 2021's default binding modes. A `mut x`
  binding there binds by value again, as in Rust 2021 (a copy, or a move, which E0407
  rejects out of a reference).
- Coercions: `!` to any type, and `&mut T` to `&T`. Nothing else: no `&T` to `&mut T`, no
  numeric widening, no autoref outside method receivers.
- Method calls `e.Name(args)` (#7, decision 53). The receiver is checked first. Only
  structs have methods; the method is the innermost type's (`e` dereferenced through every
  reference), and the receiver is adjusted as rustc's probe does: the receiver's type and
  then each type reached by one more dereference is matched against the method's receiver
  type (`P`, `&P` or `&mut P`) by value, then as `&step`, then as `&mut step`, the first
  match deciding. A reference matched by value is reborrowed (`r.Get()` for `r: &P` is
  `P::Get(&*r)`, `m.Inc()` for `m: &mut P` is `P::Inc(&mut *m)`), a place matched by an
  autoref is borrowed (`p.Inc()` is `P::Inc(&mut p)`, a temporary for any other
  expression), and a by-value `self` reached through a reference copies (or, not `Copy`,
  is E0407). The arguments are then checked against the parameters after the receiver
  ("this method takes N arguments", E0304). No method of that name, or a receiver that is
  not a struct, is E0319 (rustc's E0599), labelled "field, not a method" or "this is an
  associated function, not a method" when that is the case; `e.Name` naming a method is
  E0320 (rustc's E0615); a receiver of unknown type is E0313. Called as `P::Name(..)`, a
  method takes its receiver as an ordinary first argument: no autoref or autoderef
  (E0300 for `P::Get(p)` where `&P` is expected).
- Operators, by operand type:
  - `+ - * / %`: both operands the same integer or float type; result that type.
  - `& | ^`: both operands the same integer type, or both `bool`.
  - `<< >>`: left operand an integer type, right operand any integer type; result the
    left type.
  - `== != < <= > >=`: both operands the same primitive type (integers, floats, `bool`,
    `char`, `()`); result `bool`. References compare their referents, as Rust's
    `PartialEq`/`PartialOrd` impls for `&A` do: both operands must be references of the
    same depth to the same comparable primitive (`&i32 == i32` is E0306, like rustc's
    E0277). `==` and `!=` accept any mix of `&` and `&mut` at every level; `<`, `<=`, `>`
    and `>=` need the same mutability on both sides, except that a `&mut` right operand of
    a `&` left one is accepted (rustc coerces it to the `&B` of `impl PartialOrd<&B> for
    &A`). Structs and `&str` do not compare (operator traits arrive in Phase 6; `&str`
    comparisons come with `&str` support).
  - `&& ||`: `bool` operands, `bool` result.
  - Unary `-`: signed integer or float. Unary `!`: integer or `bool`.
  Violations are E0306 (binary) or E0307 (unary); an operand of the wrong type for an
  operator whose other operand fixed the type is E0300.
- Casts `e as T`, with Rust semantics (integer casts wrap, float to integer saturates):

  | from \ to | integer | float | `bool` | `char` |
  | --- | --- | --- | --- | --- |
  | integer | yes | yes | no | only from `u8` |
  | float | yes | yes | no | no |
  | `bool` | yes | no | identity | no |
  | `char` | yes | no | no | identity |

  Anything else, including references, structs, tuples and `()`, is E0308. An unsuffixed literal
  takes the target type when the families agree.
- `if c { a } else { b }`: `c` is `bool`; both branches have the same type, a diverging
  branch taking the other's. `if` without `else` is `()`, so its block must be `()`.
  `while` is `()`. `loop` has the type of its `break value`s, which must agree, and `!` if
  it never breaks. `break value` is only allowed directly inside `loop` (E0312);
  `break`/`continue` outside a loop is E0311.
- A block has the type of its tail expression, even when an earlier statement diverges
  (`{ return; 2u8 }` is `u8`, as in rustc); without a tail it is `!` when a statement in
  it diverges and `()` otherwise. A block-like expression used as a statement (`if`, `while`, `loop`,
  `{ .. }` without a trailing `;`) must be `()`.
- A function body is a block checked against the declared return type (`()` when
  omitted); `return;` has the value `()`.
- Field access `e.f` auto-dereferences through any number of references (E0301 for an
  unknown field or a non-struct, naming the type as written, references included:
  "no field `W` on type `&Point`"). A struct literal must name every field exactly once
  (E0301, E0302, E0303). A unit struct's name is a value; a named struct's is not (E0203).
  A function name is only callable, not a value (E0317); calls check arity (E0304) and
  argument types.
- Assignment `p = e` and compound assignment `p op= e` require `p` to be a place: a local
  (not a constant), a field of a place, or a dereference (E0310). Their value is `()`.
  Compound assignment follows the operator rules above.
- The `drop` of an `impl Drop` must be exactly `fn drop(&mut self)`: another receiver, a
  missing receiver or a return type is E0321 (rustc's E0053/E0186), another parameter
  count E0322 (E0050). Calling it, `d.drop()` or `D::drop(&mut d)`, is E0323 (E0040).
  `#[derive(Copy)]` on a struct with `impl Drop` is E0324 (E0184); on a struct with a
  field that has drop glue it is E0417, as for any non-`Copy` field.
- A `const` initialiser is evaluated at compile time. Allowed: literals, arithmetic,
  comparisons, logical operators, casts and references to other constants. Overflow,
  division or remainder by zero, over-wide shifts and cycles between constants are errors
  (E0316); blocks, calls, struct values, references and control flow are not constant
  expressions yet (E0315). Values follow Rust exactly: integer casts wrap, float to
  integer casts saturate (NaN gives 0), and an `f32` constant is rounded to `f32` after
  every operation, so `0.1f32 as f64` is `0.10000000149011612`.

### 11.3 Mutability

Checked on the MIR by `sabiko_borrowck` (E0400–E0405):

- A binding declared without `mut` is assigned at most once: its initialiser, or one
  deferred assignment on each path (E0400 otherwise). Reading a binding that may not have
  been assigned on every path is E0405.
- A pattern's bindings are separate locals: in `let (a, mut b) = t;` only `b` is
  mutable. Tuple elements are fields: everything below about `p.X` holds for `t.0`.
- Field mutability follows the binding: `p.X = 1` and `&mut p.X` require `p` to be
  declared `mut` (E0401, E0402). A `mut` binding allows assignment and `&mut` borrows of
  itself and of any field.
- Through `&T` nothing is mutable: assigning to `*r` or `r.X`, or taking `&mut` of them, is
  rejected whatever the binding's mutability (E0403, E0404).
- Through `&mut T` the target is mutable regardless of whether the reference binding
  itself is `mut`: `*r = 1` and `&mut r.X` are fine for `r: &mut T`.
- A method's receiver follows the same rules: `p.Inc()` with `Inc(&mut self)` borrows `p`
  mutably, so `p` must be `mut` (E0402), and through `r: &P` it is E0404. In a method,
  `self` is a parameter: a by-value `self` is immutable unless written `mut self`
  (`self.X = 1` is E0401, `self = ..` E0400); `&self` and `&mut self` make `self` a `&P`
  or `&mut P`.

### 11.4 Borrow checking (Phase 3 subset)

Checked on the MIR by `sabiko_borrowck` (E0406–E0418), after the mutability and
initialisation rules of §11.3. Every rule below is also a rule of Rust, and
`tests/differential/` holds a program per rule that `rustc` must judge the same way.

Moves (E0406, E0407):

- Primitives, `&T`, `()`, structs with `#[derive(Copy)]` and tuples whose elements are
  all `Copy` are `Copy`: using them copies.
  `#[derive(Copy)]` requires every field to be `Copy` (E0417); `Clone` may be named
  alongside it. Every other struct and `&mut T` is a move type: a by-value use (a `let`
  initialiser, assignment right-hand side, call argument, struct literal field, return
  value) moves the place, and any later read, borrow or move of it, or of a place that
  contains or is contained in it, is a use of a moved value (E0406). A move in one branch
  counts as a move after the `if`; a move inside a loop is a use-after-move on the next
  iteration. Moving a field (or tuple element) leaves the sibling fields usable but the
  parent not; assigning the moved field again makes the parent whole.
- A destructuring `let` whose initialiser is a place binds each name to its element of the
  place: only the bound elements are moved or copied, `_` touches nothing (`let (a, _) =
  t;` leaves `t.1` usable). Any other initialiser is evaluated into a temporary first.
  Temporary lifetime extension follows rustc through tuples: in `let p = (&f(), 1);` and
  `let (a, b) = &(x, y);` the borrowed temporary lives as long as the bindings.
- `let _ = place;` (a place initialiser: a local, a field of a place, a dereference) only
  mentions the place (#37, rustc's `PlaceMention`): nothing is read, moved, copied or
  required to be initialised, so `let _ = p; Take(p);` is fine for a non-`Copy` `p`, and so
  is `let _ = p;` after `p` was moved or before it is initialised. The mention is a use of
  the local for liveness only: a reference mentioned there keeps its loans live up to it
  (`let r = &x; x = 2; let _ = r;` and `let _ = *r;` there are E0410), but the mention
  itself conflicts with no loan. A value initialiser (`let _ = F();`) is evaluated into a
  temporary that dies at the end of the statement.
- Assigning a whole value to a moved-from `mut` binding re-initialises it.
- A move type cannot be moved out from behind a reference: `*r` for `r: &P`/`&mut P`, or
  a non-`Copy` field reached through one, can only be borrowed or copied (E0407).
- No non-`Copy` field may be moved out of a value whose type implements `Drop`, at any
  depth (`o.Inner`, `o.Inner.X`, also out of a by-value `self`): its `drop` will see the
  whole value (E0418, rustc's E0509). Moving the whole value, or a field of a struct
  without `Drop` that holds one, is fine.
- A `&mut T` is implicitly reborrowed (`&mut *r`) where the expected type is `&mut`:
  call arguments, struct literal fields, assignment, annotated `let`, `return`. Where the
  expected type is `&T` the reborrow is shared (`&*r`), so `F(&*r, r)` with two `&T`
  parameters is accepted. A plain `let s = r;` moves it (as in Rust).
- A method call evaluates its receiver before its arguments: a by-value receiver is moved
  (or copied) first, so `p.Merge(p.X)` for a non-`Copy` `p` is E0406 at `p.X`, while
  `P::Merge(p, x)` after `let x = p.X` is fine; a receiver reached by a `&mut` (`m.Inc()`)
  is reborrowed, not moved.

Loans (E0408–E0413):

- `&p` creates a shared loan, `&mut p` a mutable loan of the place `p`. A loan is live
  from its creation to the last use of any reference that may hold it (NLL liveness:
  through assignments, calls and returns, not to the end of the lexical scope), and dies
  when the reference holding it is reassigned (a kill) or goes out of scope.
- While a shared loan is live, the place and every place that overlaps it (its fields,
  any struct containing it, the referent of a reference to it) may be read and shared
  again, but not written (E0410), mutably borrowed (E0408) or moved (E0412).
- While a mutable loan is live, the loaned place and everything overlapping it may not be
  read, written, moved or borrowed at all, other than through that reference (E0409 for a
  second `&mut`, E0411 for a use, E0410 for a write, E0412 for a move). Reborrows through
  the reference (`&mut *r`, `&*r`, `&mut r.X`) are allowed and suspend the original until
  the reborrow's last use.
- A tuple holding references holds their loans (`let p = (&a, &mut b);` borrows both
  until `p`'s last use); its elements' origins are tracked one by one.
- Loans of disjoint fields (`&mut p.X` and `&mut p.Y`, `&mut t.0` and `&mut t.1`) do not
  conflict; two loans of the
  same field do.
- A referent must outlive every reference to it: a reference to an inner-block local used
  after the block ends is E0413. A value with drop glue is dropped at the end of its scope
  (§7), which ends its loans the same way; the drop is not a use of the references the
  value holds (drop-liveness). A temporary receiver dies with its statement:
  `let r = Make().XRef();` followed by a use of `r` is E0413 (rustc's E0716).

Two-phase borrows (#15, decision 54). Two borrows a call makes implicitly are two-phase:
an autoref'd `&mut self` receiver (`v.Add(..)`, also `m.Add(..)` for `m: &mut P`,
`a.B.Add(..)` and `self.Add(..)`) and the reborrow of a `&mut` argument passed where a
`&mut` is expected (`Use(m, ..)`, `P::Add(m, ..)`). Such a borrow is *reserved* where it is
made and *activated* by the call:
- while it is reserved, the later arguments may read its place, directly, through fields
  or through `&self` methods (`v.Add(v.Get())`, `Use(m, m.X)`), but not write it (E0410),
  move out of it (E0412) or borrow it mutably, two-phase or not (E0409);
- at the activation no other borrow of the place may be live: a shared borrow passed to the
  call conflicts (`v.AddFrom(&v)`, E0408), and so does one taken before the reservation
  and used after the call (`let s = &v; v.Add(s.Get()); s.X` is E0408; without the last
  use it is accepted);
- explicit borrows are never two-phase: `P::Add(&mut v, v.Get())` and
  `(&mut v).Add(v.Get())` are E0408, `Use(&mut *m, m.X)` is E0411, and so is
  `a.BMut().Add(a.N)`, whose receiver is borrowed by the call `a.BMut()`. A `&self`
  receiver is a plain shared borrow held through the call (`p.Plus({ p.X = 5; 2 })` is
  E0410).

Signatures and lifetimes (E0414–E0416):

- Lifetimes are elided only (a reference inside a tuple counts like any other, in the
  parameters and in the return type): a function returning a reference must have exactly one
  reference parameter (nested references count once per parameter), and the returned
  reference is tied to it (E0416 otherwise, until Phase 9 adds `'a`; reported from the
  signature alone, so also when the body has other errors). A method taking `&self` or
  `&mut self` returns references tied to `self` whatever its other parameters, so the
  result borrows the whole receiver (`let x = p.XMut(); p.Y = 5; *x += 1;` is E0410) and
  returning a reference into another parameter is E0415; a by-value `self` takes no part
  (`fn Larger(self, n: &i32) -> &i32` borrows `n`). A reference
  returned may therefore only point into that parameter's referent; returning a reference
  to a local or to a by-value parameter is E0414.
- Inside a body, references with different elided lifetimes are unrelated: storing `y` in
  `*x` for `x: &mut &i32, y: &i32` is E0415 ("lifetime may not live long enough" in Rust).
  References in struct fields stay rejected until Phase 9.

The two error families map onto rustc's as follows: E0406/E0407 ↔ E0382/E0507; E0408–E0412
↔ E0502/E0499/E0506/E0503/E0505; E0413 ↔ E0597 (E0716 for a temporary); E0414 ↔ E0515;
E0415 ↔ the uncoded "lifetime may not live long enough"; E0416 ↔ E0106; E0417 ↔ E0204;
E0418 ↔ E0509.
The differential harness compares verdicts (accept vs borrow error), not codes. The
mutability family maps as E0400 ↔ E0384, E0401/E0403 ↔ E0594, E0402/E0404 ↔ E0596.
