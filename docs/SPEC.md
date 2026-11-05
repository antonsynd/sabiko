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
- Enums (Phase 5, #8): `enum E { A, B(T, U), C { F: T }, D = 3 }`, non-generic (generic
  parameters are E0107 until Phase 6), with `#[derive(Copy, Clone)]` and `impl` blocks
  (also `impl Drop for E`) as for structs. Variants are named `E::V` or `Self::V`: a unit
  variant is a value, a tuple variant a constructor `E::V(a, b)`, a struct variant a
  literal `E::V { F: a }`. A struct-like variant's fields take doc comments but not `pub`
  (E0100, rustc's E0449)
- Patterns (#6, #8): `_`; bindings `x`, `mut x`, `ref x`, `ref mut x`, each optionally
  `@ subpattern`; literals (integer, possibly negated, float, `bool`, `char`) and ranges
  `a..=b`, `a..b` whose bounds are literals or constants (a local is E0315, rustc's
  E0080); paths `E::V`; tuple patterns
  `(p, ..)`, tuple-variant patterns `E::V(p, ..)` and struct patterns `S { F: p, G, ref H,
  .. }` (struct or struct variant), with at most one `..` among the elements; or-patterns
  `p | q` (a leading `|` is allowed in a `match` arm and an `if let`); `(p)` is `p`. A lone
  identifier that names a constant or a unit struct compares instead of binding, as in
  rustc. A string literal pattern matches a `&str` (#10). A top-level or-pattern in a `let`
  or a parameter needs parentheses (E0100, as in rustc). Not yet supported (E0107):
  reference patterns `&p`, half-open ranges `a..` and `..=b`, slice patterns
- `match e { p if guard => body, .. }`; the scrutinee may not be a bare struct literal
  (E0108). A block-like arm body needs no comma. `if let p = e { .. } else { .. }`, `while
  let p = e { .. }` and `let p = e else { .. };`; let chains (`if let p = e && c`) are
  edition 2024 and rejected (E0100), and the initialiser of a `let`-`else` may not end with
  `}` or be a lazy boolean (E0100)
- Types: primitives, references, tuples (`(A,)`, `(A, B)`, ...; `(A)` is `A`), arrays
  `[T; N]`, slices `[T]`, named types, generics
- Arrays and slices (Phase 5, #9): array literals `[a, b, c]` and `[]` (a trailing comma
  is allowed), repeat expressions `[x; N]`, indexing `e[i]` and `e.len()`. The length `N`
  of `[T; N]` and `[x; N]` is a constant expression of its own (rustc's anonymous
  constant). Slice patterns (`[a, b]`, `[first, .., last]`) are E0107
- Ranges (#10): `a..b`, `a..`, `..b`, `..`, `a..=b` and `..=b`, only as the whole index of
  `e[...]` (`&s[1..3]`, `&mut a[i..]`); the bounds are parsed at the precedence of `||`,
  as in rustc, and `..=` needs an end (E0102). A range anywhere else, a value of type
  `Range<T>` in Rust, is E0107 until generic types (Phase 6)
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
- Enums and layout: see §10 (Enums) and §11.2
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
  struct literal), recursively. A tuple drops its elements first to last, and so does an
  array (#9): when one element's `drop` panics, the elements after it are still dropped
  while unwinding. An array is moved and dropped whole (nothing moves out of an element,
  E0420); assigning an element drops the old element after the store, like any
  assignment; `[x; 0]` evaluates `x` and drops it at the end of the statement, `[x; 1]`
  moves it in. An array of length 0 has no drop glue.
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
  E0716), and so does a `let` (or `let`-`else`) whose pattern has a `ref`/`ref mut`
  binding, for its initialiser's value (`let S { ref mut a, .. } = Make();`, rustc's
  extending patterns); a failing `let`-`else` drops its initialiser's temporaries before
  the `else` block runs; a block's tail expression's temporaries are dropped after the block's locals
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

Enums (decision 57). An enum is a top-level `public sequential ansi sealed` value type like
a struct: a field `Tag` (`Tag_` if a variant is called `Tag`) holding the variant's
discriminant, `int32` unless a discriminant needs `int64`, then one public field per
variant that has fields, named after the variant, of a nested `sequential` value type
`<Variant>Payload` (decision 59) holding the variant's fields (a struct variant's by name, a tuple variant's
as `Item1`, `Item2`, ...). Payloads do not overlap yet (decision 15). An enum without
variants still has its `Tag`. C# reads `Shape::Rect { W, H }` as `s.Tag == 2` and
`s.Rect.W`. A variant value is built in place like a struct literal: `initobj`, the tag,
then each payload field through `ldflda` of the payload; a downcast place `(e as V).F` is
`ldflda E::V` then the field; `discriminant(e)` loads the tag and widens it to `native
int`; a fieldless enum cast with `as` loads the tag of the value and converts it like an
integer of the tag's type. A `switchInt` on a discriminant compares with each target in
turn. Drop glue of an enum with drop glue calls its own `drop` (if any), then one helper
per variant with droppable fields, which returns at once unless the enum holds its
variant and otherwise drops the variant's fields in order, each step protected like a
struct's. Fake borrows (match guards) emit nothing and their temporaries have no local.

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
`valuetype Name`. `&T` and `&mut T` are managed pointers `T&`; in signatures an `&T`
parameter is `[in]` and an `&T` return carries
`modreq(System.Runtime.InteropServices.InAttribute)`, and the parameter (or the return
value) has the `IsReadOnlyAttribute` custom attribute, so C# sees `in T` and `ref readonly
T`, as C# itself emits them for a non-virtual method (decision 59);
`&mut T` is a plain `ref T`. An enum (decisions 15 and 57, names by decision 59) is a
`sequential` value type with a `Tag` field (`int32`, or `int64` when a discriminant needs
it; `Tag_` when a variant is called `Tag`) holding the discriminant, then one field per
variant with fields, named after the variant, of a nested value type `<Variant>Payload`
(`_` appended while that names another member) holding a struct variant's fields by name
and a tuple variant's as `Item1`, `Item2`, ...; C# reads `Shape::Rect { W, H }` as
`s.Tag` and `s.Rect.W` (`tests/interop/enum_layout`). `&str` is `System.ReadOnlySpan<char>` by value. A tuple
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
`Unit` local instead. A reference to `()` (`&()`, `&mut ()`, `&&()`, ...) is lowered
exactly as `()` is (decision 64): it has one observable value, so it has no storage in a
local, is a `Unit` as a parameter or tuple element and `void` as a return type, and a read
through it reads nothing.

References to references (#11, decision 64). The CLR has no pointer to a managed pointer,
and a managed pointer can be neither a generic argument nor a field of anything but a
`ref struct`. A reference that something else points at, or that a tuple holds, is
therefore kept in a wrapper generated into the module: `Ref<T>` (`'Ref`1'`) for a shared
reference and `RefMut<T>` (`'RefMut`1'`) for a mutable one, each a `readonly ref struct`
with one `initonly` field `Value`, a managed pointer to `T` (`ref readonly T` and `ref T`
to C#, the former marked with `IsReadOnlyAttribute`), and a constructor taking that
pointer. The type parameter is `byreflike` (C#'s `allows ref struct`), so wrappers nest:
`T` is the referent's own storage type, itself a wrapper for `&&&i32`
(`Ref<Ref<int32>>`). The wrapper is the storage of a reference place in three cases: a
local holding a reference that some borrow (not a guard's fake borrow) takes the address
of (`let rr = &r;`, `&mut r`, also a two-phase borrow), which is declared as the wrapper;
a parameter likewise borrowed, which keeps its `T&` (`[in] T&`) type in the signature and
is copied into a wrapper local on entry (`ldarg; newobj; stloc`); and every tuple element
of reference type, so a tuple with references (decision 52) has `Ref<T>`/`RefMut<T>`
fields while its constructor still takes the managed pointers and wraps them. A reference
to a reference is then a managed pointer to a wrapper (`&&i32` is `Ref<int32>&`, `[in]` as
a parameter like any `&T`), and the place it points to is a wrapper. Reading a reference
from a wrapper place loads the wrapper's address (`ldloca`, `ldflda`, or the outer
pointer) and its `Value` (`ldfld !0& valuetype 'Ref`1'<int32>::'Value'`); writing one
builds a new wrapper (`newobj`) and stores it whole over the old one (`stloc`, `stfld`,
`stobj`), so nothing writes `Value` after construction. Other reference locals and
parameters stay plain managed pointers. `&str` and `&[T]` are span values, not pointers, so
`&&str` is a managed pointer to a `ReadOnlySpan<char>` and needs no wrapper (a wrapper
appears at `&&&str`). Not lowered yet (E0500): arrays and slices of references (`[&T; N]`,
`&[&T]`, `&[&str]`, #43), since an inline array can hold neither a managed pointer nor a
`ref struct`.

Arrays and slices (#9, decision 61). An array type `[T; N]` is a value type generated into
the module and named after the Sabiko type (`'[i32; 3]'`): for `N > 0` a `sequential`
struct with one field `Element` of type `T` and `[InlineArray(N)]`, which makes the
runtime lay out `N` elements (C# 12's inline arrays; the same layout as C#'s fixed-size
buffers); for `N = 0` an empty struct (the runtime rejects an inline array of length 0).
A reference to an array is a managed pointer to that struct (`'[i32; 3]'&`, `[in]` when
shared, as for any `&T`). `&[T]` is `System.ReadOnlySpan<T>` and `&mut [T]` is
`System.Span<T>`, by value, as for `&str`. An element is reached through a span over the
array's elements, `MemoryMarshal.CreateSpan(ref a.Element, N)`, or over the slice itself,
and the span's `get_Item`; before that, the index (a `native uint`) is compared, unsigned,
with the length widened by `conv.i` (the shape after which RyuJIT drops the span's own
check in a `while i < s.len()` loop), and a failure calls
`Sabiko.Runtime.Checks::IndexOutOfBounds(native uint len, native uint index, string
location)`, which panics with rustc's `index out of bounds: the len is {len} but the index
is {index}` at the indexing expression's location, in release builds too. Every use of an
element place checks its indices: reads, writes, borrows and `let _ = a[i];`. An array
literal is built in a scratch local through a span; `[x; N]` reads `x` once and fills the
elements with `Span<T>.Fill`, or is `initobj` for a zero constant. Unsizing (`&[T; N]` to
`&[T]`) is `CreateReadOnlySpan`/`CreateSpan` over the array's elements; `&mut [T]` to
`&[T]` is `Span<T>`'s implicit conversion. `.len()` of an array is the constant `N`, of a
slice the span's `Length`. An array with drop glue gets glue that drops its elements first
to last, each in a protected region whose `fault` handler drops the remaining ones. Not
lowered yet (E0500): arrays and slices holding references (`[&T; N]`, `&[&str]`): an
inline array cannot hold a managed pointer or a `ref struct`; arrays longer than
`int32::MAX`.

Strings and range slicing (#10, decision 62). A `str` place is reached like a `[char]`
one, through its `ReadOnlySpan<char>`; `.len()` is the span's `Length`. MIR lowers `e[a..b]`
as rustc lowers its `Index::index` (`IndexMut::index_mut` where the place is written,
borrowed `&mut` or bound `ref mut`) call: the base is borrowed first (an array's borrow
unsized to a span), then the bounds are evaluated, then `Subslice(base, a..b)` makes the
sub-slice, a reference of the base's type that borrows what the base does; the place is
the sub-slice behind it. The backend loads the span, compares the bounds unsigned with its
length and each other in rustc's order (`b <= len`, `b < len` for `..=b`, then `a <= b`,
`a <= b + 1` for `..=b`), and calls the span's `Slice(start, length)`; a failure calls
`Sabiko.Runtime.Checks::StrSliceFail` or `SliceFail(native uint start, native uint end,
bool inclusive, native uint len, string location)`, which pick rustc's message (`start
byte index 7 is out of bounds for string of length 5`, `end byte index ...`, `byte range
starts at 3 but ends at 1`; for slices `range start index 7 out of range for slice of
length 5`, `range end index ...`, `slice index starts at 3 but ends at 1`), located at the
`[`, where rustc locates a panic of the `Index` call, in release builds too. `&s[..]` is
the span itself. A comparison of two `&str` calls `Sabiko.Runtime.Str::Equal` (`==`,
`!=`) or `Str::Compare` (the orderings), `MemoryExtensions.SequenceEqual` and
`SequenceCompareTo` underneath; a string literal pattern is such a comparison. A string
literal holding a NUL is a `ldstr bytearray (...)`, since `ilasm` ends a quoted string at
an escaped NUL.

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
operand runs (`r == { *r = 3; s }` is E0410, rustc E0506). rustc also borrows the left
operand's reference *itself* (`&r`), so a left place operand gets a shallow fake borrow
(`&fake shallow r`, decision 63) kept live until the comparison: assigning, moving or
mutably borrowing `r` (or a prefix of it: `t = ..` for `t.0`, `a[j] = ..` for `a[i]`) in
the right operand is a conflict, as in rustc (`r == { r = &y; r }` is E0410, rustc E0506),
and so is reassigning a compared `&str` place (`s == { s = "b"; s }`), which is copied
rather than reborrowed. A conflict both borrows see is reported once, as the reborrow's.
The fake borrow emits no code and does not count as borrowing `r` for copy forwarding, so a
comparison compiles to the same IL as without it. Casts: integer
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
no generics or arrays; tuples since #6, inherent `impl` blocks and methods since #7,
enums, `match` and patterns since #8). This section is folded into §3, §4 and §6
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
- Enums (#8) are items like structs (`impl` blocks, `impl Drop`). Their variants are not in
  the item namespace: `E::V` and `Self::V` name them, before an associated function of the
  same name; a variant name is unique within its enum (E0200, rustc's E0428), and so is a
  struct variant's field (E0204, E0124). An enum or a variant used as a value without a
  path to a variant is E0203; `V(..)` with a lone name is E0202 with a note naming `E::V`;
  an unknown `E::X` is E0212 (rustc's E0599).
- Patterns bind per pattern: `match` arms, `if let` and `while let` bind for their arm,
  `then` block or body; `let`-`else` binds in the enclosing block after the statement (the
  `else` block does not see the bindings). Every alternative of an or-pattern binds the
  same names to the same locals (E0218, rustc's E0408) with the same binding mode (`ref`,
  `mut`: E0219, rustc's E0409).
- An array length or repeat count (#9) is resolved as a constant of its own: `const` items
  are in scope, the locals and parameters of the enclosing function are not (E0220,
  "attempt to use a non-constant value in a constant", rustc's E0435).

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
  (`-129` as `i8`) and when the type comes from a cast (`300 as u8`). Of two nested
  negations only the outer counts, as in rustc: the literal of `-(-128i8)` is `128i8`.
- A `let` whose type nothing constrains is an error (E0313). Deferred initialisation
  (`let x; x = 1;`) is allowed; the type comes from the assignment.
- Arrays and slices (#9). `[T; N]` has a length `N` that is a `usize` constant
  expression: literals, `const` items and constant arithmetic (another type is E0300,
  `-1` E0307, a local E0220); the constants are evaluated first, so a signature may use
  them (`fn F(a: [i32; N])`). A constant of array type, and an array expression inside a
  constant, are not supported yet (E0315). `[T]` is unsized: only `&[T]` and `&mut [T]`
  exist, a `[T]` parameter, `let` or `*s` is E0208, and its element type must be sized.
  `[a, b, c]` has type `[T; 3]`, every element checked against the element type (the
  expected one, else the first element's), so `[1, true]` is E0300 at `true`; `[]` takes
  its element type from the context. `[x; N]` checks `x` against the element type and
  `N` as above; for `N` other than 0 or 1, `x`'s type must be `Copy` unless `x` names a
  `const` item (E0334, rustc's E0277). The length is part of the type: arrays of two
  lengths are E0300 with the note "expected an array with a size of 3, found one with a
  size of 2", and nothing converts between lengths. `e[i]` looks through any number of
  references to an array or slice (E0336 for anything else, rustc's E0608) and needs a
  `usize` index (E0335 "the type `[i32]` cannot be indexed by `i32`", rustc's E0277); an
  unsuffixed literal or an integer variable becomes `usize`. It is a place of the element
  type. `e.len()` on an array or slice (through references) is a `usize`; any other
  method of one is E0319, `e.len` E0320, an argument E0304. `&[T; N]` coerces to `&[T]`
  and `&mut [T; N]` to `&mut [T]` or `&[T]` (unsizing) at every coercion site, and
  `&mut [T]` to `&[T]`; `&[T]` never converts back to an array (E0300). Arrays of `Copy`
  elements are `Copy`. Arrays are not printable, have no arithmetic and do not cast;
  comparing arrays or slices (`==`, `<`, ...) is valid Rust but E0107 until the comparison
  traits (Phase 6). A struct containing itself through an array is E0206.
- Strings and range slicing (#10, decision 62). `str` is a sequence of UTF-16 code units
  (decision 7): `s.len()` on a `&str` (through references) is a `usize` counting code
  units, and range bounds are code-unit indices, where Rust counts UTF-8 bytes; the two
  agree on ASCII text, so the differential corpus and the generator keep their strings
  ASCII. A string is indexed by a range only (an integer index is E0336, rustc's E0277).
  `e[r]` for a range `r` looks through references to a `str`, an array or a slice and is
  a place of type `str` or `[T]`; each bound is checked against `usize` (E0300). The
  place is unsized: it may be borrowed (`&s[1..]`, `&mut a[..2]`), indexed
  (`a[1..][0] = 5`) or measured (`s[2..].len()`), and is E0208 anywhere else (a `let`, an
  argument, an operand). `== != < <= > >=` compare two `&str` (references of the same depth
  otherwise compare their referents, so `&&str` operands compare the strings, decision
  63); comparing `str` places (`*s == *t`) is
  E0208. Equality is by code unit, and so is the order, which is Rust's order (by Unicode
  scalar value) except between a character above U+FFFF and one from U+E000 to U+FFFF.
  A string literal pattern matches a `&str` exactly (it does not look through another
  reference, E0300 against `&&str`, as in rustc) and never makes a match exhaustive: the
  missing pattern is `&_`.
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
- Enums (#8). `E::V` is a value of type `E` for a unit variant; `E::V(a, b)` constructs a
  tuple variant (E0304 for the wrong count, rustc's E0061; `E::Unit()` is E0305, E0618);
  `E::V { F: a }` a struct variant, with a struct literal's checks. A tuple variant's
  constructor is not a value yet (E0317); a struct variant without braces is E0326. An
  explicit discriminant `V = e` is an `isize` constant; the others count up from the
  previous one (0 first). Duplicates, an implicit discriminant past `isize::MAX` and an
  explicit discriminant on an enum with a field (which needs `#[repr]`) are E0333 (rustc's
  E0081, E0370, E0732). A fieldless enum casts with `as` to any integer type, giving its
  discriminant, wrapped like an integer cast; nothing else casts from or to an enum
  (E0308). `#[derive(Copy)]` needs `Copy` payloads (E0417); an enum without the derive is
  moved, even a fieldless one. An enum that contains itself by value is E0206. An enum
  without variants is uninhabited: `match n {}` on it has type `!`.
- Pattern typing (#8). A pattern that is not a binding (a literal, range, path, tuple,
  tuple-variant or struct pattern) matched against a reference looks through it, and every
  binding inside binds by reference (`&` or `&mut` like the reference, `&` once any `&`
  was crossed): Rust 2021's default binding modes. `ref x` and `ref mut x` always bind a
  reference; `mut x` under a by-reference default binds by value again. A literal has the
  matched type (E0314 when it does not fit, E0300 for another type); a range needs an
  integer or `char` type, a lower bound not above the upper one (E0332, rustc's E0030),
  and for `a..b` a non-empty range (E0332, E0579). A variant pattern must be of the
  scrutinee's enum (E0300) and of the right kind (E0326: `V(x)` for a unit or struct
  variant, `V` for a tuple variant, rustc's E0532/E0164); a tuple-variant pattern has as
  many elements as the variant, `..` standing for any number (E0327, E0023); a struct
  pattern names existing fields (E0301, E0026) and all of them unless it ends with `..`
  (E0302, E0027). The bindings of an or-pattern have one type (E0300). A binding named like
  a unit variant of the enum it matches is E0325 (rustc's deny-by-default E0170).
- `match` (#8). The arms' bodies are checked against the expected type, or against the
  first one that does not diverge (E0300 "`match` arms have incompatible types"); a guard
  is `bool`. The match diverges when its scrutinee does or every arm does; without arms
  it is `!`. `if let` and `while let` type like `if` and `while`; a `let`-`else` block must
  diverge (E0300).
- Exhaustiveness and reachability (#8, decision 58), as rustc's usefulness algorithm
  decides them: a `match` must cover every value of its scrutinee (E0328, rustc's E0004,
  listing up to three missing patterns as rustc does: "`A`, `B` and `C(_)` not covered",
  "`i32::MIN..=-1_i32` and `3_i32..=i32::MAX` not covered", "type `i32` is non-empty" for
  a `match` without arms); guarded arms count for nothing; integers are covered by ranges
  that together span the type; `char` ranges cover Unicode scalar values (rustc's
  `0..=0xD7FF` and `0xE000..=0x10FFFF`), so covering only the Basic Multilingual Plane
  leaves `'𐀀'..='\u{10ffff}'` uncovered although a Sabiko `char` cannot hold those values
  (decision 31): Sabiko must not accept invalid Rust; floats are never covered by literals.
  A scrutinee reached through a dereference (`match *r`, or `match r.F` for a reference
  `r`, whose field access dereferences it) is not assumed valid, so its
  uninhabited variants must be matched, as in rustc. A `let` or parameter pattern must be
  irrefutable (E0329, rustc's E0005). Warnings: a pattern no value reaches (E0330, also a
  single alternative of an or-pattern), and an `if let`, `while let` or `let`-`else` whose
  pattern always matches (E0331).
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
    &A`). Two `&str` compare as strings (#10). Structs do not compare (operator traits
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
  mutable. Tuple elements are fields: everything below about `p.X` holds for `t.0`, and
  array elements likewise for `a[i]` (#9): writing or `&mut`-borrowing an element needs a
  `mut` array (E0401, E0402), an element of a slice behind `&[T]` is never writable
  (E0403, E0404) and one behind `&mut [T]` always is, whatever the binding. Writing an
  element of an array that may not be initialised is E0405.
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

Checked on the MIR by `sabiko_borrowck` (E0406–E0420), after the mutability and
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
- Nothing that is not `Copy` moves out of an array or slice element (#9), at any depth
  (`a[i]`, `a[i].Inner`, `s[0]` through a reference): E0420, rustc's E0508 ("cannot move
  out of type `[T; N]`, a non-copy array"); borrow the element instead. A whole array
  moves like any value (`let b = a;`, an argument, an array literal's elements move into
  it), and writing an element of a moved array is a use of the moved value (E0406).
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
  same field do. Indices are never compared (#9, as in rustc): `&mut a[0]` and `&mut a[1]`
  conflict, and so do a loan of `a[i]` and any access to `a[j]` or to `a` itself, such as
  `a.len()` (a shared borrow of `a`) while `&mut a[0]` is live; fields of elements stay
  disjoint (`&mut a[i].X` and `&mut a[j].Y`). Writing an element kills no loan.
- A loan of a place behind a shared reference (`let d = &*m;` for `m: &i32`, `&s[1..]`,
  `&**mr` for `mr: &mut &i32`) does not conflict with anything done to that reference
  itself or to what holds it: `&mut m`, `m = &y`, a retarget through `&mut m` and a move
  of `m` leave `d` pointing at the old referent, which stays borrowed while `d` lives
  (decision 65, as rustc). Through a `&mut` it does conflict: with `d = &*mm` live,
  `&mut mm` is E0408 and moving `mm` is E0412.
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

Patterns and `match` (#8, decision 57). A `match` evaluates its scrutinee once, into a
temporary unless it is a place, and reads only what its arms test: a variant pattern of
an enum with more than one variant reads the discriminant, a literal or range reads the
value; a lone `_` arm, a binding or a single-variant enum's pattern reads nothing. So
matching a partially moved enum is E0406 (rustc's E0382), matching a mutably borrowed one
E0411 (E0503), an uninitialised one E0405 (E0381), while `match c { _ => .. }` touches
nothing. A by-value binding copies or moves its part of the scrutinee (`Slot::Full(t)`
moves `t` out of `s`, leaving `s` partially moved; out of a reference it is E0407, out of
a value whose type implements `Drop` E0418); `ref`/`ref mut` bindings borrow it, and the
loan lives as long as the binding is used. Arms are tried in order; a guard runs only when
its pattern matched, once per matching alternative of an or-pattern, and a false guard
tries the next candidate. While a guard runs, the scrutinee is fake-borrowed: assigning
it or borrowing it mutably is E0419 (rustc's E0510), while moving it out is not (a move
leaves its memory as it was; decision 60), although an arm after a failed guard that
tests the moved scrutinee again is E0406, which rustc, testing once before any guard,
accepts (a known gap, `tests/pending/enums/known_gap/`); the arm's bindings denote shared
references to their parts, so a guard cannot move out of them, cannot assign a
binding (`mut n`) or write through one (`ref mut n`), both E0403 (rustc's E0594), and an
assignment to the scrutinee also conflicts with a binding's borrow (E0410, E0506). Once
the guard holds, the by-value bindings move or copy, so an arm that is not taken moves
nothing. Variants of one enum are disjoint places, as are their fields. A partially moved
enum is dropped field by field, each behind a drop flag set from the discriminant when the
enum is written, so only the variant it holds is dropped.

The two error families map onto rustc's as follows: E0406/E0407 ↔ E0382/E0507; E0408–E0412
↔ E0502/E0499/E0506/E0503/E0505; E0413 ↔ E0597 (E0716 for a temporary); E0414 ↔ E0515;
E0415 ↔ the uncoded "lifetime may not live long enough"; E0416 ↔ E0106; E0417 ↔ E0204;
E0418 ↔ E0509; E0419 ↔ E0510.
The differential harness compares verdicts (accept vs borrow error), not codes. The
mutability family maps as E0400 ↔ E0384, E0401/E0403 ↔ E0594, E0402/E0404 ↔ E0596.
