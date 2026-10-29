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
