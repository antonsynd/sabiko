#!/bin/bash
# SessionStart hook for Claude Code cloud sessions: make sure the .NET toolchain
# (runtime, ilasm, ildasm, ILVerify) is available so the IL spike and, later, the
# run-pass test suites can execute. Idempotent; reuses ./tools when present.
set -euo pipefail

if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

cd "$CLAUDE_PROJECT_DIR"

if [ ! -x tools/bin/ilasm ] || [ ! -f tools/env.sh ]; then
  scripts/setup-dotnet.sh
fi

# Export the toolchain for the rest of the session.
if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
  cat tools/env.sh >> "$CLAUDE_ENV_FILE"
fi
