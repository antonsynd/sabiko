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

## `tuples/`: issue #6, tuples and `()` as a value (landed)

The staged programs were checked with rustc 1.99.1 (58cf8f9c3 2026-10-13), the version
`rust-toolchain.toml` pins, and moved into their harnesses by #6 (decision 52):

| staged | now |
| --- | --- |
| `differential/{loans,moves,mutability}/*_tuple_*.sabi` (31 programs) | `tests/differential/{loans,moves,mutability}/`, unchanged |
| `differential/run/tuples/*.sabi` but `equality.sabi` (16 programs, `binding_modes.sabi` included: default binding modes are implemented) | `tests/differential/run/tuples/`; the `.expected` notes are deleted. `refs.sabi` reads `r.1.1` through one reference instead of two |
| `ui/arity_mismatch`, `duplicate_binding_and_struct_pattern`, `field_on_non_tuple`, `field_out_of_range`, `literal_out_of_range_element`, `mismatched_elements`, `one_tuple_vs_parens`, `tuple_operators` | `tests/ui/typeck/tuple_*.sabi` with Sabiko's codes (E0300, E0301, E0306–E0308, E0314, E0209) |
| `ui/refutable_pattern` | `tests/ui/parse/tuple_refutable_pattern.sabi` (E0107: no literal patterns before `match`) |
| `ui/tuple_index_syntax` | split: `tests/ui/parse/tuple_index_suffix.sabi` (E0100) and `tests/ui/typeck/tuple_index_not_decimal.sabi` (E0301), since a parse error stops before type checking |

### Known gaps (`tuples/known_gap/`)

| file | gap | rustc |
| --- | --- | --- |
| `equality.sabi` | tuple `==`, `!=`, `<`, ... (also of `()` against `()` inside tuples): E0107 until the comparison traits of Phase 6 | runs, exit 0 |
| `refs_through_double_ref.sabi` | `rr.1.1` for `rr: &&(i32, (i32, i32))`: code generation rejects `&&T` (E0500) until `Ref<T>` (Phase 5) | runs, prints 3 |

## `methods/`: issues #7 (inherent `impl` blocks and methods) and #15 (two-phase borrows) (landed)

The staged programs were checked with rustc 1.99.1 (58cf8f9c3 2026-10-13) as the harnesses
prepare them (the compile-time ones with the prelude of
`crates/sabiko_driver/tests/differential.rs`, the run-time ones with that of
`crates/sabikoc/tests/differential_run.rs`, the UI ones annotated `//~ rustc E0xxx`), and
moved into their harnesses by #7 and #15 (decision 53). All of them agree with rustc; none
needed a correction.

| staged | now |
| --- | --- |
| `differential/{loans,moves,mutability}/*.sabi` (50 programs) | `tests/differential/{loans,moves,mutability}/`, unchanged |
| `differential/run/methods/*.sabi` (13 programs, `self_type.sabi` included: `Self` is supported) | `tests/differential/run/methods/`; the `.expected` notes are deleted |
| `ui/duplicate_method`, `path_to_non_type`, `self_value_outside_method`, `unqualified_associated_function` | `tests/ui/resolve/` with Sabiko's codes (E0210, E0201, E0203, E0212, E0213, E0202) |
| `ui/method_call_mismatched_args`, `method_on_primitive`, `receiver_type_mismatch`, `unknown_associated_function`, `unknown_method`, `wrong_arg_count` | `tests/ui/typeck/` (E0300, E0211, E0319, E0212, E0320, E0304) |
| `ui/self_param_in_free_function` | `tests/ui/parse/` (E0110) |
| `tuples/known_gap/ok_two_phase_reborrow_tuple_field.sabi` | `tests/differential/loans/` |

Each converted UI program names the rustc codes it was checked against in a comment.

### Known gaps (`methods/known_gap/`)

| file | gap | rustc |
| --- | --- | --- |
| `double_ref_receivers.sabi` | run-time `&&P` and `&mut &mut P` receivers and a `&&P` parameter: borrow checking accepts them (`tests/differential/loans/ok_autoderef_receivers.sabi`), code generation rejects `&&T` (E0500) until `Ref<T>` (Phase 5) | exit 0 (`.expected`) |
| `associated_consts.sabi` | `const` items in an impl block, `Grid::Cells`, `Self::Cells`: E0107 (associated constants are a later step) | exit 0 (`.expected`) |

### Two-phase borrows: the rules rustc applies

Each rule has a compile-time program above and a hand-built MIR test in
`crates/sabiko_borrowck/tests/loans.rs`.

1. **Autoref'd `&mut self` receiver.** The receiver borrow is *reserved* when the receiver
   is evaluated, before the arguments run, and *activated* at the call. Until then it
   behaves like a shared borrow: later arguments may read the receiver directly, through
   fields or through `&self` methods (`ok_two_phase_autoref_receiver`). This also holds for
   a field path (`a.B.Add(a.B.Get())`), for a `&mut P` binding (`m.Add(m.Get())`, the
   autoref is the reborrow `&mut *m`), and for `self` in a `&mut self` method
   (`ok_two_phase_receiver_through_mut_ref`).
2. **Implicit reborrow of a `&mut` argument.** Passing `m: &mut P` where `&mut P` is
   expected reborrows `&mut *m`, and that reborrow is two-phase: `Use(m, m.X)`,
   `Use(m, m.Get())`, `P::Add(m, m.Get())` (`ok_two_phase_reborrow_argument`).
3. **Explicit borrows are never two-phase**, whatever their position:
   `P::Add(&mut v, v.Get())` (E0502), `Use(&mut *m, m.X)` (E0503), `(&mut v).Add(v.Get())`
   (E0502). A receiver that is a call returning `&mut` (`a.BMut().Add(a.N)`) is a normal
   borrow made before the arguments, so it is E0503 too: only the outermost implicit
   reborrow of such a receiver is two-phase.
4. **During the reservation, later arguments may only read**: a mutable borrow
   (`v.Add(v.Bump())`, `Use(m, m.Bump())`: E0499), an assignment (`v.Add({ v.X = 5; 2 })`:
   E0506) and a move out of a field (`v.Store(v.Inner)`: E0505) are errors.
5. **At activation, no other borrow of the place may be live.** A shared borrow created by
   an argument and passed to the call conflicts (`v.AddFrom(&v)`: E0502). A value computed
   from the receiver does not (`v.Add(v.Get())`).
6. **A shared borrow taken before the reservation is tolerated** while it is only used up
   to the activation: `let s = &v; v.Add(s.Get());` is accepted, and using `s` after the
   call is E0502.
7. **Shared receivers are not two-phase.** A `&self` receiver is borrowed before the
   arguments and stays borrowed through the call; an argument that writes the receiver is
   E0506 (`err_shared_receiver_arg_writes`).

## `enums/`: issue #8, enums, `match`, patterns, exhaustiveness

Every program was checked with rustc 1.99.1 (58cf8f9c3 2026-10-13), the version
`rust-toolchain.toml` pins, prepared exactly as the harnesses prepare it (the same three
preparations as for `drops/`):

- compile-time programs (`differential/{loans,moves,mutability}`): the five-line prelude of
  `crates/sabiko_driver/tests/differential.rs` prepended, `rustc --edition 2021
  --crate-type lib --emit=metadata -A warnings`; `ok_*` are accepted, and `err_*` are
  rejected with borrow-family codes only (`RUSTC_BORROW_CODES`);
- run-time programs (`differential/run/enums`): the prelude of
  `crates/sabikoc/tests/differential_run.rs` appended (line numbers are the file's), its
  lints as `-A` flags, `--crate-type bin -C overflow-checks=on` (`off` for `//@ release`),
  run with `RUST_BACKTRACE=0`. Each has a sibling `.expected` note with rustc's stdout,
  exit code and panic report, for convenience only (delete them when moving the programs);
- UI programs (`ui/`): each rustc error line carries `//~ rustc E0xxx: <primary message>`
  (`-` for an error without a code), sometimes followed by `[<primary label>]`. **Warnings**
  are annotated `//~ rustc warning: <message>`; they were checked by a second rustc run
  without `-A warnings`, keeping the harness's lint allows but with `-W unreachable_patterns`
  (that lint belongs to the `unused` group the harnesses allow). Every rustc error and
  warning is annotated, and every annotation was reported at that line. Sabiko's UI
  harness already accepts `//~ WARN`; the implementer converts both kinds and blesses the
  `.stderr` files.

The programs use what SPEC.md describes, tuples (#6), methods (#7) and, where marked
"(drops, #12)", `impl Drop` (decision 46), which lands before #8: four programs
(`ok_match_drop_payload_not_moved`, `err_match_drop_payload_moved_then_used`,
`run/enums/drops_in_match`, `ui/move_out_of_drop_enum`) plus
`run/enums/if_let/scrutinee_temporaries`. There are no generics: optional values are a
user `enum MaybeI32 { None, Some(i32) }`, and nothing compares enums with `==`.

### `if let`, `while let`, `let else`: the programs that need a decision

They are staged in `if_let/` subdirectories, which the harnesses also walk once moved
(every collector recurses): `differential/run/enums/if_let/` (4 programs),
`differential/moves/if_let/` (3), `differential/loans/if_let/` (2), `ui/if_let/` (3).
Recommendation: include `if let` and `while let` in #8. They are a `match` with one
tested arm and a fallback, so they reuse match lowering, exhaustiveness (none needed) and
the borrow checker unchanged; the one new rule is the edition 2021 scrutinee-temporary
scope (rule 9). `let else` is the same lowering with a diverging fallback plus the
"does not diverge" check, and can follow in the same PR or later. Nothing outside the
`if_let/` folders uses any of the three, and `refutable_pattern_in_let` only points at
`let else` in its comment.

### Compile-time verdict pairs (`enums/differential/`)

| file | tests | rustc |
| --- | --- | --- |
| `moves/err_match_binding_moves_payload.sabi` | `Slot::Full(t) => Consume(t)`, then `let again = s;` | E0382 ("partially moved") |
| `moves/ok_match_copy_payload_binding.sabi` | Copy payloads (tuple and struct variant, `..` over a non-Copy field) are copied; the scrutinee is moved afterwards | accept |
| `moves/ok_match_ref_binding_does_not_move.sabi` | `ref t`, `_` and `..` move nothing; `Take(s)` after three matches | accept |
| `moves/err_match_partial_move_then_match_again.sabi` | `Pair(a, _)` moves `a`; matching `s` again for `Pair(_, b)` reads the discriminant of a partially moved value | E0382 at the second scrutinee |
| `moves/ok_match_move_then_reinit.sabi` | moved out, reassigned, matched again; per loop iteration; reassigned by the arm that moved | accept |
| `moves/err_match_move_in_one_arm_then_use.sabi` | a move in one arm, a `ref` in another, then `Take(s)` | E0382 |
| `moves/ok_match_move_in_diverging_arm.sabi` | the moving arms `return` or panic, so `Take(s)` after the match sees a whole value | accept |
| `moves/err_match_by_value_through_shared_ref.sabi` | `match *r { Slot::Full(t) => .. }` | E0507 |
| `moves/err_match_by_value_through_mut_ref.sabi` | the same through `&mut` | E0507 |
| `moves/ok_match_default_binding_through_ref.sabi` | `match r` for `r: &Slot` and `match &s` bind `&Token` and `&i32` | accept |
| `moves/err_match_mut_binding_through_ref_moves.sabi` | `Slot::Full(mut t)` against a `&Slot`: Rust 2021 resets `mut` to by-value | E0507 |
| `moves/err_match_move_in_loop.sabi` | a moving match in a loop body without reinitialising | E0382 |
| `moves/err_match_catch_all_binding_moves.sabi` | `other => Take(other)`, then `Take(l)` | E0382 |
| `moves/err_match_at_binding_moves_whole.sabi` | `whole @ Slot::Full(_)` moves the scrutinee | E0382 |
| `moves/err_enum_not_copy_without_derive.sabi` | a fieldless enum without the derive passed twice | E0382 |
| `moves/ok_enum_derive_copy.sabi` | `#[derive(Copy, Clone)]` enums with Copy payloads, in a Copy struct, copied out of `*r` | accept |
| `moves/ok_match_drop_payload_not_moved.sabi` | (drops) `ref`, `_`, `..` leave a droppable payload; moving both out then reinitialising | accept |
| `moves/err_match_drop_payload_moved_then_used.sabi` | (drops) one droppable field moved out of an enum without `Drop`, then the enum passed on | E0382 |
| `moves/if_let/err_if_let_moves_payload.sabi` | `if let Slot::Full(t) = s { Consume(t) }`, then `Take(s)` | E0382 |
| `moves/if_let/ok_if_let_ref_binding.sabi` | `ref` and `_` in `if let`/`while let`, a moving `let else` | accept |
| `moves/if_let/err_while_let_moves_in_loop.sabi` | `while let Slot::Full(t) = s { Consume(t) }` | E0382 |
| `loans/err_match_ref_binding_then_assign_scrutinee.sabi` | `Cell::Value(ref n) => { c = ..; *n }` | E0506 |
| `loans/ok_match_copy_binding_then_assign_scrutinee.sabi` | the same with a copied `n`, and with a `ref` binding already dead | accept |
| `loans/err_match_ref_mut_binding_then_read_scrutinee.sabi` | `ref mut c`, then `Peek(&s)` before the last use of `c` | E0502 |
| `loans/ok_match_ref_mut_binding_mutates_payload.sabi` | `ref mut` writes, two disjoint `ref mut` fields, then reads of `s` | accept |
| `loans/err_match_mut_default_binding_then_read.sabi` | `match &mut c { Cell::Value(n) => { Peek(&c); *n += .. } }` | E0502 |
| `loans/err_match_result_borrow_outlives_assignment.sabi` | `let id = match &s { Full(t) => &t.Id, .. }; s = ..; *id` | E0506 |
| `loans/ok_match_result_borrow_dead_before_assignment.sabi` | the same with `*id` used before `s = ..` | accept |
| `loans/err_match_scrutinee_while_mut_borrowed.sabi` | `let m = &mut c; match c { Cell::Value(n) => .. }; *m = ..` | E0503 (x2: discriminant, then `c.0`) |
| `loans/ok_match_wildcard_while_mut_borrowed.sabi` | the same with a lone `_` arm (enum and `i32`), and `One::Only(_)` on a single-variant enum | accept |
| `loans/err_match_move_binding_while_borrowed.sabi` | `let r = &s;` then a moving binding, then `r` used | E0505 |
| `loans/ok_match_guard_reads_scrutinee.sabi` | guards read the scrutinee (`Peek(&c)`), bindings, write other locals; the body assigns the scrutinee | accept |
| `loans/err_match_guard_binding_blocks_assignment.sabi` | `Cell::Value(n) if { c = Cell::Empty; n > 0 }` | E0506 (the guard's binding borrows `c`) |
| `loans/err_match_ref_binding_then_borrow_mut.sabi` | `Pair(ref a, _) => { Bump(&mut s); a.Id }` | E0502 |
| `loans/if_let/err_while_let_ref_binding_then_assign.sabi` | `while let Cell::Value(ref n) = c { c = ..; *n }` | E0506 |
| `loans/if_let/ok_if_let_mut_borrow_not_live_in_else.sabi` | `if let .. = &mut c { .. } else { c = .. }` | accept |
| `mutability/err_match_assign_immutable_binding.sabi` | `Cell::Value(n) => { n += 1; .. }` | E0384 |
| `mutability/ok_match_mut_binding.sabi` | `mut n`, `mut small @ 0..=9`, `mut other` | accept |
| `mutability/err_match_ref_mut_of_immutable_scrutinee.sabi` | `ref mut n` on a non-`mut` local | E0596 |
| `mutability/err_match_assign_through_shared_default_binding.sabi` | `match c` for `c: &Cell`, `*n = 0` | E0594 |
| `mutability/ok_match_assign_through_mut_default_binding.sabi` | `match c` for `c: &mut Cell`, and `match &mut c`, write through the bindings | accept |
| `mutability/err_match_guard_assigns_mut_binding.sabi` | `Cell::Value(mut n) if { n += 1; .. }` | E0594 ("immutable for the pattern guard") |
| `mutability/err_match_guard_writes_through_ref_mut.sabi` | `Cell::Value(ref mut n) if { *n += 1; .. }` | E0594 (same) |
| `mutability/err_match_init_in_some_arms.sabi` | `let wait;` assigned in two of three arms | E0381 |
| `mutability/ok_match_init_in_every_arm.sabi` | assigned once in every arm but a `return` arm | accept |
| `mutability/err_match_uninit_scrutinee.sabi` | `let l: Light; match l { .. }` | E0381 |
| `mutability/err_match_assign_in_arm_after_init.sabi` | `let wait = 10;` then `wait = 30` in an arm | E0384 |

### Run-time programs (`enums/differential/run/enums/`)

| file | tests | rustc |
| --- | --- | --- |
| `basics.sabi` | unit, tuple and struct variants built (struct fields in any order) and matched; enums as arguments and results; a match as a value | exit 0 |
| `literals_and_ranges.sabi` | integer, `char`, `bool`, `f64`-condition and `()` literals; `a..=b`, one exclusive `4..10`, negative bounds, `-2147483648..=-1`; or-patterns; `d @ '0'..='9'`, `small @ (1 \| 2 \| 4 \| 8)`; exhaustive `u8` (`0`, `1..=127`, `128..=254`, `255`), `i8` (`-128..=-1`, `0..=127`) and `(bool, bool)` without `_` | exit 0 |
| `arm_order_and_guards.sabi` | first match wins; guards only for matching patterns, false guards fall through (to the same pattern again); an or-pattern's guard runs once per matching alternative (`guard 5` twice) | exit 0 |
| `evaluation_order.sabi` | the scrutinee once, before any test; guards top to bottom; a body after its guard; a match inside a guard, a tuple scrutinee evaluated left to right, a match in an arm | exit 0 |
| `nested_patterns.sabi` | struct patterns with `..` and shorthand, enums in structs in enums, tuple-variant `..` at start/middle/end, literals deep inside, `(a, b)` scrutinees with or-patterns | exit 0 |
| `binding_modes.sabi` | `&Cell` and `&mut Cell` scrutinees bind `&T`/`&mut T`; `ref`, `ref mut`, `mut` copies; writes change the scrutinee only through references | exit 0 |
| `copy_enums.sabi` | derived Copy enums assigned, passed, matched by value, copied out of `*held`, inside a Copy struct | exit 0 |
| `discriminants.sabi` | `as` casts of fieldless enums: implicit 0, 1, 2; explicit `-2`, implicit `-1`, `300`, `301`, `10`; wrapping (`300 as u8` = 44, `-2 as u8` = 254, `-2 as u64`) | exit 0 |
| `state_machine.sabi` | a `loop` over `state = match state { .. }` with guards, `break n * 1000 + steps` and `continue` from arms | exit 0 |
| `calculator.sabi` | a stack machine: instructions and outcomes as enums with payloads, `(op, b)` tuple scrutinees, guards on `s.Len`, `return` from arms, nested matches as values | exit 0 |
| `diverging_arms.sabi` | `return`, `continue`, `break`, `Panic` arms typed `!`; a match whose arms all return | exit 0 |
| `empty_enum.sabi` | `enum Never {}`; `match n {}` for `n: Never` and `*r` for `r: &Never`, as `i32`, `bool` and a struct | exit 0 |
| `single_variant.sabi` | irrefutable single-variant patterns in `let`, parameters and a one-arm match; struct patterns in `let` and parameters; a parenthesised struct-variant scrutinee | exit 0 |
| `methods_on_enums.sabi` | `Self::V` constructors and patterns, `match self` on `&self`, `*self = match self { .. }`, `Self::Green { Seconds }` through `&mut self`, by-value `self` | exit 0 |
| `enums_in_aggregates.sabi` | enums in struct fields and tuples, structs and tuples in payloads, `match w.Status`, `match t.1`, assignment to `t.1` in its own match | exit 0 |
| `ident_patterns.sabi` | a constant (`LIMIT`, `NEG`) and a unit struct as patterns compare, they do not bind | exit 0 |
| `drops_in_match.sabi` | (drops) a moved binding dropped at arm end, the unbound field at scope end; a temporary scrutinee dropped after the arm; `let kept = match Make() { Two(a, _) => a, .. }` drops the rest at the end of the `let` | exit 0 |
| `panic_in_arm.sabi` | `Panic` in an arm after output | exit 101, `negative value` at 11:13 |
| `panic_in_guard.sabi` | a guard not evaluated when its pattern fails or an earlier arm wins; `100 / d` panicking in a guard | exit 101, `attempt to divide by zero` at 8:27 |
| `overflow_in_arm.sabi` | `i8` addition overflowing in an arm | exit 101, `attempt to add with overflow` at 8:37 |
| `release_wrapping_in_arm.sabi` | `//@ release`: the same arms wrap (100 + 28 = -128, `i32::MAX + 1`) | exit 0 |
| `if_let/if_let.sabi` | `if let`, `else if let` with or-patterns and `@` ranges, `ref mut`, an expression value, a tuple scrutinee | exit 0 |
| `if_let/while_let.sabi` | draining a stack whose `Pop` returns `MaybeI32`, `break`/`continue`, a `while let` over a reassigned state | exit 0 |
| `if_let/let_else.sabi` | `let .. else { return }`, `{ break }`, `{ Panic(..) }`; `let (A(a, b) \| B(a @ b)) = c else ..` | exit 0 |
| `if_let/scrutinee_temporaries.sabi` | (drops) edition 2021: `if let` and `match` scrutinee temporaries dropped after the `else`/arm; `while let`'s per iteration; an `if` condition's before the body | exit 0 |

### UI programs (`enums/ui/`), rejected by rustc with non-borrow errors (or warnings)

| file | tests | rustc | stage |
| --- | --- | --- | --- |
| `non_exhaustive_missing_variant.sabi` | one variant missing; three missing ("`A`, `B` and `C(_)` not covered") | E0004 x2 | exhaustiveness (typeck or MIR build) |
| `non_exhaustive_nested.sabi` | `Cell::Value(false)`, `Shape::Rect { Filled: false, .. }`, `(Cell::Empty, false)` | E0004 x3 | exhaustiveness |
| `non_exhaustive_integer.sabi` | `i32` (`i32::MIN..=-1_i32` and `3_i32..=i32::MAX`), `u8` (`u8::MAX`), `i8` (`0_i8`) | E0004 x3 | exhaustiveness |
| `non_exhaustive_bool_char.sabi` | `(false, true)`; `char` gaps (surrogates excluded, "and 1 more"); all of the BMP still leaves `'𐀀'..='\u{10ffff}'` | E0004 x3 | exhaustiveness |
| `non_exhaustive_guard.sabi` | guards `n >= 0` and `n < 0` do not count | E0004 | exhaustiveness |
| `match_without_arms_on_inhabited_type.sabi` | `match l {}` on an enum, an `i32`, a struct | E0004 x3 | exhaustiveness |
| `unreachable_arms.sabi` | after `_`, a repeated variant, a range covered by two ranges, one alternative of `1 \| 20`; guarded arms do not count | 4 warnings, accepted | exhaustiveness (`//~ WARN`) |
| `refutable_pattern_in_let.sabi` | `let Cell::Value(n) = c;`, `let (0, m) = t;` | E0005 x2 | exhaustiveness |
| `refutable_pattern_in_parameter.sabi` | `fn F((0, b): ..)`, `fn G(Cell::Value(n): Cell)` | E0005 x2 | exhaustiveness |
| `or_pattern_binding_missing.sabi` | `Square(w) \| Rect(w, h)`, `Square(a) \| Dot` | E0408 x2 | resolve |
| `or_pattern_binding_type_mismatch.sabi` | `Int(x) \| Flag(x)` | E0308 | typeck |
| `or_pattern_binding_mode_mismatch.sabi` | `Left(ref x) \| Right(x)` | E0409, E0308 | resolve / typeck |
| `or_pattern_in_let_needs_parens.sabi` | `let A(x) \| B(x) = e;` (parenthesised: accepted) | syntax error | parse |
| `duplicate_binding_in_pattern.sabi` | `Rect { W: a, H: a }`, `Pair(b, b)` | E0416 x2 | resolve (Sabiko's E0209) |
| `variant_pattern_shape.sabi` | wrong tuple arity, unknown field, missing field without `..`, `Dot(x)`, `Square` without parens, `Rect(w, h)` | E0023, E0026, E0027, E0532 x2, E0164 | resolve / typeck |
| `variant_construction_shape.sabi` | `Square(1, 2)`, `Rect { W: 1 }`, `Dot()`, `Circle(3)` | E0061, E0063, E0618, E0599 | typeck / resolve |
| `match_arm_type_mismatch.sabi` | arms `i32` and `bool`; an `i32` guard | E0308 x2 | typeck |
| `pattern_type_mismatch.sabi` | `'a'` against `i32`, `Dir::North` against `Light`, a 3-tuple against a pair | E0308 x3 | typeck |
| `range_pattern_errors.sabi` | `5..=1`, `3..3` | E0030, E0579 | typeck (or const eval) |
| `literal_pattern_out_of_range.sabi` | `0..=300` for `u8`, `128` for `i8` | `overflowing_literals` (deny) x2 | typeck (Sabiko's E0314) |
| `binding_named_like_variant.sabi` | bare `Red =>` and `Green =>` bind | E0170 x2 (deny lint) + unreachable warning | resolve / typeck |
| `struct_literal_in_scrutinee.sabi` | `match P { X: 1 } { .. }`, `match Named::Only { Id: 2 } { .. }` | syntax error x2 | parse (Sabiko's E0108) |
| `enum_definition_errors.sabi` | duplicate variant, enum named like a struct, duplicate struct-variant field | E0428 x2, E0124 | resolve (E0200/E0204) |
| `recursive_enum.sabi` | `Cons(i32, List)`; `Node`/`Link` through a struct | E0072 x2 | typeck (Sabiko's E0206) |
| `enum_casts.sabi` | an enum with a payload `as i32`, `as f64`, `1 as Light`, `as bool` | E0605 x2, E0606, E0054 | typeck (E0308) |
| `discriminant_errors.sabi` | duplicate explicit, explicit equal to implicit, overflow after `i64::MAX`, a discriminant with a payload variant | E0081 x2, E0370, E0732 | typeck / const eval |
| `derive_copy_enum_with_non_copy_payload.sabi` | `#[derive(Copy, Clone)]` with a non-Copy payload | E0204, E0277 | borrowck (Sabiko's E0417) |
| `guard_assigns_scrutinee.sabi` | `Cell::Value(_) if { c = ..; false }`, `if { Clear(&mut c); false }` | E0510 x2 | borrowck |
| `move_out_of_drop_enum.sabi` | (drops) moving a payload out of an enum with its own `Drop` | E0509 | borrowck |
| `if_let/let_else_not_diverging.sabi` | a `let else` block that finishes | E0308 | typeck |
| `if_let/irrefutable_let_patterns.sabi` | irrefutable `if let`, `let else`, `while let x = k` | 3 warnings, accepted | exhaustiveness (`//~ WARN`) |
| `if_let/let_chains.sabi` | `if let .. = c && n > 0` | syntax error (edition 2024 only) | parse |

### Known gaps (`enums/known_gap/`)

All five run under rustc (`.expected` notes); none is required by #8.

| file | gap | rustc |
| --- | --- | --- |
| `primitive_consts_in_patterns.sabi` | `i8::MIN..=-1`, `1..=i8::MAX`, `i32::MIN` as patterns: Sabiko has no associated constants on primitives (E0212 in expressions too) | exit 0 |
| `reference_patterns.sabi` | `&p` patterns (`&Cell::Value(n)`, `let &(a, b) = &t;`), not listed in #8 (E0107 today, SPEC §2) | exit 0 |
| `half_open_range_patterns.sabi` | `..=-1`, `1..` (exhaustive for `i32`), optional | exit 0 |
| `enum_equality.sabi` | `#[derive(PartialEq)]` and `==` on enums: Phase 6 comparison traits | exit 0 |
| `match_through_double_ref.sabi` | `match rr` for `rr: &&Cell`: code generation rejects `&&T` (E0500) until `Ref<T>` | exit 0 |

### Rules rustc applies (each confirmed by the program named)

1. **Binding modes.** A by-value binding copies a Copy payload and moves a non-Copy one
   out of the scrutinee (`ok_match_copy_payload_binding`, `err_match_binding_moves_payload`);
   `ref`/`ref mut` borrow it, `_` and `..` touch nothing (`ok_match_ref_binding_does_not_move`).
   Matching a *reference* with a non-reference pattern dereferences it and switches the
   default binding mode to `ref` (or `ref mut` for `&mut`, unless a `&` was crossed), so
   `match r { Slot::Full(t) => .. }` binds `t: &Token` and `n: &i32`
   (`ok_match_default_binding_through_ref`, `binding_modes`), while `match *r` binds by
   value and moving out is E0507 (`err_match_by_value_through_shared_ref`). Rust 2021:
   `mut t` under a `ref` default mode binds by value again (E0507,
   `err_match_mut_binding_through_ref_moves`). `x @ p` binds the whole value like `x`
   (`err_match_at_binding_moves_whole`).
2. **Matching reads the discriminant.** Any arm with a variant or literal pattern reads
   the scrutinee: it is a use for move checking (matching a partially moved enum is E0382,
   `err_match_partial_move_then_match_again`, unlike a struct, whose other fields stay
   usable), for loans (E0503 while `&mut` borrowed, `err_match_scrutinee_while_mut_borrowed`)
   and for initialisation (E0381, `err_match_uninit_scrutinee`). A match with a lone `_`
   arm, or with a variant pattern of a single-variant enum binding nothing, reads nothing
   (`ok_match_wildcard_while_mut_borrowed`).
3. **Guards and fake borrows.** A guard runs only when its pattern matched, top to bottom,
   and a false guard falls through (`arm_order_and_guards`, `evaluation_order`,
   `panic_in_guard`). While a guard runs the scrutinee is frozen: assigning it or
   borrowing it `&mut` is E0510 (`guard_assigns_scrutinee`); with a binding in the arm,
   the binding itself is a shared borrow during the guard, so rustc reports E0506
   (`err_match_guard_binding_blocks_assignment`). Bindings, even `mut n` and `ref mut n`,
   are immutable until the end of the guard (E0594 "immutable for the pattern guard",
   `err_match_guard_assigns_mut_binding`, `err_match_guard_writes_through_ref_mut`). The
   guard may read anything and write other locals; the arm body may then change the
   scrutinee (`ok_match_guard_reads_scrutinee`). An or-pattern's guard is evaluated once
   per alternative that matches, in order (`guard 5` twice in `arm_order_and_guards`).
   Lowering a guard: bind by reference first, evaluate the guard, then bind by value
   (the by-value move happens only once the guard succeeded,
   `ok_match_move_in_diverging_arm`).
4. **Or-patterns.** Every alternative binds the same names (E0408), with the same types
   (E0308) and the same binding modes (E0409) (`or_pattern_binding_*`). In `let` a
   top-level or-pattern needs parentheses (`or_pattern_in_let_needs_parens`).
5. **Exhaustiveness and reachability** (rustc's usefulness algorithm). Guarded arms never
   count (`non_exhaustive_guard`). Integers are exhaustive when the ranges cover the type:
   `u8` with `0..=255` pieces and `i8` with `-128..=-1 | 0..=127` need no `_`
   (`literals_and_ranges`); otherwise E0004 lists the gaps as ranges with suffixed
   literals and `i32::MIN`/`u8::MAX` names (`non_exhaustive_integer`). `bool` and tuples
   of `bool` need every combination (`non_exhaustive_bool_char`). `char` ranges cover
   Unicode scalar values: surrogates are not values, and the BMP alone is not exhaustive
   (`'𐀀'..='\u{10ffff}'` remains) although a Sabiko `char` cannot hold those values
   (decision 31): Sabiko must report E0004 there as rustc does. An empty enum is
   exhaustively matched by `match n {}`, which has type `!` (`empty_enum`); any other type
   is not (`match_without_arms_on_inhabited_type`). Unreachable arms, including single
   alternatives of an or-pattern and ranges covered by several earlier ranges together,
   are *warnings* (`unreachable_arms`), and `let`/parameter patterns must be irrefutable
   (E0005).
6. **Evaluation order.** The scrutinee is evaluated once, before any test, a tuple
   scrutinee left to right; then guards in arm order; then the body
   (`evaluation_order`).
7. **Paths vs bindings.** A variant is always a path (`Light::Red`, `Self::Red`); a bare
   identifier binds, unless it names a constant or a unit struct, which compare
   (`ident_patterns`). A bare binding named like a variant of the scrutinee's enum is the
   deny-by-default E0170 (`binding_named_like_variant`). A struct literal as a scrutinee
   needs parentheses (`struct_literal_in_scrutinee`, `single_variant`).
8. **Discriminants and casts.** Fieldless enums cast to any integer type with `as`, by
   value: implicit discriminants start at 0 and continue from the previous one, explicit
   ones may be negative, out of order and up to `isize`'s range, and casts wrap like
   integer casts (`discriminants`). Duplicates (E0081) and overflow of the implicit
   increment (E0370) are errors; a discriminant on an enum with a payload variant needs
   `#[repr(inttype)]` (E0732), which Sabiko does not interpret; casting an enum with a
   payload, to a float or `bool`, or to an enum, is an error (`enum_casts`).
9. **Drop interaction** (drops, #12). A by-value binding moves the payload into the arm,
   where it is dropped at the arm's end; what the pattern does not bind stays in the
   scrutinee and is dropped with it, so the scrutinee needs per-field drop flags through
   the variant (`drops_in_match`). A temporary scrutinee lives to the end of the `match`
   statement, after the arm; in a `let x = match temp { .. }` the rest is dropped at the
   end of the `let`. Edition 2021: an `if let` scrutinee's temporaries live until after
   the `else` block, a `while let`'s are dropped every iteration
   (`if_let/scrutinee_temporaries`). An enum with its own `Drop` cannot be moved out of
   (E0509, `move_out_of_drop_enum`).

### Notes for the implementer

- **E0510's bucket.** rustc reports E0510 from its borrow checker, but it is not in
  `RUSTC_BORROW_CODES`, so `guard_assigns_scrutinee.sabi` is a UI program. Adding E0510
  to the list with a Sabiko borrowck code (E0419, if #12 takes E0418 for E0509) lets its
  two cases become `differential/loans/err_*` programs. Either way the E0506 and E0594
  guard programs are already differential.
- **Warnings in UI tests.** `unreachable_arms.sabi` and `if_let/irrefutable_let_patterns.sabi`
  compile with warnings only; convert their annotations to `//~ WARN` (the UI harness
  supports it). The differential harnesses pass `-A warnings` and the `unused` allow
  (which covers `unreachable_patterns`) to rustc, and a Sabiko warning on stderr does
  not fail a run program, so `arm_order_and_guards.sabi` may keep its unreachable `5`.
  Sabiko needs warning codes for unreachable patterns and irrefutable `if let`/`while
  let`/`let else` (or one code with rustc's messages).
- **Explicit discriminants.** Decision 15's tag must carry the declared discriminant (or
  map to it) so that `as` gives rustc's values; values span `isize`
  (`C { X = 9223372036854775807 }` is valid, only its successor overflows), so the tag of
  a fieldless enum with explicit values is best an `int64`, or the smallest type that
  holds them, as rustc's layout picks. Discriminants on enums with payloads stay E0732
  until `#[repr]` is interpreted. `discriminants.sabi` pins the values.
- **`if let`/`while let`/`let else`**: see the recommendation above. Let chains are edition
  2024 only and rejected (`if_let/let_chains.sabi`).
- **`char` exhaustiveness** must use rustc's scalar-value ranges (0..=0xD7FF and
  0xE000..=0x10FFFF), not Sabiko's UTF-16 storage, or Sabiko accepts invalid Rust
  (`non_exhaustive_bool_char`, function `H`). The gap names in E0004 messages will then
  read like rustc's.
- **Panic locations** inside arms are the `Panic` call's or the operator's (11:13, 8:37),
  inside a guard the guard expression's operator (8:27).
- **Literals as patterns** have the scrutinee's type: `128` for an `i8` is the deny lint
  `overflowing_literals` (Sabiko's E0314), `'a'` against `i32` is E0308.
