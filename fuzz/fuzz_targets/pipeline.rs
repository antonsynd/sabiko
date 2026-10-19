//! The whole pipeline (lex, parse, resolve, typeck, MIR build + validate + simplify,
//! borrowck, codegen) must never panic, whatever the input. Run with
//! `cargo +nightly fuzz run pipeline` from `fuzz/`, or `scripts/fuzz.sh pipeline 600`.
//!
//! Only "no panic" is asserted here: the driver's MIR validator already debug-asserts
//! after every pass, and the IL of accepted programs is checked by ILVerify in the
//! snapshot and run-pass tests.
#![no_main]

use libfuzzer_sys::fuzz_target;
use sabiko_driver::{Emit, Options, OutputKind};

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else {
        return;
    };
    // `SABIKO_FUZZ_PRINT=1` echoes the input (`scripts/fuzz.sh` does this for every
    // reproducer, so all targets report alike).
    if std::env::var_os("SABIKO_FUZZ_PRINT").is_some() {
        eprintln!("---- program ----\n{src}\n---- end ----");
    }
    let il = Options {
        emit: Emit::Il,
        output_kind: OutputKind::Library,
        ..Options::default()
    };
    let _ = sabiko_driver::compile_str("fuzz.sabi", src, il);
    // The MIR printer runs only for `Emit::Mir`, and it is a separate code path over the
    // same bodies.
    let mir = Options {
        emit: Emit::Mir,
        ..Options::default()
    };
    let _ = sabiko_driver::compile_str("fuzz.sabi", src, mir);
});
