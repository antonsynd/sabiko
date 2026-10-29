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
- Statements and expressions
- Nesting is limited to 256 levels (`sabiko_parser::MAX_NESTING`, clang's default bracket
  depth); deeper input is a fatal E0109, where rustc has no fixed limit. A level is a
  nested expression (parentheses, a block's statement, a call argument, an `else if`), a
  prefix operator, each binary operator or cast of a chain, each postfix call, index or
  field access, or a `&` in a type; `&&` counts twice
- Patterns
- Types: primitives, references, tuples, arrays, slices, named types, generics

## 3. Names and modules (Phase 2, 9)

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

## 7. Drop and panics (Phase 4)

Panics (Phase 4): a panic is a `Sabiko.Runtime.PanicException`. `Panic(msg)` throws one;
integer overflow in a debug build, division or remainder by zero, `MIN / -1`, `MIN % -1`,
and a shift by an amount outside `0..bits` (negative amounts included) panic too, with
Rust's messages ("attempt to divide by zero", "attempt to calculate the remainder with
overflow", "attempt to shift left with overflow", ...). Overflow of `+`, `-` and `*` is
the CLR's own `OverflowException`, which the runtime reports as "attempt to perform
arithmetic with overflow" (Rust names the operator).
An executable's entry point runs `Main` inside a `try`: an exception that escapes is
reported on standard error as `thread 'main' panicked:` followed by the message, and the
process exits with code 101 (Rust's). Standard output written before the panic is kept.
Release builds (`--release`) wrap integer arithmetic instead of panicking; division, shift
and explicit panics behave the same in both.

Drop: user `Drop` implementations need `impl` blocks and arrive with drop elaboration in
Phase 5. The lowering is fixed (decision 12): a scope owning droppable values becomes a
`try`/`finally` whose `finally` drops in reverse declaration order, so drops run during
unwinding for free.

## 8. Memory model (Phase 7)
- `Box<T>`, `Gc<T>`, `Cell<T>`, `RefCell<T>`, `Arena`
- Strings and collections

## 9. .NET interop (Phase 8)
- Import mapping (`ref`/`in`/`out`, classes, exceptions, nullability)
- Export shape (attributes, `IDisposable`, naming)

## 10. Lowering to CIL (Phase 4 onward)

Implemented by `sabiko_codegen` (textual IL assembled by `ilasm`; decision 3).

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
usize` are `uint8 uint16 uint32 uint64 native uint`; `f32`/`f64` are `float32`/`float64`;
`bool` is `bool`; `char` is `char` (a UTF-16 code unit, decision 31); a struct is
`valuetype Name`. `&T` and `&mut T` are managed pointers `T&`; in signatures `&T` carries
`modreq(System.Runtime.InteropServices.InAttribute)` and the parameter (or the return
value) the `IsReadOnlyAttribute` custom attribute, so C# sees `in T` and `ref readonly T`;
`&mut T` is a plain `ref T`. `&str` is `System.ReadOnlySpan<char>` by value. `()` is
`void` as a return type; locals and temporaries of type `()` or `!` have no storage, and
assignments to them are dropped. Not lowered yet (E0500): `i128`/`u128`, `&&T`, `()`-typed
parameters and fields.

Statements and expressions. The backend walks the MIR (§11); every statement and
terminator becomes an instruction sequence that leaves the evaluation stack empty, so basic
blocks are plain labels. MIR locals become IL locals (`.locals init`) and MIR arguments IL
arguments. A place is accessed through its address: `ldloca`/`ldarga` for a local or
argument, `ldflda` for a field, the pointer value itself for `*r`; the final load or store
is `ldloc`/`stloc`, `ldfld`/`stfld` or `ldind.*`/`stind.*` (`ldobj`/`stobj` for structs).
`&place` is that address; `&mut *r` is `r` itself. A struct literal is built in its
destination local with `initobj` and one `stfld` per field, or staged through a temporary
when the destination is a projection or is read by one of the fields. Calls are `call`
with the callee's full signature; `Print`/`PrintLine` select the runtime overload by the
argument's type and `Panic` calls `Sabiko.Runtime.Panic::Fail`; a call that cannot return,
and `unreachable`, are followed by a throw. `if` is `brfalse`, other `switchInt`s a `beq`
chain; `return` loads the return local and `ret`.

Arithmetic follows Rust. Values narrower than `int32` (`i8 i16 u8 u16`) compute at `int32`
width and are re-narrowed after every arithmetic operation: `conv.ovf.*` in debug builds
(trapping on overflow), `conv.*` in release builds (wrapping). `i32`/`i64`/`isize` and the
unsigned widths use `add.ovf`/`sub.ovf`/`mul.ovf` (`.un` for unsigned) in debug builds and
the plain opcodes in release builds. Negation is `0 - x` in debug builds so `-MIN` traps,
`neg` otherwise. Division and remainder are `div`/`rem` (`.un` for unsigned) preceded by
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
unordered forms on floats so that any comparison with NaN but `!=` is false. Casts: integer
to integer `conv.*` chosen by the target, with sign extension decided by the source
(`conv.i8` from signed, `conv.u8` from unsigned sources); integer to float `conv.r4`/`conv.r8`
preceded by `conv.r.un` for unsigned sources; float to `i32`/`i64`/`isize` and their
unsigned counterparts rely on the CLR's saturating conversions (NaN gives 0); float to a
narrow type clamps with `System.Math.Clamp` first, because the CLR only saturates to 32
bits. Constants are `ldc.i4`/`ldc.i8` (with `conv.i`/`conv.u` for the native widths),
`ldc.r4`/`ldc.r8` (non-finite values by bit pattern), and `ldstr` converted with
`String::op_Implicit` for `&str`. `.maxstack` is computed exactly.

Verifiability. Every emitted assembly passes ILVerify with `ReturnPtrToStack` ignored
(decision 30): ECMA-335 has no rule that lets a function return a managed pointer it
received, so that error fires for every `fn ... -> &T`; the borrow checker (§11.4) ensures
such a function only returns a pointer into its parameter's referent. The run-pass tests
verify every assembly they run; a verification error is a compiler bug.

## 11. Static semantics (Phase 2 subset)

What the compiler enforces today for the Phase 1 language subset (`fn`, `struct`, `const`,
no methods, generics, enums, tuples or arrays). This section is folded into §3, §4 and §6
as they fill in. Error codes are in `crates/sabiko_diagnostics`; each rule has a UI test
under `tests/ui/resolve` or `tests/ui/typeck`.

### 11.1 Name resolution

- Items (`fn`, `struct`, `const`) are visible everywhere in the file, including before
  their definition.
- Items share one namespace: a struct, function and constant may not have the same name
  (E0200). Field names must be unique within a struct (E0204) and parameter names within
  a parameter list (E0205).
- A `let` binding is visible from the statement after it to the end of its block and
  shadows earlier bindings and items of the same name. Its initialiser is resolved before
  the binding is introduced, so `let x = x + 1;` reads the outer `x`. A local is not
  visible before its `let` or outside its block (E0202).
- Parameters live in the function's outermost scope, which the body block shares.
- Primitive type names (`i32`, `bool`, ...) resolve last, so `struct i32;` shadows the
  primitive within the file. A primitive name is never a value (E0202).
- A name used in the wrong position (a function as a type, a constant in a struct literal,
  a non-unit struct as a value) is E0203. `_` as a type is allowed only inside bodies
  (E0207). Paths with `::` are rejected until modules arrive (E0107).

### 11.2 Types and inference

- Types: `i8`–`i128`, `isize`, `u8`–`u128`, `usize` (`isize`/`usize` are 64-bit), `f32`,
  `f64`, `bool`, `char`, `()`, `str` (only behind a reference, E0208), named structs,
  `&T`, `&mut T`. A struct may not contain itself by value, directly or through other
  structs (E0206). `!` is the type of `break`, `continue`, `return` and of a `loop`
  without `break`; it coerces to any type.
- Inference runs per function body and per constant initialiser, by unification; nothing
  is inferred across item signatures. Expected types flow down from `let` annotations,
  parameter types, return types, struct field types and assignment targets; the types of
  subexpressions flow back up.
- An unsuffixed integer literal has type `{integer}`, unifies with any integer type and
  defaults to `i32`; a float literal is `{float}` and defaults to `f64`. A literal that
  does not fit the type it ends up with is an error (E0314), including after negation
  (`-129` as `i8`) and when the type comes from a cast (`300 as u8`).
- A `let` whose type nothing constrains is an error (E0313). Deferred initialisation
  (`let x; x = 1;`) is allowed; the type comes from the assignment.
- Coercions: `!` to any type, and `&mut T` to `&T`. Nothing else: no `&T` to `&mut T`, no
  numeric widening, no autoref.
- Operators, by operand type:
  - `+ - * / %`: both operands the same integer or float type; result that type.
  - `& | ^`: both operands the same integer type, or both `bool`.
  - `<< >>`: left operand an integer type, right operand any integer type; result the
    left type.
  - `== != < <= > >=`: both operands the same primitive type (integers, floats, `bool`,
    `char`, `()`); result `bool`. Structs and references do not compare (operator traits
    arrive in Phase 6).
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

  Anything else, including references, structs and `()`, is E0308. An unsuffixed literal
  takes the target type when the families agree.
- `if c { a } else { b }`: `c` is `bool`; both branches have the same type, a diverging
  branch taking the other's. `if` without `else` is `()`, so its block must be `()`.
  `while` is `()`. `loop` has the type of its `break value`s, which must agree, and `!` if
  it never breaks. `break value` is only allowed directly inside `loop` (E0312);
  `break`/`continue` outside a loop is E0311.
- A block has the type of its tail expression, `()` without one, and `!` when a statement
  in it diverges. A block-like expression used as a statement (`if`, `while`, `loop`,
  `{ .. }` without a trailing `;`) must be `()`.
- A function body is a block checked against the declared return type (`()` when
  omitted); `return;` has the value `()`.
- Field access `e.f` auto-dereferences through any number of references (E0301 for an
  unknown field or a non-struct). A struct literal must name every field exactly once
  (E0301, E0302, E0303). A unit struct's name is a value; a named struct's is not (E0203).
  A function name is only callable, not a value (E0317); calls check arity (E0304) and
  argument types.
- Assignment `p = e` and compound assignment `p op= e` require `p` to be a place: a local
  (not a constant), a field of a place, or a dereference (E0310). Their value is `()`.
  Compound assignment follows the operator rules above.
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
- Field mutability follows the binding: `p.X = 1` and `&mut p.X` require `p` to be
  declared `mut` (E0401, E0402). A `mut` binding allows assignment and `&mut` borrows of
  itself and of any field.
- Through `&T` nothing is mutable: assigning to `*r` or `r.X`, or taking `&mut` of them, is
  rejected whatever the binding's mutability (E0403, E0404).
- Through `&mut T` the target is mutable regardless of whether the reference binding
  itself is `mut`: `*r = 1` and `&mut r.X` are fine for `r: &mut T`.

### 11.4 Borrow checking (Phase 3 subset)

Checked on the MIR by `sabiko_borrowck` (E0406–E0417), after the mutability and
initialisation rules of §11.3. Every rule below is also a rule of Rust, and
`tests/differential/` holds a program per rule that `rustc` must judge the same way.

Moves (E0406, E0407):

- Primitives, `&T` and structs with `#[derive(Copy)]` are `Copy`: using them copies.
  `#[derive(Copy)]` requires every field to be `Copy` (E0417); `Clone` may be named
  alongside it. Every other struct and `&mut T` is a move type: a by-value use (a `let`
  initialiser, assignment right-hand side, call argument, struct literal field, return
  value) moves the place, and any later read, borrow or move of it, or of a place that
  contains or is contained in it, is a use of a moved value (E0406). A move in one branch
  counts as a move after the `if`; a move inside a loop is a use-after-move on the next
  iteration. Moving a field leaves the sibling fields usable but the parent struct not.
- Assigning a whole value to a moved-from `mut` binding re-initialises it.
- A move type cannot be moved out from behind a reference: `*r` for `r: &P`/`&mut P`, or
  a non-`Copy` field reached through one, can only be borrowed or copied (E0407).
- A `&mut T` is implicitly reborrowed (`&mut *r`) where the expected type is `&mut`:
  call arguments, struct literal fields, assignment, annotated `let`, `return`. Where the
  expected type is `&T` the reborrow is shared (`&*r`), so `F(&*r, r)` with two `&T`
  parameters is accepted. A plain `let s = r;` moves it (as in Rust).

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
- Loans of disjoint fields (`&mut p.X` and `&mut p.Y`) do not conflict; two loans of the
  same field do.
- A referent must outlive every reference to it: a reference to an inner-block local used
  after the block ends is E0413.

Signatures and lifetimes (E0414–E0416):

- Lifetimes are elided only: a function returning a reference must have exactly one
  reference parameter (nested references count once per parameter), and the returned
  reference is tied to it (E0416 otherwise, until Phase 9 adds `'a`; reported from the
  signature alone, so also when the body has other errors). A reference
  returned may therefore only point into that parameter's referent; returning a reference
  to a local or to a by-value parameter is E0414.
- Inside a body, references with different elided lifetimes are unrelated: storing `y` in
  `*x` for `x: &mut &i32, y: &i32` is E0415 ("lifetime may not live long enough" in Rust).
  References in struct fields stay rejected until Phase 9.

The two error families map onto rustc's as follows: E0406/E0407 ↔ E0382/E0507; E0408–E0412
↔ E0502/E0499/E0506/E0503/E0505; E0413 ↔ E0597; E0414 ↔ E0515; E0415 ↔ the uncoded
"lifetime may not live long enough"; E0416 ↔ E0106; E0417 ↔ E0204. The differential harness
compares verdicts (accept vs borrow error), not codes.
