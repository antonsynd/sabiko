#!/usr/bin/env bash
# Assemble a .il file with ilasm, verify it with ILVerify, run it with dotnet.
# Usage: scripts/il-run.sh <file.il> [--no-verify] [--no-run]
# Requires `source tools/env.sh` (or scripts/setup-dotnet.sh first).
# When tools/runtime/Sabiko.Runtime.dll exists it is passed to ILVerify as a reference and
# copied next to the output, so the .il may reference `[Sabiko.Runtime]`.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
[[ -f "$ROOT/tools/env.sh" ]] && source "$ROOT/tools/env.sh"
il="$1"; shift || true
VERIFY=1; RUN=1
for a in "$@"; do case "$a" in --no-verify) VERIFY=0;; --no-run) RUN=0;; esac; done
name="$(basename "${il%.il}")"
out="$ROOT/target/il/$name"; mkdir -p "$out"
runtime="${SABIKO_RUNTIME_DIR:-$ROOT/tools/runtime}/Sabiko.Runtime.dll"
refs=(); [[ -f "$runtime" ]] && { refs=(-r "$runtime"); cp "$runtime" "$out/"; }
ilasm -quiet -exe -output:"$out/$name.dll" "$il"
if [[ $VERIFY == 1 ]]; then
  if ilverify ${refs[@]+"${refs[@]}"} "$out/$name.dll" >"$out/ilverify.log"; then echo "ilverify: ok" >&2; else cat "$out/ilverify.log" >&2; exit 1; fi
fi
if [[ $RUN == 1 ]]; then
  cfg="$(dirname "$il")/$name.runtimeconfig.json"
  [[ -f "$cfg" ]] || cfg="$ROOT/spike/hello.runtimeconfig.json"
  cp "$cfg" "$out/$name.runtimeconfig.json"
  "$SABIKO_DOTNET" "$out/$name.dll"
fi
