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
- Statements and expressions
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

## 8. Memory model (Phase 7)
- `Box<T>`, `Gc<T>`, `Cell<T>`, `RefCell<T>`, `Arena`
- Strings and collections

## 9. .NET interop (Phase 8)
- Import mapping (`ref`/`in`/`out`, classes, exceptions, nullability)
- Export shape (attributes, `IDisposable`, naming)

## 10. Lowering to CIL (Phase 4 onward)
- Assembly and module layout
- Statement and expression lowering
- Verifiability guarantee

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
  expressions yet (E0315).

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
