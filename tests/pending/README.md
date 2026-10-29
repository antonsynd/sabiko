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
