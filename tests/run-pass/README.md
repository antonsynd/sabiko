# Run-pass tests

Each `*.sabi` file here is compiled to an executable by the `sabikoc` binary, verified with
ILVerify, run with `dotnet`, and compared with its expectations
(`crates/sabikoc/tests/run_pass.rs`):

- `<name>.stdout` next to the program holds the expected standard output. `BLESS=1` creates
  or rewrites it from the actual output (only when the exit code matched).
- Directives are `//@` comments at the top of the file: `//@ exit-code: 101` (default 0;
  panics, including overflow in debug builds, exit with 101) and `//@ release` (compile
  with `--release`, so arithmetic wraps).
- A program that panics has `<name>.stderr` with its exact panic report, `thread 'main'
  panicked at tests/run-pass/<dir>/<name>.sabi:L:C:` and the message (the harness writes
  the source path relative to the repository); `BLESS=1` creates it for a program that
  exits with 101. Any other program must leave standard error empty.

ILVerify runs with `ReturnPtrToStack` ignored: ECMA-335 forbids returning any managed
pointer, which rejects every `fn ... -> &T` (and every C# `ref` return); the borrow
checker guarantees what the verifier cannot. Any other verification error fails the test.

The test needs the .NET toolchain under `tools/` (`scripts/setup-dotnet.sh`, which also
builds `Sabiko.Runtime.dll`). Without it the test prints a notice and is skipped, unless
`SABIKO_REQUIRE_DOTNET=1` is set (CI sets it). Directories group programs by topic.

Programs that must agree with `rustc` at run time belong in `tests/differential/run`
instead (see `tests/differential/README.md`).
