# UI tests

Each `*.sabi` file here is compiled by `crates/sabiko_driver/tests/ui.rs`. Expectations:

- `//~ ERROR E0100` on a line: an error with that code must be reported on that line.
  The code is optional; text after it must appear in the message:
  `//~ ERROR E0100 expected `;``.
- `//~^ ERROR ...`: same, but for the previous line (one `^` per line up).
- `//~ WARN ...`: a warning instead of an error.
- Every error the compiler reports must be annotated, and every annotation must match.
- The full rendered diagnostics are compared against the `.stderr` file next to the
  source. A file without diagnostics has no `.stderr`.

Run `BLESS=1 cargo test -p sabiko_driver --test ui` to regenerate `.stderr` files after
an intentional change, then review the diff.

Directories mirror the pipeline stage under test, and the harness stops the compiler
after that stage: `parse/` (parser only), `resolve/` (through name resolution), `typeck/`
(through type checking), `borrowck/` and anything else (the whole pipeline). Files whose
name starts with `ok_` must compile without diagnostics.
