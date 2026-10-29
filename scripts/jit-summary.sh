#!/usr/bin/env bash
# Compile a Sabiko program and print what the JIT did with its methods: one line per
# compiled method with its tier (Tier0, Instrumented Tier0, Tier1, Tier1-OSR, ...), IL size
# and code size, from DOTNET_JitDisasmSummary. Diagnostic only: machine code and tiering
# change between runtime versions, so nothing should assert on this output.
#
# Usage: scripts/jit-summary.sh [--release] [--all] [--disasm METHOD] file.sabi [-- args]
#   --release        compile with --release (wrapping arithmetic)
#   --all            list every method the JIT compiled, the BCL and Sabiko.Runtime too
#   --disasm METHOD  also print the disassembly of METHOD (DOTNET_JitDisasm syntax, e.g.
#                    'FibRecursive' or 'Bench:*'), every tier it was compiled at
# The program's stdout is discarded; set SABIKOC to use a prebuilt sabikoc binary.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ -f "$ROOT/tools/env.sh" ]] && source "$ROOT/tools/env.sh"

release=(); all=0; disasm=""; src=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --release) release=(--release); shift;;
    --all) all=1; shift;;
    --disasm) disasm="$2"; shift 2;;
    --) shift; break;;
    -*) echo "jit-summary: unknown option $1" >&2; exit 2;;
    *) src="$1"; shift;;
  esac
done
[[ -n "$src" ]] || { sed -n '2,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }

out="$ROOT/target/jit-summary/$(basename "${src%.sabi}")"
mkdir -p "$out"
dll="$out/$(basename "${src%.sabi}").dll"
if [[ -n "${SABIKOC:-}" ]]; then
  "$SABIKOC" "$src" --emit=exe ${release[@]+"${release[@]}"} -o "$dll"
else
  cargo run -q --manifest-path "$ROOT/Cargo.toml" -p sabikoc -- "$src" --emit=exe \
    ${release[@]+"${release[@]}"} -o "$dll"
fi

log="$out/jit.txt"
rm -f "$log"
env DOTNET_JitDisasmSummary=1 DOTNET_JitStdOutFile="$log" \
  ${disasm:+DOTNET_JitDisasm="$disasm"} \
  "${SABIKO_DOTNET:-dotnet}" "$dll" "$@" >/dev/null || true
if [[ $all == 1 || -n "$disasm" ]]; then
  cat "$log"
else
  # The program's own methods live in a class without a namespace (`Bench:FibRecursive`).
  grep -E 'JIT compiled [^.: ]+:' "$log" | grep -v "<EntryPoint>" || true
fi
