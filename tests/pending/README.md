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

## `drops/`: issues #12 (drop elaboration) and #37 (`let _ = place;`) (landed)

The staged programs were checked with rustc 1.99.1 (58cf8f9c3 2026-10-13) as the harnesses
prepare them (the compile-time ones with the prelude of
`crates/sabiko_driver/tests/differential.rs`, the run-time ones with that of
`crates/sabikoc/tests/differential_run.rs`, the UI ones annotated `//~ rustc E0xxx`), and
moved into their harnesses by #37 and #12 (decision 55). All of them agree with rustc; none
needed a correction. The drop rules they pin are in `docs/SPEC.md` (Drop).

| staged | now |
| --- | --- |
| `differential/{loans,moves,mutability}/*.sabi` (26 programs) | `tests/differential/{loans,moves,mutability}/`, unchanged |
| `differential/run/drops/*.sabi` (18 programs) | `tests/differential/run/drops/`; the `.expected` notes are deleted |
| `known_gap/let_underscore_place.sabi` | `tests/differential/run/drops/` (#37: `let _ = place;` is a `PlaceMention`) |
| `known_gap/panic_in_drop_while_unwinding.sabi` | `tests/differential/run/drops/`: Sabiko aborts as rustc does, and the run harness now compares aborts (exit 134 on every platform, first panic report) |
| `ui/move_out_of_drop_type.sabi` (E0509 x3) | three `tests/differential/moves/err_drop_move_field_out_of_drop_*` programs: E0509 is in `RUSTC_BORROW_CODES`, Sabiko's E0418 |
| `ui/drop_for_non_struct`, `drop_for_unknown_type`, `drop_missing_or_extra_items`, `duplicate_drop_impl` | `tests/ui/resolve/` (E0214, E0201, E0216/E0217, E0215) |
| `ui/derive_copy_on_drop_type`, `copy_struct_with_drop_field`, `drop_wrong_signature`, `explicit_drop_call` | `tests/ui/typeck/` (E0324, E0417, E0321/E0322, E0323) |

Each converted UI program names the rustc codes it was checked against in a comment. #12
added `ui/resolve/drop_shadowed_trait` (rustc E0404, Sabiko E0203), a receiverless
`fn drop()` to `drop_wrong_signature` (E0186, Sabiko E0321) and
`run/drops/inherent_drop_method`. No known gap is left.

## `enums/`: issue #8, enums, `match`, patterns, exhaustiveness

**Landed (#8, decisions 57 and 58).** Every staged program moved into its harness and
agrees with rustc: the compile-time pairs to `tests/differential/{moves,loans,mutability}/`
(the `if_let/` ones to `if_let/` subdirectories), the run-time programs to
`tests/differential/run/enums/` (with `if_let/`; the `.expected` notes are deleted), and
the UI programs, their annotations converted to Sabiko's codes (a first line names the
rustc code each stands for), to `tests/ui/{parse,resolve,typeck,borrowck}/`. E0510 joined
the differential harness's rustc borrow codes and E0419 its Sabiko ones, as suggested
below. Two UI programs differ from rustc in position only: `move_out_of_drop_enum` reports
E0418 at the binding that moves rather than at the scrutinee, and rustc's E0509 note is
kept as a comment. `known_gap/` stays: none of its five programs is supported yet. The
text below is the staging record.

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

All but one run under rustc (`.expected` notes); none is required by #8.

| file | gap | rustc |
| --- | --- | --- |
| `primitive_consts_in_patterns.sabi` | `i8::MIN..=-1`, `1..=i8::MAX`, `i32::MIN` as patterns: Sabiko has no associated constants on primitives (E0212 in expressions too) | exit 0 |
| `reference_patterns.sabi` | `&p` patterns (`&Cell::Value(n)`, `let &(a, b) = &t;`), not listed in #8 (E0107 today, SPEC §2) | exit 0 |
| `half_open_range_patterns.sabi` | `..=-1`, `1..` (exhaustive for `i32`), optional | exit 0 |
| `enum_equality.sabi` | `#[derive(PartialEq)]` and `==` on enums: Phase 6 comparison traits | exit 0 |
| `match_through_double_ref.sabi` | `match rr` for `rr: &&Cell`: code generation rejects `&&T` (E0500) until `Ref<T>` | exit 0 |
| `unreachable_arms_borrowck.sabi` | rustc borrow-checks code that patterns make unreachable (false edges between match candidates; the `else` of an irrefutable `if let`/`let`-`else`), Sabiko does not: assignments to immutable bindings and uses after move there are E0384/E0382 in rustc and accepted by Sabiko; found by the generator, which makes no pattern unreachable | rejected (E0384, E0382) |

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

## `arrays/`: issue #9, arrays `[T; N]`, slices `&[T]`/`&mut [T]`, indexing, `.len()`

Every program was checked with rustc 1.99.1 (58cf8f9c3 2026-10-13), the version
`rust-toolchain.toml` pins, prepared exactly as the harnesses prepare it:

- compile-time programs (`differential/{loans,moves,mutability}`): the five-line prelude of
  `crates/sabiko_driver/tests/differential.rs` prepended, `rustc --edition 2021
  --crate-type lib --emit=metadata -A warnings`; `ok_*` are accepted, and `err_*` are
  rejected with borrow-family codes only, **except the five `moves/err_move_*` programs
  rejected with E0508**, which is not in `RUSTC_BORROW_CODES` yet (see the notes);
- run-time programs (`differential/run/arrays`): the prelude of
  `crates/sabikoc/tests/differential_run.rs` appended (line numbers are the file's), its
  lints as `-A` flags, `--crate-type bin -C overflow-checks=on` (`off` for `//@ release`),
  run with `RUST_BACKTRACE=0`. Each has a sibling `.expected` note with rustc's stdout,
  exit code and panic report, for convenience only (delete them when moving the programs);
- UI programs (`ui/`): the builtins appended (no `main`, so line numbers are the file's),
  the prelude's lints as `-A` flags, `-A warnings`, `--crate-type lib` **with code
  generation** (not `--emit=metadata`: the `unconditional_panic` lint runs only then).
  Each rustc error line carries `//~ rustc E0xxx: <primary message>` (`-` for an error
  without a code), sometimes followed by `[<primary label>]`. Every rustc error is
  annotated and every annotation was reported at that line; a second run without
  `-A warnings` (keeping the prelude's lint allows) found no warnings.

The programs use what SPEC.md describes, tuples (#6), methods (#7) and, in the four
`run/arrays/drops*.sabi` programs, `impl Drop` (#12). They use no enums (#8; the one
program that needs them is a known gap), no generics, no `Option` (`.get(i)` moved to
Phase 6, #13), no `for` and no ranges: loops are `while i < s.len()`. Lengths are literals,
`const` items or constant arithmetic on them. A constant out-of-bounds index is a compile
error in rustc (rule 12), so every run program that panics on an index gets the index from
a parameter.

### Compile-time verdict pairs (`arrays/differential/`)

| file | tests | rustc |
| --- | --- | --- |
| `loans/err_index_mut_borrows_conflict.sabi` | `let first = &mut a[0]; let second = &mut a[1]; *first += *second;` | E0499 |
| `loans/ok_index_mut_borrows_sequential.sabi` | the same borrows one after the other; `Bump(&mut a[2])` twice | accept |
| `loans/err_index_borrow_then_assign_other_element.sabi` | `let r = &a[0]; a[1] = 5; *r` | E0506 |
| `loans/ok_index_borrow_then_read_other_element.sabi` | reads, shared borrows and `a.len()` while `&a[0]` is live; a write after it | accept |
| `loans/err_index_mut_borrow_then_read_other_element.sabi` | `let r = &mut a[0]; let x = a[1]; *r = x;` | E0503 |
| `loans/err_index_mut_borrow_then_len.sabi` | `a.len()` (an autoref'd `&a`) while `&mut a[0]` is live | E0502 |
| `loans/err_len_while_element_mut_borrowed_in_compound.sabi` | `*second += a.len() as i32` for `second = &mut a[1]`: the right operand runs while `second` is live | E0502 |
| `loans/ok_index_disjoint_fields_of_elements.sabi` | `&mut a[i].X` and `&mut a[j].Y` at once, `i`, `j` unknown | accept |
| `loans/err_index_same_field_of_elements.sabi` | `&mut a[i].X` and `&mut a[j].X` | E0499 |
| `loans/ok_array_fields_of_struct_disjoint.sabi` | `&mut g.Left[0]`, `&mut g.Right[1]` and `g.Count += 1` at once | accept |
| `loans/err_slice_borrow_then_assign_element.sabi` | `let s: &[i32] = &a; a[0] = 10; s.len()` | E0506 |
| `loans/err_mut_slice_then_read_array.sabi` | `let s: &mut [i32] = &mut a; let x = a[0]; s[1] = x;` | E0503 |
| `loans/ok_mut_slice_index_reads_and_writes.sabi` | `s[s.len() - 1] += s[0]`, `s[0] = s[1] * s[2]`, `Bump(&mut s[1])`, `let t: &[i32] = s;`, then `a` | accept |
| `loans/err_element_borrow_outlives_array.sabi` | `r = &a[1]` for a block-local `a`, `*r` after the block | E0597 |
| `loans/err_return_ref_to_local_array_element.sabi` | `fn Pick(n: &i32) -> &i32 { let a = [..]; &a[1] }` | E0515 |
| `loans/ok_return_element_ref_from_slice.sabi` | `-> &i32` from `&[i32]` (`&s[best]`), `-> &mut i32` from `&mut [i32]`, `*LastMut(&mut a) += 10` | accept |
| `loans/err_returned_element_ref_blocks_assignment.sabi` | `let m = Largest(&a); a[2] = 0; *m` | E0506 |
| `loans/err_nested_row_borrow_then_assign.sabi` | `let row = &mut m[0]; m[1][0] = 5; row[0] = 6;` | E0506 |
| `loans/ok_index_with_index_from_same_array.sabi` | `a[a[0] as usize] = a[1]`, `a[a.len() - 1] += 1`, `a[a[2] as usize % 3] *= a[a.len() - 3]`, `m[m[0][0]][m[1][1]] = 5` | accept |
| `loans/err_element_method_while_element_borrowed.sabi` | `let r = &a[0]; a[1].Inc(); r.N` (`&mut self` autoref of `a[1]`) | E0502 |
| `moves/err_move_out_of_index.sabi` | `let t = a[0];` for a non-Copy element | E0508 (not yet a borrow code) |
| `moves/ok_copy_out_of_index.sabi` | Copy elements (a Copy struct, a tuple) copied out; a Copy field of a non-Copy element read | accept |
| `moves/err_move_field_out_of_index.sabi` | `let inner = a[0].Inner;` | E0508 |
| `moves/err_move_out_of_slice_index.sabi` | `s[0]` returned from `&[Token]`, `s[s.len() - 1]` from `&mut [Token]` | E0508 x2 ("a non-copy slice") |
| `moves/err_move_out_of_index_through_array_ref.sabi` | `r[1]` returned from `r: &[Token; 2]` | E0508 (not E0507) |
| `moves/err_move_out_of_nested_index.sabi` | `let row = m[1];` for `[[Token; 1]; 2]` | E0508 |
| `moves/err_array_moved_then_indexed.sabi` | `let b = a;` then `a[0].Id` | E0382 |
| `moves/ok_array_of_copy_is_copy.sabi` | `[i32; 3]`, `[P; 2]` (Copy `P`) and `[[u8; 2]; 2]` copied by `let` and by value arguments | accept |
| `moves/err_array_of_non_copy_passed_twice.sabi` | `Take(a); Take(a);` | E0382 |
| `moves/ok_array_move_then_reassign.sabi` | moved, a whole array assigned, moved again per loop iteration | accept |
| `moves/err_assign_element_of_moved_array.sabi` | `let b = a; a[0] = Token { .. };` | E0382 ("use of moved value") |
| `moves/err_array_moved_in_loop.sabi` | `Take(a)` in a `while` body | E0382 |
| `moves/err_array_literal_moves_elements.sabi` | `let a = [t, u];` then `t.Id` | E0382 |
| `moves/ok_move_array_field_out_of_struct.sabi` | `let items = bag.Items;`, `bag.Count` still usable, `bag.Items = [..]` makes it whole | accept |
| `moves/ok_repeat_zero_or_one_needs_no_copy.sabi` | `[Token { .. }; 1]`, `[Token { .. }; 0]`, `[t; ONE]` without Copy | accept |
| `moves/err_repeat_moves_operand.sabi` | `let none = [t; 0];` then `t.Id` | E0382 |
| `mutability/err_assign_element_immutable.sabi` | `a[0] = 4` without `mut` | E0594 |
| `mutability/ok_assign_element_mut.sabi` | `a[0] =`, `a[1] +=`, `ps[1].X *=`, `m[1][2] =`, `g.Cells[1] =`, `&mut a[2]` | accept |
| `mutability/err_mut_borrow_element_immutable.sabi` | `Bump(&mut a[0])` without `mut` | E0596 |
| `mutability/err_assign_through_shared_slice.sabi` | `s[0] = 0; s[1] += 1;` for `s: &[i32]` | E0594 x2 |
| `mutability/ok_assign_through_mut_slice.sabi` | writes and `&mut s[2]` through `s: &mut [i32]` (not a `mut` binding) and `r: &mut [i32; 3]`, `Clear(r)` | accept |
| `mutability/err_mut_slice_from_immutable_array.sabi` | `Clear(&mut a)` (to `&mut [i32]`) without `mut` | E0596 |
| `mutability/err_assign_element_field_immutable.sabi` | `a[0].X = 2` without `mut` | E0594 |
| `mutability/err_compound_assign_element_of_param.sabi` | `a[0] += 1` for a by-value parameter `a: [i32; 3]` | E0594 |
| `mutability/ok_mut_array_param.sabi` | `mut a: [i32; 3]` changed and returned; the caller's copy unchanged | accept |
| `mutability/err_assign_element_uninit.sabi` | `let mut a: [i32; 2]; a[0] = 1;` | E0381 ("used binding `a` isn't initialized") |
| `mutability/ok_deferred_init_whole_array.sabi` | `let a: [i32; 2];` assigned whole on both paths, no `mut` | accept |
| `mutability/err_len_of_uninit_array.sabi` | `a.len()` of a declared, unassigned array | E0381 |
| `mutability/err_assign_array_field_through_shared_ref.sabi` | `g.Cells[0] = 0` for `g: &Grid` | E0594 |
| `mutability/err_element_mut_method_through_shared_slice.sabi` | `s[0].Inc()` (`&mut self`) for `s: &[Counter]` | E0596 |

### Run-time programs (`arrays/differential/run/arrays/`)

| file | tests | rustc |
| --- | --- | --- |
| `basics.sabi` | `[a, b, c]`, `[7; 4]`, `[0u8; 5]`, reads, writes, `+=`/`-=`, `.len()`, `usize` index arithmetic, `c[c[2] as usize]`, `a[a.len() - 1]` | exit 0 |
| `slices.sabi` | `&[i32]`/`&mut [i32]` parameters; coercions at arguments, annotated `let`s, a return (`fn AsSlice(a: &[i32; 4]) -> &[i32] { a }`), `&mut [i32]` to `&[i32]`; in-place reverse; slices of two lengths through one function | exit 0 |
| `copy_semantics.sabi` | `let b = a` copies; a `mut` by-value parameter returned; a Copy struct with an array field; a row copied out of a nested array and assigned back | exit 0 |
| `nested_arrays.sabi` | `[[0; 3]; 2]`, `m[i][j]` reads/writes, a row as `&[i32]` and `&mut [i32]`, inner and outer `.len()`, a transpose, `[[[0u8; 2]; 2]; 2]` | exit 0 |
| `arrays_of_structs.sabi` | `cs[1].N += 5`, `&mut self`/`&self` methods on elements, `Reset(&mut cs[0])`, `self.Cells[i]` in methods, an array of structs with array fields | exit 0 |
| `arrays_of_tuples.sabi` | `pairs[1].0 += 40`, `pairs[2].1 = false`, `let (n, flag) = pairs[1]`, `t.0[1] = 60`, `([i32; 2], bool)` returned, `Split(3).0[1]` | exit 0 |
| `evaluation_order.sabi` | `a[Idx(0)] = Val(10)` and `a[Idx(1)] += Val(5)` run `Val` first; `m[Idx(1)][Idx(0)] = Val(7)` runs `Val`, then the indices outside in; reads go base, index, left to right; `Arr(3)[Idx(2)]`; literal elements left to right; `[Val(4); 3]` calls `Val` once | exit 0 |
| `const_lengths.sabi` | `const N: usize = 4`, `M = N * 2 + 1`, `[FILL; M]`, `[i32; N]` parameters, `&[i64; 3 * 2]`, `a[N - 1]`, `[[N; N]; 2]` | exit 0 |
| `element_types.sabi` | arrays of `u8` `i16` `u32` `i64` `u64` `i128` `u128` `isize` `usize` `f32` `f64` `bool` `char`, extremes printed | exit 0 |
| `zero_length.sabi` | `let a: [i32; 0] = [];`, `[7; 0]`, a `[i32; 0]` parameter, `&[]` as `&[i32]`, a loop over an empty slice, `Count(&[1, 2, 3])`, `[(); 3].len()` | exit 0 |
| `algorithms.sabi` | insertion sort over `&mut [i32]`, binary search over `&[i32]`, a sieve over `[bool; 100]`, a prefix sum: the `while i < s.len()` loops of the JIT note | exit 0 |
| `drops.sabi` | (drops) elements dropped 0 to N-1 at scope end; nested arrays row by row; an array field in field order; `a[Idx(1)] = Make(6)`: make, index, drop the old; an array moved into a function and one returned; `[Make(19); 0]` drops at once, `[Make(20); 1]` at scope end | exit 0 |
| `drops_unwind_index.sabi` | (drops) an out-of-bounds read with two arrays of `D` live: the later array first, each first to last | exit 101, `index out of bounds: the len is 3 but the index is 3` at 14:5 |
| `drops_unwind_assign.sabi` | (drops) `a[i] = Make(4)` out of bounds: made, not stored, dropped first while unwinding, then the elements | exit 101, `.. the len is 2 but the index is 2` at 21:5 |
| `drops_unwind_literal.sabi` | (drops) a panic in the third element of `[Make(2), Make(3), Make(-4), Make(5)]`: 3 and 2 dropped (last made first), then the earlier local | exit 101, `negative id` at 15:9 |
| `index_out_of_bounds.sabi` | `a[i]` with `i = 3` for a `[i32; 3]` by value | exit 101, `index out of bounds: the len is 3 but the index is 3` at 6:5 |
| `index_out_of_bounds_write.sabi` | `a[i] = Val(..)` with `i = 2`: `value 20` printed, then the panic | exit 101, `.. the len is 2 but the index is 2` at 10:5 |
| `index_out_of_bounds_empty_slice.sabi` | `s[i] += 1` on an empty `&mut [i32]` | exit 101, `.. the len is 0 but the index is 0` at 4:5 |
| `index_out_of_bounds_nested.sabi` | `m[1][2]` for `&[[i32; 2]; 3]`: the inner length, the outer expression's location | exit 101, `.. the len is 2 but the index is 2` at 4:13 |
| `index_out_of_bounds_huge.sabi` | index 4294967296 (2^32) on a slice of 3: must not wrap to 0 | exit 101, `.. the len is 3 but the index is 4294967296` at 5:5 |
| `index_negative_cast.sabi` | `a[i as usize]` with `i = -1` | exit 101, `.. the len is 3 but the index is 18446744073709551615` at 4:5 |
| `index_arith_overflow.sabi` | `a[i - 1]` with `i = 0usize` | exit 101, `attempt to subtract with overflow` at 4:7 |
| `release_index_wraps.sabi` | `//@ release`: `i8` elements wrap (`100 + 100` = -56, `127 + 1` = -128), `i - 1` wraps, the bounds check stays | exit 101, `.. the len is 4 but the index is 18446744073709551615` at 6:5 |
| `panic_in_index_expression.sabi` | `a[Check(3, a.len())] = 40` where `Check` panics: nothing written | exit 101, `index too large` at 5:9 |

### UI programs (`arrays/ui/`), rejected by rustc with non-borrow errors

| file | tests | rustc | stage |
| --- | --- | --- | --- |
| `length_mismatch.sabi` | a return value, an annotated `let`, an argument, an assignment of another length | E0308 x4 ("expected an array with a size of 3, found one with a size of 2") | typeck |
| `element_type_mismatch.sabi` | `[1, true]`, `c[0] = 1.5`, `c[1] = 2i64`, `[1u8, 2u16]` | E0308 x4 | typeck |
| `element_literal_out_of_range.sabi` | `[1, 300]` as `[u8; 2]`, `[128]` as `[i8; 1]` | `overflowing_literals` (deny) x2 | typeck (Sabiko's E0314) |
| `index_type_not_usize.sabi` | `i32`, `u32`, `u64` and `bool` indices; `a[-1]` | E0277 x4 ("the type `[i32]` cannot be indexed by `i32`"), an uncoded error | typeck |
| `index_non_indexable.sabi` | `x[0]` for `i32`, a struct, a tuple; `"abc"[0]` | E0608 x3, E0277 | typeck |
| `len_on_wrong_type.sabi` | `.len()` on `i32` and a struct, `a.Len()`, `a.len(1)`, `a.len` | E0599 x3, E0061, E0615 | typeck (E0319, E0304, E0320) |
| `repeat_needs_copy.sabi` | `[t; 2]`, `[Make(); TWO]`, `[Token { .. }; 3]`, one function each | E0277 x3 ("the trait bound `Token: Copy` is not satisfied") | typeck or borrowck |
| `repeat_length_not_const.sabi` | `[0; n]`, `[i32; n]`, `[true; len]` | E0435 x3 | resolve or const eval |
| `repeat_length_type.sabi` | `[0; 3i32]`, `[i32; 2u8]`, `[0; LEN]` with `LEN: i32`, `[0; -1]` | E0308 x3, E0600 | typeck / const eval |
| `unconditional_panic.sabi` | constant out-of-bounds indices: a literal, a `const`, a write, an empty array, a local constant, a `&[i32; 3]` parameter; a slice is not linted | `unconditional_panic` (deny) x6, only with code generation | after MIR building |
| `print_array.sabi` | `PrintLine(a)`, `Print(s)` | E0277 x2 ("doesn't implement `std::fmt::Display`") | typeck |
| `unsized_slice_by_value.sabi` | a `[i32]` parameter, `let v = *r;` | E0277 x2 ("the size for values of type `[i32]` cannot be known at compilation time") | typeck |
| `no_slice_to_array_coercion.sabi` | `&[i32]` to `&[i32; 3]` (twice), `&[i32; 3]` to `&mut [i32]`, to `&[i64]`, to `[i32; 3]` | E0308 x5 | typeck |
| `array_operators.sabi` | `a + b`, `-a`, `a as i32`, `a[0] + b` | E0369, E0600, E0605, E0277 | typeck (E0306, E0307, E0308) |
| `derive_copy_with_non_copy_array_field.sabi` | `#[derive(Copy, Clone)]` on a struct with a `[Token; 2]` field | E0204, E0277 | borrowck (Sabiko's E0417) |

### Known gaps (`arrays/known_gap/`)

All eight run under rustc (`.expected` notes); none is required by #9.

| file | gap | rustc |
| --- | --- | --- |
| `range_indexing.sabi` | `&a[1..3]`, `&a[..2]`, `&a[3..]`, `a[..]`, `&mut a[1..]`, a slice of a slice: ranges are not expressions yet | exit 0 |
| `slice_patterns.sabi` | `let [a, b, c] = arr;`, `let [x, ..] = arr;`, `[first, .., last]` parameters (SPEC §2 patterns) | exit 0 |
| `array_equality.sabi` | `==`, `!=`, `<` on arrays and slices, array against slice: Phase 6 comparison traits | exit 0 |
| `const_array_items.sabi` | `const PRIMES: [i32; 4]`, `const SECOND: i32 = PRIMES[1]`: aggregates in `const` are E0315 today | exit 0 |
| `for_loops.sabi` | `for x in a`, `in &a`, `in s`, `in 0..a.len()`: no `for` until iterators | exit 0 |
| `arrays_of_references.sabi` | `[&x, &y]`, `["ada", "grace"]`, `[&mut p, &mut q]`: an inline array cannot hold byrefs or `ReadOnlySpan<char>`; a ref-struct representation (like decision 52's tuples) or E0500, to decide in #9 | exit 0 |
| `slice_methods.sabi` | `.is_empty()`, `.swap`, `.reverse`, `.contains`, `.split_at_mut`, `.fill`: only `.len()` is in #9 | exit 0 |
| `arrays_of_enums.sabi` | `[Light::Red; 3]`, `match lights[i]`, `match &slots[1]`: needs #8, then belongs in `run/arrays/` | exit 0 |

### Rules rustc applies (each confirmed by the program named)

1. **Lengths are constant `usize` expressions.** A literal, a `const` item or arithmetic on
   them, in types and in repeat expressions alike (`const_lengths`); a local or parameter
   is E0435 (`repeat_length_not_const`), another integer type E0308, `-1` E0600
   (`repeat_length_type`). The length is part of the type: nothing converts between
   lengths (E0308, `length_mismatch`).
2. **Repeat expressions.** `[x; N]` evaluates `x` once (`[Val(4); 3]` prints once,
   `evaluation_order`). For N >= 2 `x` must be Copy (E0277, reported once per function
   body, `repeat_needs_copy`); for N = 1 it is moved into the array, for N = 0 it is moved
   and dropped on the spot (`ok_repeat_zero_or_one_needs_no_copy`, `drops`: `make 19`,
   `drop 19`, then `after [x; 0]`), and in both cases `x` is moved
   (`err_repeat_moves_operand`).
3. **Copy-ness.** `[T; N]` is Copy exactly when `T` is: copies by `let`, argument and
   return leave the original unchanged (`ok_array_of_copy_is_copy`, `copy_semantics`), a
   `#[derive(Copy)]` struct may hold one, and not with a non-Copy element (E0204,
   `derive_copy_with_non_copy_array_field`). Otherwise the whole array moves (E0382 rules,
   `err_array_*`), an array literal moves its elements (`err_array_literal_moves_elements`),
   and the array is one unit for initialisation: assigning an element of a moved or
   never-initialised array is E0382/E0381, only a whole array revives it
   (`err_assign_element_of_moved_array`, `err_assign_element_uninit`,
   `ok_array_move_then_reassign`). `.len()` reads the array (E0381 uninitialised,
   `err_len_of_uninit_array`).
4. **Index type.** Only `usize` indexes (E0277 for any other type, `index_type_not_usize`);
   an unsuffixed literal index is a `usize`, so `a[-1]` is an uncoded error. A negative
   value cast with `as usize` wraps to a huge index (`index_negative_cast`).
5. **Bounds check and panic.** `a[i]` with `i >= len` panics with "index out of bounds:
   the len is {len} but the index is {i}", the index printed as the full 64-bit value
   (`index_out_of_bounds_huge`, `index_negative_cast`), located at the start of the indexing
   expression; for `m[i][j]` that is the start of `m`, with the inner array's length
   (`index_out_of_bounds_nested`). The check stays in release builds, where index
   arithmetic wraps first (`release_index_wraps`); in debug builds overflow in the index
   panics before indexing (`index_arith_overflow`), and so does a panic inside the index
   expression (`panic_in_index_expression`).
6. **Evaluation order.** A read evaluates the base, then the index, operands left to
   right, nested indices outside in. In `a[i] = v` and `a[i] op= v` (primitive operands)
   the right operand `v` is evaluated *first*, then the index, then the bounds check, then
   the store (`evaluation_order`, `index_out_of_bounds_write`). With drop glue, the old
   element is dropped after the new value is made and the index checked (`drops`:
   `make 6`, `index 1`, `drop 5`), and an assignment that fails the check stores nothing
   (`drops_unwind_assign`).
7. **Indexing gives no disjoint borrows.** Any two index projections of one array may
   overlap, constant indices included: `&mut a[0]` and `&mut a[1]` are E0499, a write to
   `a[1]` while `&a[0]` is live E0506, a read of `a[1]` while `&mut a[0]` is live E0503,
   and `a.len()` (a borrow of the whole array) conflicts with an element's `&mut`
   (`loans/err_index_*`, `err_nested_row_borrow_then_assign`). The projections *after*
   the index are still compared: `&mut a[i].X` and `&mut a[j].Y` are accepted,
   `&mut a[i].X` and `&mut a[j].X` are not (`ok_index_disjoint_fields_of_elements`,
   `err_index_same_field_of_elements`); arrays in different struct fields are disjoint
   (`ok_array_fields_of_struct_disjoint`). Built-in indexing needs no two-phase borrow:
   `a[a.len() - 1] += 1` and `s[s.len() - 1] += s[0]` are fine, the index and right
   operand being read before the place is written (`ok_index_with_index_from_same_array`,
   `ok_mut_slice_index_reads_and_writes`), while `*second += a.len()` with `second` a live
   `&mut a[1]` is E0502 (`err_len_while_element_mut_borrowed_in_compound`).
8. **No moves out of an index (E0508).** A non-Copy element, a non-Copy field of one, or a
   row of a nested array cannot be moved out by indexing, from an array, through
   `&[T; N]` (E0508, not E0507), or from `&[T]`/`&mut [T]` ("a non-copy slice")
   (`moves/err_move_*`). Copy elements and Copy fields are copied
   (`ok_copy_out_of_index`); a whole array field may be moved out of a struct
   (`ok_move_array_field_out_of_struct`).
9. **Unsized coercion sites.** `&[T; N]` becomes `&[T]` and `&mut [T; N]` becomes
   `&mut [T]` or `&[T]` at call arguments, annotated `let`s and return values (`slices`,
   `ok_return_element_ref_from_slice`), and `&mut [T]` reborrows as `&[T]`. `.len()` on an
   array autorefs and unsizes the receiver, borrowing the whole array (rule 7). Nothing
   goes back (`&[T]` to `&[T; N]`), changes the element type or adds `mut`
   (`no_slice_to_array_coercion`), and `[T]` by value is E0277
   (`unsized_slice_by_value`). An elided return lifetime ties `&s[i]` to the slice
   parameter (`ok_return_element_ref_from_slice`, `err_returned_element_ref_blocks_assignment`).
10. **Mutability.** An element is mutable when its array is: a `mut` binding, a
    `&mut [T]`/`&mut [T; N]` (whatever the binding's own mutability) or a `mut`
    parameter; through `&[T]` or `&Grid` it is E0594/E0596, also for a `&mut self` method
    on an element (`mutability/*`).
11. **Drop order.** An array's elements are dropped first to last, at scope end and while
    unwinding, nested arrays row by row, an array field at its place in the field order
    (`drops`, `drops_unwind_index`). An unfinished array literal drops the elements it
    made, the last made first (`drops_unwind_literal`).
12. **Constant-index lint.** An index rustc's MIR lint proves out of bounds (a literal, a
    `const`, a local holding a constant, through `&[T; N]`) is the deny-by-default
    `unconditional_panic` lint, an uncoded error "this operation will panic at runtime"
    labelled "index out of bounds: the length is 3 but the index is 5" ("length", where
    the run-time message says "len"). Slices are never linted. The lint runs only with
    code generation: `--emit=metadata` does not report it (`unconditional_panic`).

### Notes for the implementer

- **E0508's bucket.** rustc reports E0508 from its borrow checker, but it is not in
  `RUSTC_BORROW_CODES`, so the five `moves/err_move_*` programs classify as
  `OTHER-ERROR` until it is added. They are staged as differential programs because #9
  puts the check in the borrow checker: add `"E0508"` to `RUSTC_BORROW_CODES`, give it the
  next free Sabiko borrowck code (E0419, or E0420 if #8 takes E0419 for E0510), widen
  `SABIKO_BORROW_CODES`, and update the code list in `tests/differential/README.md` and
  the SPEC §11.4 mapping. Otherwise move them to `tests/ui/`.
- **`unconditional_panic`.** No compile-time program relies on it (the compile-time
  harness would not see it), and no run program uses a constant out-of-bounds index, or
  rustc refuses to build it. Sabiko should reject what `unconditional_panic.sabi` rejects
  (an error after MIR building, from constant propagation of index operands), or the
  generator and corpus must keep avoiding constant indices; `G` (a slice) must stay
  accepted.
- **Index width.** `usize` is `native uint`; the bounds check must compare the full
  64-bit index against the length (`(nuint)i >= (nuint)len`, the JIT-friendly form the
  issue asks for), never a truncation to `int32`, or `index_out_of_bounds_huge` reads
  `a[0]`. The CLR's `IndexOutOfRangeException` carries neither number, so the explicit
  check is also what produces rustc's message.
- **Panic locations** are the start of the whole indexing expression (`a[i]` at `a`, 6:5;
  `m[i][j]` at `m`, 4:13; `s[i] += 1` at `s`, 4:5), the operator for an overflow in the
  index (`i - 1`, 4:7), and the `Panic` call inside an index expression (5:9).
- **Evaluation order** (rule 6) differs between reads and assignments: MIR building must
  evaluate an assignment's right operand before the index operand, also for compound
  assignment on primitives, and emit the bounds check after both.
- **`.len()`** keeps Rust's lowercase name, like `drop`: `a.Len()` is E0599 (Sabiko's
  E0319) and `a.len` E0615 (E0320). It is a method of arrays and slices only, borrows its
  receiver (rules 3, 7), and returns `usize`.
- **Zero-sized cases** (`zero_length.sabi`): `[]` with an annotation, `[7; 0]`, an empty
  `&[]` (a promoted constant), and `[(); 3]`, whose `()` elements need a storage decision
  (decision 32 stores `()` as `Unit` in fields).
- **Release mode** (`release_index_wraps.sabi`) keeps the bounds check while index and
  element arithmetic wrap.
- **Printing and comparing** arrays stay errors and gaps: `PrintLine(a)` is E0277
  (`print_array.sabi`), `==` on arrays is `known_gap/array_equality.sabi`.
