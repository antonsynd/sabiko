#!/usr/bin/env bash
# Mutation testing with cargo-mutants (https://mutants.rs), configured in .cargo/mutants.toml.
#
#   scripts/mutants.sh [crate...]      default: sabiko_borrowck sabiko_codegen sabiko_types
#
# Each crate is mutated in place (`--in-place`: the sabikoc run-pass and run-time
# differential tests locate the .NET toolchain under tools/ relative to the source tree, so
# a copy of the tree would skip them; in-place runs one job at a time) and tested with the
# suites that exercise it, see test_packages. Results land in target/mutants/<crate>/mutants.out
# (missed.txt, caught.txt, outcomes.json, per-mutant logs). Extra cargo-mutants arguments
# come from MUTANTS_ARGS, e.g. `MUTANTS_ARGS="--shard 1/4"` or `MUTANTS_ARGS="-f check.rs"`.
#
# The mutation score of a crate is (caught + timeout) / (caught + missed + timeout): a
# mutant that makes the suite hang is killed too, and mutants that do not compile are not
# counted. The exit status is non-zero when a crate scores below minimum_score (set just
# under the score achieved when the thresholds were last recorded; raise them when tests
# are added, never lower them without a note in docs/PLAN.md).
#
# An interrupted in-place run can leave a mutated file behind: `git diff crates/` shows it,
# `git checkout -- <file>` restores it.
#
# Runs on macOS's bash 3.2 (no associative arrays, no empty "${a[@]}" under `set -u`). When
# rustup is installed, cargo runs as `rustup run <channel from rust-toolchain.toml> cargo`,
# so a Homebrew or distro `cargo` earlier on PATH cannot silently replace the pinned
# toolchain (cargo-mutants builds every mutant with the cargo that launched it).
set -euo pipefail

cd "$(dirname "$0")/.."

test_packages() {
    case "$1" in
        sabiko_borrowck) echo "sabiko_borrowck,sabiko_driver" ;;
        sabiko_codegen) echo "sabiko_codegen,sabikoc,sabiko_driver" ;;
        sabiko_types) echo "sabiko_types,sabiko_driver,sabiko_mir" ;;
    esac
}
# Percent, integer. Recorded 2026-10-19: borrowck 90.5% (262 caught + 5 timeouts of 295
# viable; the 28 survivors are unused helpers or unobservable branches), codegen 95.5%
# (233 of 244; 11 survivors, dead branches given the MIR the builder produces). Types,
# recorded 2026-10-21: 98.5% (407 of 413; the 6 survivors are the equivalent mutants listed
# in docs/TESTING.md). The 100% recorded for types on 2026-10-19 did not reproduce: the
# first nightly run measured 66.9%, see docs/PLAN.md.
minimum_score() {
    case "$1" in
        sabiko_borrowck) echo 90 ;;
        sabiko_codegen) echo 95 ;;
        sabiko_types) echo 98 ;;
    esac
}

crates=("$@")
if [ ${#crates[@]} -eq 0 ]; then
    crates=(sabiko_borrowck sabiko_codegen sabiko_types)
fi

channel=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
if command -v rustup >/dev/null 2>&1; then
    cargo=(rustup run "$channel" cargo)
    if ! "${cargo[@]}" --version >/dev/null 2>&1; then
        echo "mutants.sh: the pinned toolchain is missing: rustup toolchain install $channel" >&2
        exit 1
    fi
else
    cargo=(cargo)
    if ! cargo --version | grep -q "^cargo $channel "; then
        echo "mutants.sh: warning: $(cargo --version) is not the pinned $channel" \
            "(rust-toolchain.toml); install rustup to get the pinned toolchain" >&2
    fi
fi
if ! "${cargo[@]}" mutants --version >/dev/null 2>&1; then
    echo "cargo-mutants is not installed: cargo install cargo-mutants --locked" >&2
    exit 1
fi

# shellcheck disable=SC2206
extra_args=(${MUTANTS_ARGS:-})
status=0
summary=()
for crate in "${crates[@]}"; do
    packages=$(test_packages "$crate")
    if [ -z "$packages" ]; then
        echo "mutants.sh: no test packages configured for $crate" >&2
        exit 1
    fi
    out="target/mutants/$crate"
    mkdir -p "$out"
    echo "== cargo mutants -p $crate (tests: $packages)"
    start=$(date +%s)
    # Exit codes 2 (missed), 3 (timeouts) and 4 (unviable) are outcomes, not failures.
    # The build timeout is fixed rather than derived from the baseline: with a warm
    # target/ the baseline builds in no time and the derived limit kills every mutant build.
    rc=0
    "${cargo[@]}" mutants --in-place --build-timeout 900 -p "$crate" \
        --test-package "$packages" -o "$out" ${extra_args[@]+"${extra_args[@]}"} || rc=$?
    if [ "$rc" -ne 0 ] && [ "$rc" -lt 2 ] || [ "$rc" -gt 4 ]; then
        echo "mutants.sh: cargo mutants failed for $crate (exit $rc)" >&2
        exit "$rc"
    fi
    elapsed=$(( $(date +%s) - start ))

    dir="$out/mutants.out"
    count() { if [ -f "$dir/$1" ]; then grep -c . "$dir/$1" || true; else echo 0; fi; }
    caught=$(count caught.txt)
    missed=$(count missed.txt)
    timeout=$(count timeout.txt)
    unviable=$(count unviable.txt)
    total=$((caught + missed + timeout + unviable))
    viable=$((caught + missed + timeout))
    if [ "$viable" -eq 0 ]; then
        score=0
    else
        # One decimal, in tenths of a percent.
        score=$(( (caught + timeout) * 1000 / viable ))
    fi
    minimum=$(minimum_score "$crate")
    verdict=ok
    if [ "$score" -lt $((minimum * 10)) ]; then
        verdict="BELOW MINIMUM"
        status=1
    fi
    line=$(printf '%-16s %4d mutants: %4d caught, %4d missed, %3d timeout, %3d unviable; score %d.%d%% (minimum %d%%) %s, %dm%02ds' \
        "$crate" "$total" "$caught" "$missed" "$timeout" "$unviable" \
        $((score / 10)) $((score % 10)) "$minimum" "$verdict" $((elapsed / 60)) $((elapsed % 60)))
    summary+=("$line")
    echo "$line"
    if [ "$missed" -gt 0 ]; then
        echo "-- missed ($dir/missed.txt):"
        sed 's/^/   /' "$dir/missed.txt"
    fi
done

echo
echo "== mutation testing summary"
printf '%s\n' "${summary[@]}"
exit "$status"
