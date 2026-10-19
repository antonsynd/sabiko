#!/usr/bin/env bash
# Build the Sabiko runtime library: runtime/Sabiko.Runtime/*.cs -> tools/runtime/Sabiko.Runtime.dll
# with the Roslyn csc installed by scripts/setup-dotnet.sh (no .NET SDK needed), then
# verify the result with ILVerify. Re-run after editing the C# sources.
# Usage: scripts/build-runtime.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLS="$ROOT/tools"
SRC="$ROOT/runtime/Sabiko.Runtime"
OUT="$TOOLS/runtime"

[[ -x "$TOOLS/bin/csc" && -d "$TOOLS/ref" ]] || { echo "toolchain missing; run scripts/setup-dotnet.sh first" >&2; exit 1; }

log() { printf '\033[1;34m[build-runtime]\033[0m %s\n' "$*" >&2; }

mkdir -p "$OUT"
# Response file: one reference per line for every net10.0 reference assembly.
rsp="$OUT/Sabiko.Runtime.rsp"
{
  for dll in "$TOOLS"/ref/*.dll; do printf -- '-r:"%s"\n' "$dll"; done
  for cs in "$SRC"/*.cs; do printf '"%s"\n' "$cs"; done
} > "$rsp"

log "compiling $SRC -> $OUT/Sabiko.Runtime.dll"
"$TOOLS/bin/csc" -nologo -noconfig -nostdlib -target:library -optimize+ -deterministic \
  -nullable:enable -langversion:latest -warnaserror \
  -out:"$OUT/Sabiko.Runtime.dll" -doc:"$OUT/Sabiko.Runtime.xml" "@$rsp"

log "verifying"
if "$TOOLS/bin/ilverify" "$OUT/Sabiko.Runtime.dll" > "$OUT/ilverify.log"; then
  log "ilverify: ok"
else
  cat "$OUT/ilverify.log" >&2
  echo "build-runtime: Sabiko.Runtime.dll does not verify" >&2
  exit 1
fi
