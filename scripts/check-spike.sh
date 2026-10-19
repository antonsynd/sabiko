#!/usr/bin/env bash
# Phase 0 gate: the hand-written IL spike must assemble, verify clean, run, and print
# exactly spike/expected.txt.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
actual="$("$ROOT/scripts/il-run.sh" "$ROOT/spike/hello.il")"
if diff <(printf '%s\n' "$actual") "$ROOT/spike/expected.txt"; then
  echo "spike: ok"
else
  echo "spike: output mismatch" >&2; exit 1
fi
