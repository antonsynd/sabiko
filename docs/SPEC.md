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
