# Third-party notices

Sabiko is licensed under MIT OR Apache-2.0 (see `LICENSE-MIT` and `LICENSE-APACHE`).

## Adapted code

Portions of this software may be adapted from the Rust compiler and related crates
(https://github.com/rust-lang/rust), which are dual-licensed under the MIT License and
the Apache License 2.0. Copyright (c) The Rust Project Contributors.

Any file containing adapted code carries a header naming its origin. This section
lists them:

- `crates/sabiko_types/src/usefulness.rs`: the exhaustiveness and reachability checker
  (the usefulness algorithm, constructor splitting, integer range encoding and witness
  selection) is adapted from `compiler/rustc_pattern_analysis` (`usefulness.rs`,
  `constructor.rs`, `rustc.rs`), and its witness printing from
  `compiler/rustc_mir_build/src/thir/pattern/check_match.rs` and
  `compiler/rustc_middle/src/thir.rs` (`PatRange`) / `rustc_pattern_analysis/src/rustc/print.rs`.

## Toolchain

`scripts/setup-dotnet.sh` downloads the .NET runtime, `ilasm`, `ildasm`, ILVerify and
the .NET reference assemblies from NuGet. These are MIT-licensed by the .NET Foundation
and are not redistributed by this repository.
