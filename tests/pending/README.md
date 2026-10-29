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
| `ok_two_phase_reborrow_tuple_field.sabi` | `Use(m, m.0)` for `m: &mut (i32, i32)`: the implicit reborrow of `m` is two-phase in rustc, and Sabiko rejects it (two-phase borrows, #15) | accept |
| `equality.sabi` | tuple `==`, `!=`, `<`, ... (also of `()` against `()` inside tuples): E0107 until the comparison traits of Phase 6 | runs, exit 0 |
| `refs_through_double_ref.sabi` | `rr.1.1` for `rr: &&(i32, (i32, i32))`: code generation rejects `&&T` (E0500) until `Ref<T>` (Phase 5) | runs, prints 3 |

## `methods/`: issues #7 (inherent `impl` blocks and methods) and #15 (two-phase borrows)

Checked the same way as `tuples/` above (rustc 1.99.1 (58cf8f9c3 2026-10-13), the
harnesses' preparation, `.expected` notes beside the run-time programs, `//~ rustc`
annotations on the UI programs). The programs use only what SPEC.md describes plus what
#7 and #15 add: no tuples, traits, generics, enums or closures. `run/methods/self_type.sabi`
also uses `Self`, which #7 does not name. The two-phase programs belong to whichever PR
implements #15. That PR also moves `tuples/known_gap/ok_two_phase_reborrow_tuple_field.sabi`
into `tests/differential/loans/`, once tuples have landed.

### Compile-time verdict pairs (`methods/differential/`)

| file | tests | rustc |
| --- | --- | --- |
| `loans/ok_two_phase_autoref_receiver.sabi` | #15: `v.Add(v.Get())`, `v.Add(v.X)`, `a.B.Add(a.B.Get())`, `a.B.Add(a.N + a.B.X)` | accept |
| `loans/ok_two_phase_reborrow_argument.sabi` | #15: `Use(m, m.X)`, `Use(m, m.Get())`, `P::Add(m, m.Get())` for `m: &mut P` | accept |
| `loans/ok_two_phase_receiver_through_mut_ref.sabi` | #15: `m.Add(m.Get())` for `m: &mut P`, `a.B.Add(a.B.Get())` behind `&mut A`, `self.Add(self.Get())` in a `&mut self` method | accept |
| `loans/ok_two_phase_shared_borrow_before_reservation.sabi` | #15: `let s = &v; v.Add(s.Get());` where `s` is dead after the argument | accept |
| `loans/err_two_phase_shared_borrow_used_after_call.sabi` | #15: the same, with `s` used after the call | E0502 |
| `loans/err_two_phase_later_arg_mut_borrows.sabi` | #15: `v.Add(v.Bump())` where `Bump` takes `&mut self` | E0499 |
| `loans/err_two_phase_later_arg_writes.sabi` | #15: `v.Add({ v.X = 5; 2 })` | E0506 |
| `loans/err_two_phase_whole_receiver_as_shared_arg.sabi` | #15: `v.AddFrom(&v)`, where the argument's shared borrow is still live when the call activates the receiver's borrow | E0502 |
| `loans/err_explicit_mut_borrow_arg_not_two_phase.sabi` | #15: `P::Add(&mut v, v.Get())`, with an explicit `&mut v` argument | E0502 |
| `loans/err_explicit_reborrow_arg_not_two_phase.sabi` | #15: `Use(&mut *m, m.X)`, with an explicit reborrow | E0503 |
| `loans/err_explicit_mut_receiver_not_two_phase.sabi` | #15: `(&mut v).Add(v.Get())`, with an explicit `&mut` receiver | E0502 |
| `loans/err_chained_receiver_not_two_phase.sabi` | #15: `a.BMut().Add(a.N)`, where the receiver is a `&mut` returned by a call | E0503 |
| `loans/err_reborrow_argument_later_mut_use.sabi` | #15: `Use(m, m.Bump())` | E0499 |
| `loans/ok_shared_method_refs_coexist.sabi` | `p.XRef()`, `p.YRef()` and `p.Sum()` live together | accept |
| `loans/err_shared_method_ref_then_mut_method.sabi` | a reference from `p.XRef()` live across `p.Inc()`, even though `Inc` only writes another field | E0502 |
| `loans/err_mut_method_ref_covers_whole_receiver.sabi` | `let x = p.XMut(); p.Y = 5; *x += 1;`: the result borrows all of `p` | E0506 |
| `loans/ok_mut_method_ref_after_last_use.sabi` | the borrow from `p.XMut()` ends at the result's last use; `*p.XMut() += p.Y` | accept |
| `loans/ok_elision_self_with_other_ref_param.sabi` | `fn Pick(&self, other: &P) -> &i32`, `fn PickMut(&mut self, other: &P, n: &i32) -> &mut i32`: no E0106, and the other arguments stay writable while the result is live | accept |
| `loans/err_elision_returns_other_param.sabi` | `Pick(&self, other: &P) -> &i32 { &other.X }` | uncoded `lifetime may not live long enough` |
| `loans/err_elision_output_borrows_self.sabi` | `p.X = 5` while `p.Pick(&q)`'s result is live | E0506 |
| `loans/ok_elision_by_value_self_one_ref.sabi` | `fn Larger(self, n: &i32) -> &i32`: the output borrows `n`, not the receiver | accept |
| `loans/err_elision_by_value_self_two_refs.sabi` | `fn Pick(self, a: &i32, b: &i32) -> &i32` | E0106 |
| `loans/err_temporary_receiver_ref_escapes.sabi` | `let r = Make().XRef(); PrintLine(*r);` (the receiver gets no lifetime extension) | E0716 |
| `loans/ok_method_ref_on_temporary_same_statement.sabi` | `*Make().XRef()`, `*Make().XMut() = n`, `Make().Inc()` | accept |
| `loans/err_method_ref_outlives_receiver.sabi` | `r = p.XRef()` for a `p` that dies before `r` is used | E0597 |
| `loans/ok_autoderef_receivers.sabi` | receivers of type `&P`, `&&P`, `&mut P` and `&mut &mut P` | accept |
| `loans/err_mut_method_while_shared_borrowed.sabi` | `p.Inc()` while `&p.X` is live | E0502 |
| `loans/err_shared_method_while_mut_borrowed.sabi` | `p.Get()` while `&mut p` is live | E0502 |
| `loans/err_shared_receiver_arg_writes.sabi` | `p.Plus({ p.X = 5; 2 })` for `Plus(&self, ..)`. A shared receiver borrow is not two-phase | E0506 |
| `moves/err_by_value_self_then_use.sabi` | `p.Consume(); p.Get()` | E0382 |
| `moves/ok_by_value_self_copy_struct.sabi` | by-value `self` on a Copy struct, also through `&P` | accept |
| `moves/err_receiver_moved_before_args.sabi` | `p.Merge(p.X)` with a by-value `self`: the receiver moves before the argument is read | E0382 (at `p.X`) |
| `moves/ok_args_read_before_receiver_moved_ufcs.sabi` | `P::Merge(p, x)` after `let x = p.X`, `P::MergeInto(q.X, q)` | accept |
| `moves/err_move_field_out_of_shared_self.sabi` | `fn Take(&self) -> Q { self.Inner }` | E0507 |
| `moves/ok_move_field_out_of_owned_self.sabi` | the same with a by-value `self`, two fields moved out | accept |
| `moves/err_by_value_method_through_shared_ref.sabi` | `r.Consume()` for `r: &P`, non-Copy | E0507 |
| `moves/ok_copy_fields_out_of_shared_self.sabi` | Copy fields, including a Copy struct, read out of `&self`; methods named like fields | accept |
| `moves/ok_by_value_self_reinit.sabi` | `p = p.WithX(2).WithY(3)` on a non-Copy struct, `mut self` | accept |
| `moves/err_by_value_self_partially_moved.sabi` | `let q = p.Inner; p.Consume()` | E0382 (partially moved) |
| `moves/ok_mut_ref_receiver_not_moved.sabi` | `m.Inc(); m.Inc(); P::Inc(m); Bump(m);` reborrow `m` instead of moving it | accept |
| `moves/err_two_phase_later_arg_moves_field.sabi` | #15: `v.Store(v.Inner)` moves a field out of the reserved receiver | E0505 |
| `moves/err_self_used_after_self_move.sabi` | `self.Consume()` followed by `self.X` in a by-value `self` method | E0382 |
| `mutability/err_mut_method_on_immutable_binding.sabi` | `p.Inc()` without `let mut p` | E0596 |
| `mutability/err_mut_method_through_shared_ref.sabi` | `r.Inc()` for `r: &P`; `self.Inc()` in a `&self` method | E0596 (twice) |
| `mutability/ok_mut_method_through_mut_ref.sabi` | `&mut self` calls through non-`mut` `&mut P` bindings and parameters, and on `self` in a `&mut self` method | accept |
| `mutability/err_mut_method_on_field_of_immutable.sabi` | `a.B.Inc()` without `mut`, and through `&A` | E0596 (twice) |
| `mutability/err_assign_through_shared_self.sabi` | `self.X = n` and `&mut self.X` in `&self` methods | E0594, E0596 |
| `mutability/err_assign_by_value_self_not_mut.sabi` | `self.X = n` in a `self` (not `mut self`) method | E0594 |
| `mutability/err_reassign_by_value_self.sabi` | `self = P { .. }` without `mut self` | E0384 |
| `mutability/ok_mut_by_value_self.sabi` | `mut self` on a Copy receiver bound without `mut` | accept |

### Run-time programs (`methods/differential/run/methods/`)

| file | tests | rustc |
| --- | --- | --- |
| `basics.sabi` | constructors `Counter::New`, `self`/`&self`/`&mut self`, methods calling methods and associated functions, `Counter::TickN(&mut c, 2)`, two impl blocks (one before the struct), field `Count` beside method `Count()` | exit 0 |
| `receivers.sabi` | autoref of places, auto-deref through `&P`/`&mut P`, `(*r).Get()`, `a.B.Inc()` behind `&mut A`, `a.BRef().Get()`, `a.CMut().Inc()`, `P::Make(41).Get()`, `P::Make(0).Inc()` on a temporary | exit 0 |
| `chaining.sabi` | by-value builder chains, `w.Bump().Bump().Bump()` through `-> &mut V`, `*w.Bump().XMut() *= 2`, `p.Pick(true).Bump().Bump()`, by-value `self` on `p.Right()` (a `&V`) | exit 0 |
| `eval_order.sabi` | receiver before arguments, arguments left to right, when the receiver is a call; by-value Copy receiver read before a later argument changes it (`c.Plus(Change(&mut c))` is `8`, not `57`); `v.Add(v.Get(), v.X)`; `v.X += v.Bump()` runs the right side first (161) | exit 0 |
| `two_phase.sabi` | #15 at run time: `s.Push(s.Len())`, `m.Push(m.Len())`, `PushTwice(m, m.Len())`, `Stack::Push(m, m.Len() + 100)`, `m.Push(m.Sum as i32)` | exit 0 |
| `recursion.sabi` | self-recursive `&self`, `&mut self` and by-value `self` methods, a recursive associated function, mutually recursive methods, `Math { Base: 3 }.Fact(5)` | exit 0 |
| `copy_self.sabi` | `mut self` on Copy receivers leaves the caller's value alone, also through `&P`/`&mut P` | exit 0 |
| `elision.sabi` | returned references from `&self` with another reference parameter, from `&mut self` (written through), from a by-value `self` with one reference parameter, and through `o.InnerMut().YMut(&by)` | exit 0 |
| `panic_in_method.sabi` | `Panic` inside a `&mut self` method | exit 101, `insufficient funds` at 7:31 |
| `overflow_in_method.sabi` | `i16` `n * self.K` overflowing in a `&self` method | exit 101, `attempt to multiply with overflow` at 7:17 |
| `overflow_compound_self_field.sabi` | `self.N += 1` on a `u8` field in a loop | exit 101, `attempt to add with overflow` at 6:9 |
| `release_wrapping.sabi` | `//@ release`: wrapping `+ - *` and unary `-` in by-value, `&self`, `&mut self` methods and an associated function | exit 0 |
| `self_type.sabi` | **possibly beyond #7**: `Self` as a return type, parameter type, struct literal and path | exit 0 |

### UI programs (`methods/ui/`), rejected by rustc with non-borrow errors

| file | tests | rustc | stage |
| --- | --- | --- | --- |
| `unknown_method.sabi` | `p.Nope()`, `r.Nope(1)`, `self.Nope()`, field called as a method `p.X()`, method as a field `p.Get` | E0599 x4, E0615 | typeck |
| `unknown_associated_function.sabi` | `P::Nope()`; `q.New()` for an associated function without `self` | E0599 x2 | typeck |
| `wrong_arg_count.sabi` | too few/many arguments to methods ("this method takes"), to an associated function and to methods called as `P::Add(&mut p)` ("this function takes", receiver counted) | E0061 x6 | typeck |
| `receiver_type_mismatch.sabi` | `P::Get(p)`, `P::Get(5)`, `P::Inc(&p)`, `P::Get(&q)`, `P::Consume(&p)`: no autoref or auto-deref in path calls | E0308 x5 | typeck |
| `method_call_mismatched_args.sabi` | wrong argument types, a method's result against an annotation, `()` from a `&mut self` method | E0308 x5 | typeck |
| `method_on_primitive.sabi` | `impl i32 { .. }`; `x.Triple()`, `5.Triple()`, `true.Not()`, `1.5f64.Half()` | E0390, E0599 x4 | resolve / typeck |
| `duplicate_method.sabi` | duplicate method in one impl block; duplicate associated function across two impl blocks | E0592 x2 | resolve |
| `self_value_outside_method.sabi` | `self.X` in an associated function without `self` and in a free function | E0424 x2 | resolve |
| `self_param_in_free_function.sabi` | `fn Free(self)` (alone, as a parser error in rustc) | uncoded `` `self` parameter is only allowed in associated functions `` | parse or resolve |
| `unqualified_associated_function.sabi` | `New()` and `Get(self)` without `P::` inside the impl | E0425 x2 | resolve |
| `path_to_non_type.sabi` | `impl F` for a function, `impl Nope`; `F::New()`, `x::New()` for a local, `Nope::New()`, `i32::New()` | E0573, E0425, E0433 x3, E0599 | resolve / typeck |

### Known gaps (`methods/known_gap/`)

| file | tests | rustc |
| --- | --- | --- |
| `double_ref_receivers.sabi` | run-time `&&P` and `&mut &mut P` receivers, `&&P` parameter; needs the `&&T` lowering (codegen E0500 today) | exit 0 (`.expected`) |
| `associated_consts.sabi` | `const` items in an impl, `Grid::Cells`, `Self::Cells` (outside #7's scope) | exit 0 (`.expected`) |

### Two-phase borrows: the rules rustc applies (all confirmed with the programs above)

1. **Autoref'd `&mut self` receiver.** The receiver borrow is *reserved* when the receiver
   is evaluated, before the arguments run. It is *activated* at the call. Until then it
   behaves like a shared borrow: later arguments may read the receiver directly, through
   fields or through `&self` methods (`ok_two_phase_autoref_receiver`). This also holds for
   a field path (`a.B.Add(a.B.Get())`), for a `&mut P` binding (`m.Add(m.Get())`, the
   autoref is the reborrow `&mut *m`), and for `self` in a `&mut self` method
   (`ok_two_phase_receiver_through_mut_ref`).
2. **Implicit reborrow of a `&mut` argument.** Passing `m: &mut P` where `&mut P` is
   expected reborrows `&mut *m`, and that reborrow is two-phase: `Use(m, m.X)`,
   `Use(m, m.Get())`, `P::Add(m, m.Get())` (`ok_two_phase_reborrow_argument`).
3. **Explicit borrows are never two-phase**, whatever their position:
   - `P::Add(&mut v, v.Get())` is E0502;
   - `Use(&mut *m, m.X)` is E0503;
   - `(&mut v).Add(v.Get())` is E0502.

   A receiver that is a call returning `&mut` (`a.BMut().Add(a.N)`) is a normal borrow
   made before the arguments, so it is E0503 too. Only the outermost implicit reborrow of
   such a receiver is two-phase.
4. **During the reservation, later arguments may only read.** The following are errors:
   - a mutable borrow (`v.Add(v.Bump())` and `Use(m, m.Bump())`: E0499);
   - an assignment (`v.Add({ v.X = 5; 2 })`: E0506);
   - a move out of a field (`v.Store(v.Inner)`: E0505).
5. **At activation, no other borrow of the place may be live.** A shared borrow created by
   an argument and passed to the call conflicts (`v.AddFrom(&v)`: E0502). A value computed
   from the receiver does not (`v.Add(v.Get())`).
6. **A shared borrow taken before the reservation is tolerated** while it is only used up
   to the activation. `let s = &v; v.Add(s.Get());` is accepted. Using `s` after the call is
   E0502 (`ok_/err_two_phase_shared_borrow_*`). Sabiko's loan checker must not reject the
   reservation itself when it meets a live shared loan. Only the activation, and every
   use of the two-phase borrow after it, may conflict.
7. **Shared receivers are not two-phase.** A `&self` receiver is borrowed before the
   arguments and stays borrowed through the call. An argument that writes the receiver
   is E0506 (`err_shared_receiver_arg_writes`).

### Notes for the implementer

- **Evaluation order.** The receiver is evaluated first, then the arguments left to right.
  - A by-value receiver is moved or copied before the arguments run.
    `p.Merge(p.X)` on a non-Copy `p` is E0382 at `p.X`, while `P::Merge(p, x)` after
    `let x = p.X` is fine.
  - A Copy receiver's value is read before a later argument changes it:
    `c.Plus(Change(&mut c))` sees the old `c` (`eval_order.sabi` prints `8 50`).
  - When the receiver is a call (`MakeP(&mut log).Add(..)`, `t.Pick(&mut log, ..).Add(..)`),
    that call runs before the arguments.
  - `v.X += v.Bump()` runs the right side first and then reads `v.X` (161, not 61), as
    for tuple fields.
- **Elision.** With `&self`/`&mut self`, the output lifetime is `self`'s, whatever the
  other parameters are, so `Pick(&self, other: &P) -> &i32` is not E0416/E0106. Returning
  `&other.X` is the "lifetime may not live long enough" error (E0415). A by-value `self`
  takes no part: `Larger(self, n: &i32) -> &i32` borrows `n`, and two other reference
  parameters are E0106. The result borrows the *whole* `*self`, not the field the body
  returns (`err_mut_method_ref_covers_whole_receiver`,
  `err_shared_method_ref_then_mut_method`).
- **Temporaries.** A temporary receiver is a mutable place (`Make().Inc()`,
  `*Make().XMut() = n`). Code generation needs it in an addressable local. A reference
  derived from it dies at the end of the statement: `let r = Make().XRef();` followed by a
  use is E0716. Sabiko's temporary lifetime extension for `let r = &<value>;` must not
  extend a method receiver.
- **Autoref on a `&mut` binding reborrows, it does not move.** `m.Inc(); m.Inc(); Bump(m);`
  is accepted (`ok_mut_ref_receiver_not_moved`). By-value `self` through a reference
  copies a Copy receiver (`r.Consume()`, `p.Right().Sum()`) and is E0507 otherwise.
- **Path calls (`P::Get(..)`) get no autoref or auto-deref.** The receiver must have
  exactly the declared receiver type (E0308), and arity counts it. rustc says "this
  function takes 2 arguments" for `P::Add(&mut p)`, but "this method takes 1 argument"
  for `p.Add()`.
- **Namespaces.** Fields and methods are separate namespaces: `c.Count` and `c.Count()`
  coexist (`basics.sabi`), as do field `X` and method `X()` in
  `ok_copy_fields_out_of_shared_self`. `p.X()` for a field is E0599, and `p.Get` for a
  method is E0615. Associated functions are not in scope unqualified inside their impl
  (E0425). An impl may come before its struct.
- **Duplicates.** rustc reports a duplicate method within one impl block and across two
  impl blocks of the same type as E0592. In the second case it reports at the *first*
  definition, with the later one as the "other definition".
- **Parsing.**
  - `5.Triple()` lexes as the integer `5` followed by `.Triple`, while `1.5f64.Half()`
    is a float literal followed by `.Half`.
  - A struct literal can be a receiver outside conditions (`Math { Base: 3 }.Fact(5)`,
    `*Pick { Left: true }.Of(&p)`).
  - `fn Free(self)` is a parser error in rustc. It has a file of its own, so no other
    expectation depends on how rustc recovers from it.
- **Mutability of `self`.** A by-value `self` is an immutable binding unless written
  `mut self`. `self.X = n` is E0594 ("as `self` is not declared as mutable"), and
  `self = ..` is E0384 ("cannot assign to immutable argument `self`"). In `&self`, `self`
  is a `&P`, and `self.Inc()` is E0596 "cannot borrow `*self` as mutable".
- **Codegen.** `run/methods` avoids `&&T` (the `&&P` receivers are in
  `known_gap/double_ref_receivers.sabi` and in the compile-time
  `ok_autoderef_receivers.sabi`, which only reaches borrow checking). Chains such as
  `*w.Bump().XMut() *= 2` and `p.Pick(true).Bump().Bump()` pass managed pointers from one
  `-> &mut` method to the next.
- Panic locations inside methods are the method body's: the `Panic` call (7:31) or the
  overflowing expression (`n * self.K` at 7:17, `self.N += 1` at 6:9).
