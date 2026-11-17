#!/usr/bin/env bash
# Mutation testing with cargo-mutants (https://mutants.rs), configured in .cargo/mutants.toml.
#
#   scripts/mutants.sh [--shard k/n] [crate...]   default: sabiko_borrowck sabiko_codegen sabiko_types
#   scripts/mutants.sh --score <crate> [--shards n] <mutants.out dir>...
#
# Each crate is mutated in place (`--in-place`: the sabikoc run-pass and run-time
# differential tests locate the .NET toolchain under tools/ relative to the source tree, so
# a copy of the tree would skip them; in-place runs one job at a time) and tested with the
# suites that exercise it, see test_packages. Results land in target/mutants/<crate>/mutants.out
# (missed.txt, caught.txt, outcomes.json, per-mutant logs). Extra cargo-mutants arguments
# come from MUTANTS_ARGS, e.g. `MUTANTS_ARGS="-f check.rs"`.
#
# Each mutant's tests get an explicit timeout, test_timeout below (MUTANTS_TEST_TIMEOUT
# overrides it). cargo-mutants would otherwise scale its baseline test run, which only
# tests the mutated package, not the --test-package suites: the codegen baseline took 9 s
# where the suites take over a minute, so every mutant the fast tests missed ran into the
# 60 s floor and was counted as a timeout, that is as caught (nightly run 18).
#
# --shard k/n runs the k-th of n round-robin shards of each crate's mutants (k from 0) and
# reports that shard's counts without judging them: a shard's score is not the crate's.
# --score sums the counts of the given mutants.out directories (one per shard; with
# --shards n, exactly n of them) and judges the crate, as an unsharded run does.
#
# Only complete results are judged (#165). A mutants.out holds the mutants cargo-mutants
# was given (mutants.json, written before the baseline) and the outcomes so far
# (outcomes.json, rewritten after every mutant; its total_mutants counts the mutants
# tested). A shard cancelled at the job's time limit, or whose runner died, still uploads
# both, with fewer outcomes than mutants: the run and --score then fail with "shard k
# finished m of n mutants" rather than judge the part that finished. Reading the JSON
# takes python3. A run also fails when cargo-mutants exits with anything but 0, 2
# (missed mutants) or 3 (timeouts): 1 when interrupted, 4 when the unmutated tree fails
# its tests (no mutant tested).
#
# Every finished mutant is printed (--caught --unviable), not only the missed and the
# timed-out ones: when a runner dies, the last line names the mutant before the one that
# killed it.
#
# Each test process runs under a memory cap, MUTANTS_MEMORY_LIMIT_GIB (default in
# memory_limit_gib; 0 turns it off): a mutant that removes a loop's exit can grow a test
# binary by gigabytes in seconds and exhaust a runner's 16 GB long before the test timeout
# (runs 19 and 27 to 32, #165). The cap is `prlimit --as`, applied through cargo's target
# runner, so it covers the test binaries and what they start (sabikoc, ilasm, dotnet: .NET
# sizes its heap reservation to the limit) but not rustc or the linker, whose failure
# would count a mutant as unviable. A test process that reaches it aborts on a failed
# allocation, which cargo-mutants counts as caught. Without prlimit (util-linux; macOS has
# none) the tests run uncapped, with a warning.
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
# rustup is installed, the bin/ of the toolchain pinned in rust-toolchain.toml goes first
# on PATH, so a Homebrew or distro `cargo` earlier on PATH cannot silently replace it:
# cargo-mutants runs `cargo build` and `cargo test` through PATH for every mutant.
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
# viable; the 28 survivors are unused helpers or unobservable branches; the nightly runs
# count 11 of them unviable, so 94.0% of 284 since 2026-10-22), codegen 95.5%
# (233 of 244; 11 survivors, dead branches given the MIR the builder produces). Types,
# recorded 2026-10-21: 98.5% (407 of 413; the 6 survivors are the equivalent mutants listed
# in docs/TESTING.md). The 100% recorded for types on 2026-10-19 did not reproduce: the
# first nightly run measured 66.9%, see docs/PLAN.md. Re-measured 2026-10-31 after the
# tuple survivors got tests: borrowck 94.9% (298 of 314), codegen 95.6% (494 of 517),
# types 98.8% (570 of 577); see docs/TESTING.md. Codegen, nightly run 20 (2026-11-06, the
# first with an explicit test timeout): 95.2% (640 of 672), 96.3% (647) once the seven
# survivors that change the IL got tests; the other 25 are equivalent, and 95 keeps room
# for eight new survivors. Run 26 (2026-11-11): 94.2% with 42 survivors; tests, dead-code
# removal and argued exclusions leave none of them (#142, docs/TESTING.md).
minimum_score() {
    case "$1" in
        sabiko_borrowck) echo 93 ;;
        sabiko_codegen) echo 95 ;;
        sabiko_types) echo 98 ;;
    esac
}
# Seconds, per mutant's test run. The slowest test packages, codegen's (sabikoc's suites
# compile and run programs with both compilers), take about 2.5 minutes at
# --test-threads=2 on a loaded four-core machine and less on an idle runner, so a slow but
# passing suite is not mistaken for a hang. A real hang costs the full limit.
test_timeout() {
    if [ -n "${MUTANTS_TEST_TIMEOUT:-}" ]; then
        echo "$MUTANTS_TEST_TIMEOUT"
        return
    fi
    case "$1" in
        sabiko_borrowck | sabiko_codegen | sabiko_types) echo 300 ;;
    esac
}

# Gibibytes of address space per test process (see the header). Measured 2026-11-17 on the
# unmutated tree: the largest Rust test binary peaks at 1.8 GiB of address space (under
# 0.1 GiB resident), a rustc that a differential test starts at 0.9 GiB; dotnet reserves
# 54 GiB uncapped and sizes that to the cap (6.8 GiB under 8). All suites of the three
# crates pass under 8 GiB, and a runaway stops at half a 16 GB runner's memory.
memory_limit_gib() { echo "${MUTANTS_MEMORY_LIMIT_GIB:-8}"; }

# count <file>: its number of lines, 0 when it does not exist.
count() { if [ -f "$1" ]; then grep -c . "$1" || true; else echo 0; fi; }

# list <outcome> <n> <mutants.out dir>...: prints the mutants of that outcome (missed,
# timeout) of every directory, when there are any.
list() {
    local outcome=$1 n=$2
    shift 2
    if [ "$n" -gt 0 ]; then
        echo "-- $outcome:"
        for dir in "$@"; do
            if [ -f "$dir/$outcome.txt" ]; then sed 's/^/   /' "$dir/$outcome.txt"; fi
        done
    fi
}

# progress <mutants.out dir>: prints "<tested> <listed>", the number of mutants with an
# outcome (outcomes.json's total_mutants) and the number cargo-mutants was given
# (mutants.json), or "? ?" when either file is missing or unreadable. cargo-mutants writes
# no outcomes.json when it was given no mutants.
progress() {
    python3 -I -c '
import json, os, sys
try:
    with open(os.path.join(sys.argv[1], "mutants.json")) as f:
        listed = len(json.load(f))
    outcomes = os.path.join(sys.argv[1], "outcomes.json")
    if listed == 0 and not os.path.exists(outcomes):
        tested = 0
    else:
        with open(outcomes) as f:
            tested = json.load(f)["total_mutants"]
    print(tested, listed)
except (OSError, ValueError, KeyError, TypeError):
    print("? ?")
' "$1"
}

# complete <label> <mutants.out dir>: succeeds when every mutant cargo-mutants was given
# has an outcome; otherwise says how far it got, on stderr.
complete() {
    local label=$1 dir=$2 tested listed
    read -r tested listed <<<"$(progress "$dir")"
    if [ "$tested" = "?" ]; then
        echo "mutants.sh: $label: no readable outcomes.json and mutants.json:" \
            "not a mutants.out" >&2
        return 1
    fi
    if [ "$tested" -lt "$listed" ]; then
        echo "mutants.sh: $label finished $tested of $listed mutants: cancelled at the" \
            "job's time limit, interrupted, its runner died, or the baseline failed" >&2
        return 1
    fi
}

# shard_label <mutants.out dir>: "shard k (<dir>)" for the nightly's shard directories
# (mutants-<crate>-<k>), else the directory.
shard_label() {
    if [[ "$1" =~ -([0-9]+)/?$ ]]; then
        echo "shard ${BASH_REMATCH[1]} ($1)"
    else
        echo "$1"
    fi
}

# report <crate> <label> <elapsed seconds> <judge> <mutants.out dir>...: prints the summed
# counts of the directories, and the missed and the timed-out mutants. With judge=yes, sets
# status=1 when the score is below the crate's minimum; otherwise judge is the reason the
# counts are not judged. Appends the line to summary.
report() {
    local crate=$1 label=$2 elapsed=$3 judge=$4
    shift 4
    local caught=0 missed=0 timeout=0 unviable=0 dir
    for dir in "$@"; do
        caught=$((caught + $(count "$dir/caught.txt")))
        missed=$((missed + $(count "$dir/missed.txt")))
        timeout=$((timeout + $(count "$dir/timeout.txt")))
        unviable=$((unviable + $(count "$dir/unviable.txt")))
    done
    local total=$((caught + missed + timeout + unviable))
    local viable=$((caught + missed + timeout))
    local score
    if [ "$viable" -eq 0 ]; then
        score=0
    else
        # One decimal, in tenths of a percent.
        score=$(( (caught + timeout) * 1000 / viable ))
    fi
    local minimum verdict
    minimum=$(minimum_score "$crate")
    if [ "$judge" = yes ]; then
        verdict=ok
        if [ "$score" -lt $((minimum * 10)) ]; then
            verdict="BELOW MINIMUM"
            status=1
        fi
    else
        verdict="not judged ($judge)"
    fi
    local line
    line=$(printf '%-16s %4d mutants: %4d caught, %4d missed, %3d timeout, %3d unviable; score %d.%d%% (minimum %d%%) %s' \
        "$label" "$total" "$caught" "$missed" "$timeout" "$unviable" \
        $((score / 10)) $((score % 10)) "$minimum" "$verdict")
    if [ -n "$elapsed" ]; then
        line=$(printf '%s, %dm%02ds' "$line" $((elapsed / 60)) $((elapsed % 60)))
    fi
    summary+=("$line")
    echo "$line"
    # A timeout counts as caught, but a slow suite can be mistaken for a hang: list them
    # so a run's timeouts can be checked too.
    list missed "$missed" "$@"
    list timeout "$timeout" "$@"
}

status=0
summary=()

if [ "${1:-}" = "--score" ]; then
    if [ $# -lt 3 ]; then
        echo "usage: scripts/mutants.sh --score <crate> [--shards n] <mutants.out dir>..." >&2
        exit 1
    fi
    crate=$2
    shift 2
    if [ -z "$(minimum_score "$crate")" ]; then
        echo "mutants.sh: no minimum score configured for $crate" >&2
        exit 1
    fi
    expected=
    if [ "${1:-}" = "--shards" ]; then
        expected=$2
        shift 2
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        echo "mutants.sh: python3 is needed to read outcomes.json and mutants.json" >&2
        exit 1
    fi
    # Every problem is reported before failing, with the counts of what did finish.
    problems=0
    for dir in "$@"; do
        if ! complete "$(shard_label "$dir")" "$dir"; then
            problems=$((problems + 1))
        fi
    done
    if [ -n "$expected" ] && [ $# -ne "$expected" ]; then
        echo "mutants.sh: $crate has $# of $expected shard results; a shard job failed or" \
            "timed out before uploading its mutants.out" >&2
        problems=$((problems + 1))
    fi
    echo "== mutation score of $crate over $# result director$([ $# -eq 1 ] && echo y || echo ies)"
    if [ "$problems" -gt 0 ]; then
        report "$crate" "$crate" "" "incomplete results" "$@"
        echo "mutants.sh: $crate is not judged on incomplete results (see above)" >&2
        exit 1
    fi
    report "$crate" "$crate" "" yes "$@"
    exit "$status"
fi

shard=
if [ "${1:-}" = "--shard" ]; then
    shard=$2
    shift 2
    if ! [[ "$shard" =~ ^[0-9]+/[0-9]+$ ]]; then
        echo "mutants.sh: --shard takes k/n with k from 0, e.g. 0/4" >&2
        exit 1
    fi
fi

crates=("$@")
if [ ${#crates[@]} -eq 0 ]; then
    crates=(sabiko_borrowck sabiko_codegen sabiko_types)
fi

channel=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
if command -v rustup >/dev/null 2>&1; then
    if ! pinned_cargo=$(rustup which --toolchain "$channel" cargo 2>/dev/null); then
        echo "mutants.sh: the pinned toolchain is missing: rustup toolchain install $channel" >&2
        exit 1
    fi
    PATH="$(dirname "$pinned_cargo"):$PATH"
    export PATH
elif ! cargo --version | grep -q "^cargo $channel "; then
    echo "mutants.sh: warning: $(cargo --version) is not the pinned $channel" \
        "(rust-toolchain.toml); install rustup to get the pinned toolchain" >&2
fi
if ! cargo mutants --version >/dev/null 2>&1; then
    echo "cargo-mutants is not installed: cargo install cargo-mutants --locked --version 27.1.0" >&2
    exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "mutants.sh: python3 is needed to read outcomes.json and mutants.json" >&2
    exit 1
fi

# The memory cap (see the header): cargo runs each test binary through the host target's
# runner, and the binary's children inherit prlimit's limit.
limit_gib=$(memory_limit_gib)
if [ "$limit_gib" != 0 ]; then
    if command -v prlimit >/dev/null 2>&1; then
        host=$(rustc -vV | sed -n 's/^host: //p')
        runner_var="CARGO_TARGET_$(printf '%s' "$host" | tr 'a-z.-' 'A-Z__')_RUNNER"
        export "$runner_var=prlimit --as=$((limit_gib << 30)) --"
        echo "== each test process capped at $limit_gib GiB of address space ($runner_var)"
    else
        echo "mutants.sh: warning: prlimit (util-linux) not found: the tests run without a" \
            "memory cap, so a runaway mutant can exhaust the machine's memory" >&2
    fi
fi

# shellcheck disable=SC2206
extra_args=(${MUTANTS_ARGS:-})
if [ -n "$shard" ]; then
    # Round-robin spreads each source file over every shard, so shards take similar times.
    extra_args+=(--shard "$shard" --sharding round-robin)
fi
for crate in "${crates[@]}"; do
    packages=$(test_packages "$crate")
    if [ -z "$packages" ]; then
        echo "mutants.sh: no test packages configured for $crate" >&2
        exit 1
    fi
    timeout=$(test_timeout "$crate")
    out="target/mutants/$crate"
    mkdir -p "$out"
    echo "== cargo mutants -p $crate (tests: $packages; test timeout ${timeout}s${shard:+; shard $shard})"
    # The exclusions of .cargo/mutants.toml match names (file:line:column, mutation,
    # function), so an edit can change what they exclude: list what they exclude now, to
    # compare between runs (listing takes about a second).
    cargo mutants --list --no-config -p "$crate" | sort >"$out/all.txt"
    cargo mutants --list -p "$crate" | sort >"$out/listed.txt"
    comm -23 "$out/all.txt" "$out/listed.txt" >"$out/excluded.txt"
    echo "-- $(count "$out/excluded.txt") of $(count "$out/all.txt") mutants excluded by .cargo/mutants.toml:"
    sed 's/^/   /' "$out/excluded.txt"
    start=$(date +%s)
    # Exit codes 2 (missed mutants) and 3 (timeouts) are outcomes, not failures; 1 is an
    # error or an interruption, 4 a failing baseline (no mutant tested). Until #165 the
    # script took 4 for "unviable mutants" and passed a shard whose baseline failed.
    # The build timeout is fixed rather than derived from the baseline: with a warm
    # target/ the baseline builds in no time and the derived limit kills every mutant build.
    rc=0
    cargo mutants --in-place --build-timeout 900 --timeout "$timeout" -p "$crate" \
        --test-package "$packages" -o "$out" --caught --unviable \
        ${extra_args[@]+"${extra_args[@]}"} || rc=$?
    elapsed=$(( $(date +%s) - start ))
    judge=yes
    if [ -n "$shard" ]; then
        judge="one shard"
    fi
    finished=yes
    if ! complete "$crate${shard:+ shard $shard}" "$out/mutants.out"; then
        finished=no
        judge=incomplete
    fi
    # The counts of what finished, also after an interruption.
    if [ -f "$out/mutants.out/outcomes.json" ]; then
        report "$crate" "$crate${shard:+ $shard}" "$elapsed" "$judge" "$out/mutants.out"
    fi
    if [ "$rc" -ne 0 ] && [ "$rc" -ne 2 ] && [ "$rc" -ne 3 ]; then
        echo "mutants.sh: cargo mutants failed for $crate (exit $rc)" >&2
        exit "$rc"
    fi
    if [ "$finished" = no ]; then
        status=1
    fi
done

echo
echo "== mutation testing summary"
printf '%s\n' "${summary[@]}"
exit "$status"
