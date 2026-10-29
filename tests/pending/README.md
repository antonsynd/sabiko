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

## `drops/`: issue #12, drop elaboration

Every program was checked with rustc 1.99.1 (58cf8f9c3 2026-10-13), the version
`rust-toolchain.toml` pins, prepared exactly as the harnesses prepare it:

- compile-time programs (`differential/{loans,moves,mutability}`): the five-line prelude of
  `crates/sabiko_driver/tests/differential.rs` prepended, `rustc --edition 2021
  --crate-type lib --emit=metadata -A warnings`; `ok_*` are accepted, and `err_*` are
  rejected with borrow-family codes only (the `RUSTC_BORROW_CODES` list there);
- run-time programs (`differential/run/drops`): the prelude of
  `crates/sabikoc/tests/differential_run.rs` appended (so line numbers are the file's), its
  lints as `-A` flags, `--crate-type bin -C overflow-checks=on`, run with
  `RUST_BACKTRACE=0`. That harness accepts no directive but `//@ release` and compares
  against rustc live, so there are no goldens: each program has a sibling `.expected` note
  with rustc's stdout, exit code and panic report, for convenience only (delete them when
  moving the programs);
- UI programs (`ui/`): rejected by rustc with errors outside the borrow family. Each error
  line carries `//~ rustc E0xxx: <rustc's primary message>`, sometimes followed by
  `[<rustc's primary label>]`. Every rustc error is annotated and every annotation was
  reported, at that line. The implementer replaces them with `//~ ERROR <Sabiko code> ...`
  and blesses the `.stderr` files.

Every droppable type prints from its `drop`, and most programs print `make N` when a value
is built, so the output is the drop order. The programs use what SPEC.md describes, tuples
(#6), and `impl Drop for S { fn drop(&mut self) { .. } }` (decision 46). Only
`run/drops/methods.sabi` and `ui/move_out_of_drop_type.sabi` use inherent methods (#7), and
`ui/explicit_drop_call.sabi` its call syntax (`d.drop()`, `D::drop(&mut d)`).
There is no `std::mem::drop`: a by-value `fn Consume(d: D)` drops early.

### Compile-time verdict pairs (`drops/differential/`)

| file | tests | rustc |
| --- | --- | --- |
| `loans/err_drop_move_while_borrowed.sabi` | `let r = &d; Consume(d); r.Id` | E0505 |
| `loans/ok_drop_borrow_ends_before_move.sabi` | a borrow dead before `d = ..` (a drop) and before `Consume(d)` | accept |
| `loans/err_drop_referent_dropped_at_scope_end.sabi` | `r = &d` for a `d` dropped at the end of an inner block, `r` used after it | E0597 |
| `loans/err_drop_referent_dropped_in_loop.sabi` | `r = &d` for a loop body's `d`, `r` used after the loop | E0597 |
| `loans/ok_drop_ref_declared_before_referent.sabi` | `let r; let d = ..; r = &d;`: `d` is dropped first, `r` has no drop glue | accept |
| `loans/ok_drop_tuple_reference_not_drop_live.sabi` | a `(&D, D)` declared before its referent, and a `(&mut D, D)` whose referent is reassigned: dropping the tuple does not use the reference (drop-liveness) | accept |
| `loans/err_drop_tuple_use_keeps_borrow_live.sabi` | the same `(&mut x, D)`, then `m.1.Id` after `x = ..`: a use of the tuple keeps every region in it live | E0506 |
| `loans/err_drop_reassign_while_borrowed.sabi` | `d = D { .. }` while `&d.Id` is live | E0506 |
| `loans/err_drop_assign_field_while_borrowed.sabi` | `o.Inner = D { .. }` on a `Drop` type while `&o.Inner.Id` is live | E0506 |
| `loans/err_drop_temporary_dropped_while_borrowed.sabi` | `r = &Make(1);` (assignment: no extension), then `r.Id` | E0716 |
| `loans/ok_drop_temporary_extended.sabi` | `let r = &Make(1);` and `let t = (&Make(2), 3);` live to the end of the block | accept |
| `moves/err_drop_use_after_consume.sabi` | `Consume(d); d.Id` | E0382 |
| `moves/err_drop_conditional_move_then_use.sabi` | `if flag { Consume(d); } d.Id` | E0382 |
| `moves/err_drop_move_in_loop.sabi` | `Consume(d)` in a loop body without reinitialising | E0382 |
| `moves/ok_drop_move_in_loop_reinit.sabi` | moved and reinitialised in every iteration, or on the same iterations | accept |
| `moves/ok_drop_partial_move_from_plain_struct.sabi` | droppable fields moved out of a struct without `Drop`, and out of a tuple; the rest used | accept |
| `moves/err_drop_partial_move_then_whole.sabi` | `Consume(p.A); Take(p);` | E0382 ("partially moved") |
| `moves/ok_drop_copy_field_out_of_drop_type.sabi` | `i32`, Copy struct and `o.Inner.Id` read out of a `Drop` type; then `Take(o)` | accept |
| `moves/ok_drop_reinit_moved_field.sabi` | `Consume(p.A); p.A = ..; Take(p);` | accept |
| `moves/err_drop_move_out_of_self_in_drop.sabi` | `let inner = self.Inner;` inside `drop(&mut self)` | E0507 |
| `mutability/ok_drop_immutable_binding_is_dropped.sabi` | non-`mut` locals, deferred-init locals, parameters and temporaries are dropped although `drop` takes `&mut self` | accept |
| `mutability/ok_drop_mutates_self.sabi` | `drop` writes fields, borrows them `&mut`, assigns a droppable field | accept |
| `mutability/err_drop_reassign_immutable.sabi` | `d = ..` without `mut` | E0384 |
| `mutability/err_drop_deferred_init_twice.sabi` | `let d: D; d = ..; d = ..;` | E0384 |
| `mutability/err_drop_conditional_init_then_use.sabi` | assigned on one branch, then `d.Id` | E0381 |
| `mutability/ok_drop_conditional_init_unused.sabi` | assigned on one branch and never used (only dropped); assigned on both | accept |

### Run-time programs (`drops/differential/run/drops/`)

| file | tests | rustc |
| --- | --- | --- |
| `scope_order.sabi` | reverse declaration order at block and function end; a block expression's locals before its value is bound; a block's moved-out result; parameters after the callee's locals, last parameter first | exit 0 |
| `early_exit.sabi` | `return` from nested blocks and from a loop, `break`, `continue`, `break v`: every scope left is dropped, the returned value is not | exit 0 |
| `loops.sabi` | a body local per iteration; `cur = Make(..)` in a loop drops the previous value; move and reinitialise per iteration | exit 0 |
| `drop_flags.sabi` | moved on one branch, initialised on one branch, moved on either branch, reinitialised on one branch, moved on odd iterations; `let a: D; let b = ..; a = ..;` drops `b` first | exit 0 |
| `reassignment.sabi` | `x = Make(2)` prints `make 2` then `drop 1`; no drop after a move or for a first assignment; `x = Make(x.Id + 10)`; `x = PassThrough(x)`; field assignment on a plain and on a `Drop` struct; whole assignment runs the old value's `drop` then its fields' | exit 0 |
| `fields.sabi` | own `drop` first (it sees, and may change, its fields), then fields in declaration order (literal order is only evaluation order), recursively; tuples first to last | exit 0 |
| `shadowing.sabi` | shadowed bindings live to the end of the scope; `let y = y;` moves | exit 0 |
| `moves.sabi` | dropped where the value went: callee end (after the callee's locals), with the struct or tuple holding it, by the receiver of a return | exit 0 |
| `partial_moves.sabi` | remaining fields dropped at scope end; reinitialised field; whole assignment drops only `q.A` and `q.B` (not the moved `q.C`); `let (a, _, c) = t;` leaves `t.1` in `t`; nested `u.1.0` | exit 0 |
| `let_underscore.sabi` | `let _ = Make(1);` drops at once, `let _x` at scope end, an expression statement at its end, `let (_, y) = (..);` drops the unbound element at the end of the `let` | exit 0 |
| `temporaries.sabi` | statement end, reverse creation order; `if`/`while` condition temporaries before the branch/body; `let r = &Make(..)` extended; edition 2021 tail-expression temporaries after the block's (and the function body's) locals; `return` operand temporaries before the locals | exit 0 |
| `methods.sabi` | #7: by-value `self` dropped at method end unless returned; temporary receivers at statement end; `*self = D::New(..)` drops the old value; `drop` calls a method | exit 0 |
| `unwind_locals.sabi` | panic in a nested block: live locals in reverse order, not the moved one nor the undeclared ones | exit 101, `boom` at 34:13 |
| `unwind_callee.sabi` | panic two frames down on the third iteration: callee locals, its parameter, the caller's temporary `&Make(203)`, the caller's locals, `Main`'s loop local and locals | exit 101, `too large` at 23:9 |
| `unwind_construction.sabi` | panic in the third field of a struct literal: the two fields built, in reverse order; no `drop` of the unfinished struct | exit 101, `cannot build` at 31:5 |
| `unwind_arguments.sabi` | panic in a tuple argument: the tuple's built element, then the earlier argument | exit 101, `cannot build` at 22:5 |
| `unwind_drop_flags.sabi` | panic in a loop after a conditional move and a deferred initialisation: flags respected | exit 101, `stop` at 38:13 |
| `panic_in_drop.sabi` | a `drop` that panics at a normal scope end: its fields, the rest of the block and the enclosing scopes are still dropped | exit 101, `bomb went off` at 21:13 |

### UI programs (`drops/ui/`), rejected by rustc with non-borrow errors

| file | tests | rustc | stage |
| --- | --- | --- | --- |
| `derive_copy_on_drop_type.sabi` | `#[derive(Copy, Clone)]` on a type with `impl Drop` | E0184 | typeck (or borrowck, next to E0417) |
| `copy_struct_with_drop_field.sabi` | `#[derive(Copy, Clone)]` on a struct with a droppable field | E0204, E0277 | borrowck (Sabiko's E0417 today) |
| `drop_wrong_signature.sabi` | `fn drop(&self)`, `fn drop(self)`, `-> i32`, an extra parameter | E0053 x3, E0050 | typeck |
| `drop_missing_or_extra_items.sabi` | `impl Drop for A {}`; `fn Drop(&mut self)` (PascalCase); an extra method | E0046 x2, E0407 x2 | resolve / typeck |
| `explicit_drop_call.sabi` | `d.drop()`, `D::drop(&mut d)` | E0040 x2 | typeck |
| `duplicate_drop_impl.sabi` | two `impl Drop for D` | E0119 (at the second) | resolve / typeck |
| `drop_for_non_struct.sabi` | `impl Drop for i32`, `for (D, D)`, `for &D` | E0117 x2, E0120 x3 | typeck |
| `drop_for_unknown_type.sabi` | `impl Drop for Nope` (alone: a resolve error) | E0425 | resolve |
| `move_out_of_drop_type.sabi` | moving `Inner` out of a `Drop` type into a binding, an argument, and out of a by-value `self` | E0509 x3 | borrowck |

### Known gaps (`drops/known_gap/`)

| file | tests | rustc |
| --- | --- | --- |
| `panic_in_drop_while_unwinding.sabi` | a `drop` that panics while a panic unwinds: rustc aborts (SIGABRT, "panic in a destructor during cleanup") and runs no further drops; a `finally` that throws replaces the panic in flight instead, so Sabiko would report the second panic and exit 101. No harness can compare an abort (`.expected` has rustc's output) | abort |
| `let_underscore_place.sabi` | `let _ = d;` neither moves nor drops `d` (a pre-existing TODO in `sabiko_mir::build`: Sabiko reads the place) | exit 0 (`.expected`) |

### Drop rules rustc applies (each confirmed by the program named)

1. **Scope end, reverse declaration order.** Locals are dropped when their block ends, the
   last declared first (`scope_order`). The order is by declaration, not by
   initialisation: in `let a: D; let b = ..; a = ..;`, `b` is dropped before `a`
   (`drop_flags`, `unwind_drop_flags`).
2. **Function parameters** are dropped after the function body's locals, the last
   parameter first (`scope_order`, `moves`: `Consume`'s local before its parameter).
3. **Early exits.** `return`, `break` and `continue` drop the locals of every scope they
   leave, innermost first. A value returned or carried by `break` is moved out and is not
   dropped there (`early_exit`).
4. **Loops.** A body local is dropped at the end of every iteration, including iterations
   left by `continue` and `break` (`loops`, `early_exit`).
5. **Moved-out values are not dropped** where they were declared. A by-value parameter is
   dropped at the end of the callee; a value moved into a struct or tuple is dropped with
   it (`moves`).
6. **Drop flags.** Where a local is moved or initialised on some paths only, its drop runs
   only on the paths where it holds a value, also per iteration (`drop_flags`) and while
   unwinding (`unwind_drop_flags`). Fields of a struct without `Drop` need flags of their
   own: after a partial move only the remaining fields are dropped, and a reinitialised
   field counts again (`partial_moves`, `ok_drop_reinit_moved_field`).
7. **Assignment drops the old value**, after the right-hand side is evaluated and before
   the store: `x = Make(2)` prints `make 2`, then `drop 1`. Nothing is dropped when the
   place was moved out or never initialised (`reassignment`, `loops`). A field assignment
   drops the old field only; `*self = ..` through `&mut` drops the old value (`methods`).
   Assigning a whole partially moved struct drops only the fields still in it
   (`partial_moves`).
8. **Own `drop`, then fields.** A value's own `drop` runs first, with its fields intact
   (it may read and change them). Then its fields are dropped in *declaration* order,
   whatever the order of the struct literal, recursively. A struct without `Drop` just
   drops its fields; a tuple drops its elements first to last (`fields`).
9. **Shadowing does not drop.** The shadowed value lives to the end of its scope
   (`shadowing`).
10. **Wildcards.** `let _ = <value>;` drops the value at once and `let _x = ..;` at scope
    end. An expression statement drops its value at its end. In `let (_, y) = (..);` the
    element `_` does not bind is dropped at the end of the `let` (`let_underscore`).
    `let _ = place;` does nothing at all (known gap).
11. **Temporaries** are dropped at the end of their statement, in reverse order of creation
    (`temporaries`). Exceptions:
    - an `if` or `while` condition's temporaries are dropped before the branch or body;
    - `let r = &temp;` and `let t = (&temp, ..);` extend the temporary to the end of the
      block (`ok_drop_temporary_extended`), but `r = &temp;` does not (E0716);
    - edition 2021 (the harnesses' edition): the temporaries of a block's tail expression
      outlive the block's locals. `let k = { let a = Make(30); Make(31).Id };` drops 30,
      then 31, and a function body's tail drops its temporaries after its locals. The
      operand of an explicit `return` is not a tail: its temporaries go first.
12. **Methods.** A by-value `self` is dropped at the end of the method unless it is moved
    out; a temporary receiver is dropped at the end of the statement (`methods`).
13. **Unwinding.** A panic drops every live value on the way out, frame by frame: the
    callee's locals, then its parameters, then the caller's temporaries, then its locals
    (`unwind_callee`, `unwind_locals`). Operands already evaluated for an unfinished struct
    literal, tuple or call are dropped in reverse order of evaluation. The unfinished
    struct's own `drop` does not run (`unwind_construction`, `unwind_arguments`). Exit code
    101.
14. **A panic in `drop` at a normal scope end** is an ordinary panic. The fields of that
    value are still dropped, then the rest of the scope and the enclosing ones
    (`panic_in_drop`). A panic in `drop` *during* unwinding aborts (known gap).
15. **Borrow checking.**
    - A drop at scope end invalidates every loan of the value (E0597).
    - A move into a callee and a dropping assignment do too (E0505, E0506).
    - A reference declared before its referent is fine, since references have no drop
      glue (`ok_drop_ref_declared_before_referent`).
    - Dropping an aggregate uses only the parts that need dropping. A tuple's reference
      element is not used by the tuple's drop (`ok_drop_tuple_reference_not_drop_live`).
      Any *use* of the tuple keeps all of its regions live
      (`err_drop_tuple_use_keeps_borrow_live`).
16. **Mutability.** The implicit drop takes `&mut self` but needs no `mut` binding
    (`ok_drop_immutable_binding_is_dropped`). Inside `drop`, `self` is `&mut Self`: fields
    are writable, and moving a non-Copy field out is E0507 (`ok_drop_mutates_self`,
    `err_drop_move_out_of_self_in_drop`).
17. **Types with `Drop`.**
    - No non-Copy field may be moved out of them (E0509); Copy fields may be read
      (`ok_drop_copy_field_out_of_drop_type`).
    - They cannot be `Copy` (E0184).
    - `drop` is spelled `drop` (`fn Drop` is E0407 + E0046) with exactly
      `fn drop(&mut self)`, and it cannot be called (E0040).
    - One impl per type (E0119), and only for the program's own structs (E0117/E0120).

### Notes for the implementer

- **E0509's bucket.** rustc reports E0509 from its borrow checker, but it is not in
  `RUSTC_BORROW_CODES`, so `move_out_of_drop_type.sabi` is a UI program. You can add
  E0509 to that list with a Sabiko borrowck code (E0418, which also widens
  `SABIKO_BORROW_CODES` past 417). Its three cases can then become `differential/moves/err_*`
  programs, one per file.
- **Copy and `Drop`.** Sabiko has E0417 (rustc's E0204) for `derive(Copy, Clone)` on a
  struct with a non-Copy field. rustc reports E0204 *and* E0277 (for `Clone`), so
  `copy_struct_with_drop_field` cannot be a differential program. E0184 (Copy on a type
  with `Drop`) needs a code, typeck or next to E0417.
- **Panic inside `drop` during unwinding** (known gap). Decision 12's `finally` gives .NET
  semantics: the new exception replaces the old, the remaining `finally` blocks run, and
  the process reports the second panic with exit 101. rustc aborts and runs no more
  drops. Matching it means a drop on the unwinding path is wrapped so that a panic
  escaping it fails fast (for example a fault/catch handler around the `finally`'s drop
  calls that prints rustc's report and exits 134). That is a decision, and the run harness
  cannot compare an abort today (`status.code()` is `None` for a signal).
- **Panic in `drop` at a normal scope end** must still drop the value's fields and the
  remaining locals (`panic_in_drop`). The drop glue of a struct with its own `drop` and
  droppable fields therefore needs a `try { drop(ref x) } finally { drop fields }`
  shape, not straight-line calls.
- **Order of the panic report.** rustc prints the report on stderr *before* unwinding runs
  the drops; Sabiko's entry point prints it after the `finally` blocks ran. The harness
  compares stdout and stderr separately, so this does not matter for it, but a terminal
  shows them in a different order. .NET runs `finally` blocks during the second pass only
  because `Main`'s `try/catch` catches the exception. Keep that catch around all user
  code.
- **Edition.** Rule 11's tail-expression order is edition 2021's (the harnesses pass
  `--edition 2021`); edition 2024 drops tail temporaries before the block's locals.
  `temporaries.sabi` pins 2021 behaviour.
- **Drop-liveness.** The loan checker must treat `Drop(t)` of a value without a `Drop`
  impl as a use of only the parts that have drop glue. Otherwise it rejects
  `ok_drop_tuple_reference_not_drop_live`. A `Drop` type cannot hold a reference without
  a lifetime parameter (Phase 9), so in today's language no drop of a user type keeps a
  borrow alive. Once lifetimes arrive, `impl Drop for Holder<'a>` makes the drop a use of
  `'a` (the classic E0597 "borrow might be used here, when `h` is dropped").
- **`let _ = place;`** (known gap) is the existing Phase 5 TODO. Drops make it observable:
  Sabiko's read of the place would move and drop `d` early. Fixing it in #12 moves
  `known_gap/let_underscore_place.sabi` into `run/drops/`.
- **Lowercase `drop`.** The trait method keeps Rust's name, against Sabiko's PascalCase
  convention for methods (`fn Drop` is rejected by rustc, E0407).
- **Panic locations** inside `Fail` (a `Panic(..)` tail expression typed `!` where `D` is
  expected) are the `Panic` call's (31:5, 22:5).
