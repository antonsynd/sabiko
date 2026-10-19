#!/usr/bin/env bash
# Mutation testing with cargo-mutants (https://mutants.rs), configured in .cargo/mutants.toml.
#
#   scripts/mutants.sh [crate...]      default: sabiko_borrowck sabiko_codegen sabiko_types
#
# Each crate is mutated in place (`--in-place`: the sabikoc run-pass and run-time
# differential tests locate the .NET toolchain under tools/ relative to the source tree, so
# a copy of the tree would skip them; in-place runs one job at a time) and tested with the
# suites that exercise it, see TEST_PACKAGES. Results land in target/mutants/<crate>/mutants.out
# (missed.txt, caught.txt, outcomes.json, per-mutant logs). Extra cargo-mutants arguments
# come from MUTANTS_ARGS, e.g. `MUTANTS_ARGS="--shard 1/4"` or `MUTANTS_ARGS="-f check.rs"`.
#
# The mutation score of a crate is (caught + timeout) / (caught + missed + timeout): a
# mutant that makes the suite hang is killed too, and mutants that do not compile are not
# counted. The exit status is non-zero when a crate scores below MINIMUM_SCORE (set just
# under the score achieved when the thresholds were last recorded; raise them when tests
# are added, never lower them without a note in docs/PLAN.md).
#
# An interrupted in-place run can leave a mutated file behind: `git diff crates/` shows it,
# `git checkout -- <file>` restores it.
set -euo pipefail

cd "$(dirname "$0")/.."

declare -A TEST_PACKAGES=(
    [sabiko_borrowck]="sabiko_borrowck,sabiko_driver"
    [sabiko_codegen]="sabiko_codegen,sabikoc,sabiko_driver"
    [sabiko_types]="sabiko_types,sabiko_driver,sabiko_mir"
)
# Percent, integer. Recorded 2026-10-19: borrowck 90.5% (262 caught + 5 timeouts of 295
# viable; the 28 survivors are unused helpers or unobservable branches), codegen 95.5%
# (233 of 244; 11 survivors, dead branches given the MIR the builder produces), types
# 100% (420 of 420).
declare -A MINIMUM_SCORE=(
    [sabiko_borrowck]=90
    [sabiko_codegen]=95
    [sabiko_types]=99
)

crates=("$@")
if [ ${#crates[@]} -eq 0 ]; then
    crates=(sabiko_borrowck sabiko_codegen sabiko_types)
fi

if ! cargo mutants --version >/dev/null 2>&1; then
    echo "cargo-mutants is not installed: cargo install cargo-mutants --locked" >&2
    exit 1
fi

# shellcheck disable=SC2206
extra_args=(${MUTANTS_ARGS:-})
status=0
summary=()
for crate in "${crates[@]}"; do
    if [ -z "${TEST_PACKAGES[$crate]:-}" ]; then
        echo "mutants.sh: no test packages configured for $crate" >&2
        exit 1
    fi
    out="target/mutants/$crate"
    mkdir -p "$out"
    echo "== cargo mutants -p $crate (tests: ${TEST_PACKAGES[$crate]})"
    start=$(date +%s)
    # Exit codes 2 (missed), 3 (timeouts) and 4 (unviable) are outcomes, not failures.
    # The build timeout is fixed rather than derived from the baseline: with a warm
    # target/ the baseline builds in no time and the derived limit kills every mutant build.
    rc=0
    cargo mutants --in-place --build-timeout 900 -p "$crate" \
        --test-package "${TEST_PACKAGES[$crate]}" -o "$out" "${extra_args[@]}" || rc=$?
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
    minimum=${MINIMUM_SCORE[$crate]}
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
