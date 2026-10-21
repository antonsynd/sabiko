#!/usr/bin/env bash
# Fuzz one libFuzzer target for a fixed time budget (needs rustup, the nightly toolchain
# and cargo-fuzz: `rustup toolchain install nightly --profile minimal && rustup run nightly
# cargo install cargo-fuzz --locked`).
#
#   scripts/fuzz.sh [target] [seconds]        default: pipeline, 300
#
# Every cargo call goes through `rustup run nightly cargo`, not `cargo +nightly`: the
# `+toolchain` syntax only works when the `cargo` first on PATH is rustup's proxy, and a
# Homebrew or distro Rust ahead of it answers "no such command: `+nightly`".
#
# 1. Seeds fuzz/corpus/<target>/ with every .sabi under tests/, crates/*/tests/fixtures and
#    examples/ (the corpus directory is gitignored; libFuzzer adds to it as it runs).
# 2. Runs `cargo fuzz run <target> -- -max_total_time=<seconds>` on nightly.
# 3. On a crash, prints the reproducer path, minimizes it (`cargo fuzz tmin`) and prints
#    the minimized input, then exits non-zero. Reproducers land in fuzz/artifacts/<target>/.
set -euo pipefail

target="${1:-pipeline}"
seconds="${2:-300}"
repo="$(cd "$(dirname "$0")/.." && pwd)"
fuzz_dir="$repo/fuzz"
corpus="$fuzz_dir/corpus/$target"
artifacts="$fuzz_dir/artifacts/$target"

if [[ ! -f "$fuzz_dir/fuzz_targets/$target.rs" ]]; then
    echo "fuzz.sh: unknown target '$target'; available:" >&2
    ls "$fuzz_dir/fuzz_targets" | sed 's/\.rs$//; s/^/  /' >&2
    exit 2
fi
nightly_cargo() { rustup run nightly cargo "$@"; }
if ! command -v rustup >/dev/null 2>&1; then
    echo "fuzz.sh: rustup is required (https://rustup.rs, or \`brew install rustup\`)" >&2
    exit 2
fi
if ! nightly_cargo --version >/dev/null 2>&1; then
    echo "fuzz.sh: the nightly toolchain is required:" >&2
    echo "  rustup toolchain install nightly --profile minimal" >&2
    exit 2
fi
if ! nightly_cargo fuzz --version >/dev/null 2>&1; then
    echo "fuzz.sh: cargo-fuzz is required:" >&2
    echo "  rustup run nightly cargo install cargo-fuzz --locked" >&2
    exit 2
fi

# --- 1. seed corpus --------------------------------------------------------------------
mkdir -p "$corpus" "$artifacts"
seeded=0
while IFS= read -r -d '' f; do
    rel="${f#"$repo/"}"
    cp "$f" "$corpus/seed_$(printf '%s' "$rel" | tr '/' '_')"
    seeded=$((seeded + 1))
done < <(
    find "$repo/tests" "$repo/examples" -name '*.sabi' -print0 2>/dev/null
    find "$repo"/crates/*/tests/fixtures -name '*.sabi' -print0 2>/dev/null
)
echo "fuzz.sh: $target: seeded $seeded files into ${corpus#"$repo/"}"

# --- 2. run -----------------------------------------------------------------------------
cd "$fuzz_dir"
# `-rss_limit_mb` and `-timeout` catch runaway memory and hangs as crashes too. libFuzzer
# exits non-zero on a crash; `|| status=$?` keeps our own reporting in control.
status=0
# cargo-fuzz itself points libFuzzer's -artifact_prefix at fuzz/artifacts/<target>/.
nightly_cargo fuzz run "$target" "$corpus" -- \
    -max_total_time="$seconds" -timeout=30 -rss_limit_mb=4096 || status=$?

# --- 3. report crashes --------------------------------------------------------------------
shopt -s nullglob
crashes=("$artifacts"/crash-* "$artifacts"/timeout-* "$artifacts"/oom-* "$artifacts"/leak-*)
if (( status == 0 )) && (( ${#crashes[@]} == 0 )); then
    echo "fuzz.sh: $target: no crash in ${seconds}s"
    exit 0
fi

echo
echo "fuzz.sh: $target: libFuzzer exited with status $status; ${#crashes[@]} artifact(s):"
for c in "${crashes[@]}"; do
    [[ "$c" == *.min ]] && continue # a minimized copy from an earlier run
    echo "  reproducer: $c"
    echo "  reproduce with: rustup run nightly cargo fuzz run $target ${c#"$repo/"}"
    minimized="$c.min"
    # tmin reruns the target many times; a crash whose minimization fails (e.g. a
    # timeout) still gets reported, with the original input.
    if nightly_cargo fuzz tmin -r 2000 "$target" "$c" >"$artifacts/tmin.log" 2>&1; then
        # cargo fuzz tmin writes `minimized-from-<hash>` next to the artifact.
        newest="$(ls -t "$artifacts"/minimized-from-* 2>/dev/null | head -n1 || true)"
        if [[ -n "$newest" ]]; then
            cp "$newest" "$minimized"
        fi
    else
        echo "  (minimization failed; see $artifacts/tmin.log)"
    fi
    if [[ -f "$minimized" ]]; then
        echo "  minimized:  $minimized ($(wc -c <"$minimized") bytes)"
    fi
    # Every target echoes the program it compiled when SABIKO_FUZZ_PRINT is set; for the
    # structured target that is the decoded program, which the raw bytes do not show.
    for input in "$c" "$minimized"; do
        [[ -f "$input" ]] || continue
        echo "  ---- program from ${input##*/} ----"
        # The reproducer crashes the target, so these pipelines fail by design.
        SABIKO_FUZZ_PRINT=1 nightly_cargo fuzz run "$target" "$input" 2>&1 \
            | sed -n '/^---- program ----$/,/^---- end ----$/{/^---- /d;p}' | head -n 200 || true
        echo "  ---- panic ----"
        nightly_cargo fuzz run "$target" "$input" 2>&1 | grep -A2 'panicked at' | head -n 6 || true
        echo "  -------------------------"
    done
done
exit 1
