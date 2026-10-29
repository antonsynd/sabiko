# Tests written ahead of a feature

Programs here are tests for a feature that is **not implemented yet**, written from the
language (Rust, as `rustc` judges it) rather than from the implementation, so the feature's
author starts with failing tests. No harness runs them: every harness walks a specific
directory (`tests/differential/`, `tests/differential/run/`, `tests/ui/`, `tests/run-pass/`),
and this one is none of them. The only readers are `crates/sabiko_driver/tests/no_panic.rs`
(which mutates every `.sabi` under `tests/` and only requires the pipeline not to panic) and
`scripts/fuzz.sh` (which seeds its corpus from `tests/`); both are fine with programs that
do not compile yet.

The feature's PR moves each file to the path its subdirectory mirrors, makes it pass, and
deletes the staging directory:

| staged under `tests/pending/<feature>/` | moves to | harness |
| --- | --- | --- |
| `differential/{loans,moves,mutability}/` | `tests/differential/...` | `cargo test -p sabiko_driver --test differential` |
| `differential/run/<topic>/` | `tests/differential/run/<topic>/` | `cargo test -p sabikoc --test differential_run` |
| `ui/` | `tests/ui/<stage>/` | `cargo test -p sabiko_driver --test ui` |
| `known_gap/` | stays out until the gap is fixed | none |

## `tuples/`: issue #6, tuples and `()` as a value

Every program was checked with rustc 1.99.1 (58cf8f9c3 2026-10-13), the version
`rust-toolchain.toml` pins, prepared exactly as the harnesses prepare it:

- compile-time programs (`differential/{loans,moves,mutability}`): the five-line prelude of
  `crates/sabiko_driver/tests/differential.rs` prepended, `rustc --edition 2021
  --crate-type lib --emit=metadata -A warnings`; `ok_*` are accepted and `err_*` are
  rejected with borrow-family codes only (the `RUSTC_BORROW_CODES` list there);
- run-time programs (`differential/run/tuples`): the prelude of
  `crates/sabikoc/tests/differential_run.rs` appended (so line numbers are the file's), its
  lints as `-A` flags, `--crate-type bin -C overflow-checks=on` (`off` for `//@ release`),
  run with `RUST_BACKTRACE=0`. That harness accepts no directive other than `//@ release`
  and compares against rustc live, so there are no goldens; each program has a sibling
  `.expected` note with rustc's stdout, exit code and panic report, for convenience only
  (delete them when moving the programs);
- UI programs (`ui/`): rejected by rustc with errors outside the borrow family. Each error
  line carries `//~ rustc E0xxx: <rustc's primary message>`, sometimes followed by
  `[<rustc's primary label>]`; `-` stands for an uncoded error. Every rustc error is
  annotated and every annotation was reported, at that line. The implementer replaces the
  annotations with `//~ ERROR <Sabiko code> ...` and blesses the `.stderr` files.

### Compile-time verdict pairs (`tuples/differential/`)

| file | tests | rustc |
| --- | --- | --- |
| `loans/ok_tuple_disjoint_fields.sabi` | `&mut t.0`, `&mut t.1`, `&mut t.2.0`, `&t.2.1` live together | accept |
| `loans/err_tuple_same_field_twice.sabi` | two `&mut t.0` | E0499 |
| `loans/err_tuple_field_then_whole.sabi` | `&t.1` live across `&mut t` | E0502 |
| `loans/err_tuple_assign_field_while_borrowed.sabi` | `t.1.0 = 5` while `&t.1.0` is live | E0506 |
| `loans/err_tuple_field_read_after_explicit_mut_arg.sabi` | `Use(&mut t, t.0)`: an explicit `&mut` argument is not two-phase | E0503 |
| `loans/ok_tuple_reborrow_fields.sabi` | disjoint `&mut t.0`/`&mut t.1` behind `t: &mut (P, P)`; reborrow chain `&mut r.0` -> `&mut f.X` | accept |
| `loans/ok_tuple_of_refs.sabi` | `(&a, &mut b)` with `*p.1 += *p.0` | accept |
| `loans/err_tuple_of_refs_conflict.sabi` | `(&mut a, &a)` | E0502 |
| `loans/err_tuple_ref_outlives_referent.sabi` | `p = (&x, 2)` with `x` dying before `p` is used | E0597 |
| `loans/ok_tuple_return_ref_to_field.sabi` | `fn(&(i32, (u8, bool))) -> &u8 { &t.1.0 }`, and the `&mut` version | accept |
| `loans/err_tuple_return_ref_to_local_field.sabi` | returning `&u.0` of a local copy | E0515 |
| `moves/err_tuple_partial_move_then_field.sabi` | move `t.0`, then read `t.0.X` | E0382 |
| `moves/ok_tuple_partial_move_sibling.sabi` | move `t.0`, then read, borrow and move `t.1` | accept |
| `moves/err_tuple_partial_move_then_whole.sabi` | move `t.1`, then move `t` | E0382 ("partially moved") |
| `moves/err_tuple_move_out_of_ref.sabi` | `Take(r.0)` for `r: &(P, i32)`; `let (a, b) = *r` for `r: &mut (P, P)` | E0507 (twice) |
| `moves/ok_tuple_copy_out_of_ref.sabi` | Copy fields (and a Copy sub-tuple) read out of `&(P, i32, (u8, bool))` | accept |
| `moves/ok_tuple_of_copy_is_copy.sabi` | a tuple of `#[derive(Copy)]` struct and primitives is Copy | accept |
| `moves/err_tuple_with_move_field_is_move.sabi` | one non-Copy element, nested, makes the tuple a move type | E0382 |
| `moves/ok_tuple_wildcard_does_not_move.sabi` | `let (a, _) = t;` moves only `t.0`; `t.1` is still movable | accept |
| `moves/err_tuple_destructure_then_use.sabi` | `let (_, (b, n)) = t;` moved `t.1.0` | E0382 |
| `moves/ok_tuple_reinit_moved_field.sabi` | assigning a moved-out field makes the whole tuple usable again | accept |
| `moves/err_tuple_mut_ref_field_moved.sabi` | `let r = t.0;` moves a `&mut` element; `*t.0` after it | E0382 |
| `mutability/err_tuple_field_assign_immutable.sabi` | `t.0 = ..`, `t.1.1 += ..` without `mut` | E0594 (twice) |
| `mutability/err_tuple_field_mut_borrow_immutable.sabi` | `&mut t.1` without `mut` | E0596 |
| `mutability/ok_tuple_field_assign_mut.sabi` | assignment, compound assignment, nested and whole-tuple assignment, `&mut` of a field | accept |
| `mutability/err_tuple_field_through_shared.sabi` | `r.0 = 1` and `&mut r.1.0` through `&(..)` | E0594, E0596 |
| `mutability/ok_tuple_field_through_mut.sabi` | every field writable through a non-`mut` `&mut (..)` parameter | accept |
| `mutability/err_tuple_pattern_binding_immutable.sabi` | `let (a, mut b)`: `a` stays immutable | E0384 |
| `mutability/ok_tuple_pattern_mut_bindings.sabi` | `mut` bindings in nested patterns, assigned and `&mut`-borrowed | accept |
| `mutability/err_tuple_field_of_uninit.sabi` | `let mut t: (i32, i32); t.0 = 1;` (no field-by-field init) | E0381 |
| `mutability/ok_tuple_deferred_init.sabi` | `let t: (i32, bool);` assigned once on each branch | accept |

### Run-time programs (`tuples/differential/run/tuples/`)

| file | tests | rustc |
| --- | --- | --- |
| `basics.sabi` | mixed element types (`i128`, `f32`, `char`, ...), nested, 1-tuples, chained `t.0.1`, `.1.1.0` on a call result, tuple params/returns, `(5)` vs tuples, `()` in a tuple type | exit 0 |
| `destructuring.sabi` | flat/nested/`_`/`mut`/annotated patterns, `let (_, _) = (PrintLine(1), PrintLine(2))` still evaluates both, `()` in patterns, swap via `let (x, y) = (y, x);`, tuple-valued `if` | exit 0 |
| `assignment.sabi` | field, compound (`+= -= <<= %= ^=`), nested and whole-tuple assignment; `t = (t.0 + 1, .., t.2 * t.0 as i64)` and `pair = (pair.1, pair.0 + pair.1)` read the place being overwritten | exit 0 |
| `refs.sabi` | auto-deref `r.0`, `rr.1.1` through `&&`, `(*r).1`, `&mut t.1.0` writes, `-> &mut i32` into a tuple, `(&mut i64, &i64)` parameter, tuple of references, comparing `&f64` elements | exit 0 |
| `structs.sabi` | tuples of Copy structs, struct fields of tuple type, copy-then-mutate independence, non-Copy tuples moved through functions and destructured | exit 0 |
| `unit_values.sabi` | `()` as a parameter, struct field (also in a Copy struct), tuple element, local, explicit `-> ()` return; `() == ()`, `!=` | exit 0 |
| `eval_order.sabi` | left-to-right evaluation of (nested) elements with side effects; `(c, Next(&mut c), c)` reads each `c` when reached (4 5 5); `v.1 += Bump(&mut v)` runs the right side first | exit 0 |
| `control_flow.sabi` | tuples from `return`, `break (i, i * i)`, blocks and `if`; a best-so-far tuple updated in a loop | exit 0 |
| `wide_arity.sabi` | 8-, 9- and 16-element tuples: past `System.ValueTuple`'s seven direct fields (`TRest`) | exit 0 |
| `one_tuple_vs_parens.sabi` | `(5)` vs `(5,)`, `((one,),)`, `(one).0`, `(one.0,).0`, `((1, 2), (3))` | exit 0 |
| `panic_in_construction.sabi` | a callee panics while the third element of a nested tuple is built; earlier elements ran, later ones did not | exit 101, `element too large` at 8:16 (the `Panic` in the callee) |
| `panic_direct_element.sabi` | `if flag { Panic("direct") } else { false }` as an element (`!` coerces) | exit 101, `direct` at 4:40 |
| `overflow_in_element.sabi` | `u8` overflow in the second element `a + b` of a returned tuple | exit 101, `attempt to add with overflow` at 3:13 |
| `overflow_compound_field.sabi` | `t.1.0 += 1` overflowing an `i16` field in a loop | exit 101, `attempt to add with overflow` at 5:9 |
| `release_wrapping.sabi` | `//@ release`: wrapping `+ - *`, unary `-` of `i8::MIN`, shifts on tuple elements | exit 0 |
| `equality.sabi` | **possibly beyond #6**: tuple `== != < > <=`, nested, `()`, `-0.0`, NaN in first/last position, `*r == b`, `r == &b` | exit 0 |
| `binding_modes.sabi` | **possibly beyond #6**: `let (a, b) = &t;` / `= &mut t;` bind references (default binding modes) | exit 0 |

### UI programs (`tuples/ui/`), rejected by rustc with non-borrow errors

| file | tests | rustc | stage |
| --- | --- | --- | --- |
| `arity_mismatch.sabi` | `let (a, b) = (1, 2, 3)` and the reverse, annotation, argument, return value, nested 1-tuple | E0308 x6 | typeck |
| `field_out_of_range.sabi` | `t.2` on a pair, `n.0.2`, `.0` on an integer element, `r.5` through `&`, `().0` | E0609, E0610 | typeck |
| `mismatched_elements.sabi` | wrong element types in literal, argument, return, field assignment, whole assignment, annotated pattern, `()` vs `(1,)` | E0308 x8 | typeck |
| `field_on_non_tuple.sabi` | `.0` on an integer, a float, a named-field struct and `&P`; `t.X` on a tuple | E0609, E0610 | typeck |
| `one_tuple_vs_parens.sabi` | `(5)` where `(i32,)` is expected and vice versa, `(5).0`, `(i32)` as a type | E0308, E0610 | typeck |
| `tuple_operators.sabi` | `+` and unary `-` on tuples, `PrintLine(t)` and `Print(())` (not `Display`), `t as i32`, `==`/`<` between different tuple types | E0369, E0600, E0277, E0605, E0308 | typeck |
| `literal_out_of_range_element.sabi` | `300` for a `u8` element, `-129` for a nested `i8` | uncoded `literal out of range` (deny lint) | typeck (Sabiko E0314) |
| `duplicate_binding_and_struct_pattern.sabi` | `let (x, x) = t;`, `let (p,) = P { .. };` | E0416, E0308 | resolve / typeck |
| `refutable_pattern.sabi` | `let (1, b) = t;` (a refutable pattern; alone because rustc checks it only once type checking succeeded) | E0005 | parse (E0107) or a pattern check |
| `tuple_index_syntax.sabi` | `t.0u8`, `t.01`, `t.0x1` | uncoded `suffixes on a tuple index are invalid`, E0609 x2 | parse / typeck |

### Known gap (`tuples/known_gap/`)

| file | tests | rustc |
| --- | --- | --- |
| `ok_two_phase_reborrow_tuple_field.sabi` | `Use(m, m.0)` for `m: &mut (i32, i32)`: the implicit reborrow of `m` is two-phase in rustc, and Sabiko rejects it (the existing gap waiting for methods and two-phase borrows, PLAN.md Phase 5) | accept |

### Notes for the implementer

- `equality.sabi` needs `PartialEq`/`PartialOrd` on tuples. Drop it, or keep it as the
  specification, if #6 leaves tuple comparison to the operator traits of Phase 6. Rust's
  lexicographic `<`/`<=` stops at the first element pair that is not `Equal`, *including an
  unordered (NaN) pair*, and the last pair uses the operator itself, so `(1.0, NaN) <=
  (1.0, NaN)` and `(NaN, 1) >= (NaN, 0)` are both false (neither is `!(b < a)`).
- `binding_modes.sabi` uses Rust 2021 default binding modes. Sabiko must either implement
  them or reject the program, and rejecting it diverges from rustc. Out of #6's stated
  scope, but a `let` with tuple patterns will hit it as soon as someone destructures a
  reference.
- `wide_arity.sabi` matters for the representation decision: `System.ValueTuple` has seven
  direct fields and nests the rest in `TRest`, so `t.7` is `t.Rest.Item1` there. Generated
  sequential structs have no such limit.
- Tuples of references (`refs.sabi`, `loans/*refs*`) cannot be a `System.ValueTuple` of
  managed pointers unless the type is a `ref struct`. `&str` (`ReadOnlySpan<char>`) has the
  same problem, so no program here puts a `&str` in a tuple.
- `unit_values.sabi` stores `()` in struct fields (one of them in a Copy struct) and passes
  it as an argument. The codegen E0500 for `&()` (`tests/ui/codegen/unsupported.sabi`) is
  not exercised, because #6 does not mention it.
- `ok_tuple_wildcard_does_not_move.sabi`: `_` binds nothing, so it must not move. Neither
  must `let (_, _) = (..)` skip evaluating the initialiser (`destructuring.sabi` prints 1
  and 2).
- `eval_order.sabi`: in `v.1 += Bump(&mut v)` on integers, rustc evaluates the right side
  first and then reads `v.1` (109, not 9). Sabiko already does the same for struct fields.
- Panic locations: an overflowing element is located at the start of its own expression
  (`a + b`, 3:13), not at the tuple's `(`.
