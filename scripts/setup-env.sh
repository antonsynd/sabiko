#!/usr/bin/env bash
# One-shot development environment for Sabiko (Linux/macOS):
#   1. Rust: rustup (installed if missing) and the toolchain pinned in rust-toolchain.toml,
#      with its components (rustfmt, clippy).
#   2. cargo-deny at the version CI uses (skip with --no-cargo-deny).
#   3. .NET: the latest patch of the chosen major version (10, what CI runs, by default;
#      --dotnet 11 for the newer runtime), then scripts/setup-dotnet.sh at that version, which fetches
#      ilasm/ildasm, ILVerify, the reference assemblies and csc into ./tools and builds
#      tools/runtime/Sabiko.Runtime.dll.
#   4. With --check: cargo build and cargo test --workspace with SABIKO_REQUIRE_DOTNET=1.
#
# Usage: scripts/setup-env.sh [--dotnet 10|11] [--dotnet-version X.Y.Z] [--no-cargo-deny]
#                             [--profile] [--check] [--force]
#
#   --dotnet N           .NET major version for ./tools (default 10, as CI). With 11, sabikoc's
#                        net10.0 output runs on the .NET 11 runtime (tools/env.sh sets
#                        DOTNET_ROLL_FORWARD=Major), but ILVerify 11 rejects the `valuetype X`
#                        TypeSpecs sabikoc emits (BadTypeSpec), so the test suite fails there
#                        until codegen emits plain type tokens; then 11 becomes the default.
#   --dotnet-version V   exact patch version instead of the newest one on NuGet.
#   --no-cargo-deny      skip `cargo install cargo-deny` (a few minutes of compiling).
#   --profile            append `source <repo>/tools/env.sh` to ~/.bashrc (or ~/.zshrc).
#   --check              build and run the test suite afterwards.
#   --force              re-download the .NET toolchain (passed to setup-dotnet.sh).
#
# Afterwards: `source tools/env.sh` (and `. "$HOME/.cargo/env"` if rustup was just installed).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO_DENY_VERSION="0.20.2"   # keep in sync with CLAUDE.md and .github/workflows/ci.yml
NUGET="https://api.nuget.org/v3-flatcontainer"

DOTNET_MAJOR=10; DOTNET_VERSION=""; CARGO_DENY=1; PROFILE=0; CHECK=0; FORCE=()
while (($#)); do
  case "$1" in
    --dotnet) DOTNET_MAJOR="${2:?--dotnet needs 10 or 11}"; shift ;;
    --dotnet=*) DOTNET_MAJOR="${1#*=}" ;;
    --dotnet-version) DOTNET_VERSION="${2:?--dotnet-version needs a version}"; shift ;;
    --dotnet-version=*) DOTNET_VERSION="${1#*=}" ;;
    --no-cargo-deny) CARGO_DENY=0 ;;
    --profile) PROFILE=1 ;;
    --check) CHECK=1 ;;
    --force) FORCE=(--force) ;;
    -h|--help) sed -n '2,27p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
  esac
  shift
done
[[ "$DOTNET_MAJOR" =~ ^1[01]$ ]] || { echo "--dotnet must be 10 or 11, got '$DOTNET_MAJOR'" >&2; exit 2; }

log() { printf '\033[1;34m[setup-env]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[setup-env]\033[0m %s\n' "$*" >&2; exit 1; }

for tool in curl unzip; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required (e.g. apt-get install -y $tool)"
done
command -v cc >/dev/null 2>&1 || log "warning: no C compiler (cc) on PATH; cargo needs one to link (apt-get install -y build-essential, or xcode-select --install)"

# 1. Rust ---------------------------------------------------------------------
[[ -f "$HOME/.cargo/env" ]] && . "$HOME/.cargo/env"
if ! command -v rustup >/dev/null 2>&1; then
  log "installing rustup"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none
  . "$HOME/.cargo/env"
fi
cd "$ROOT"
# `rustup show active-toolchain` installs the toolchain named in rust-toolchain.toml (and the
# components it lists) on rustup >= 1.28; the explicit install covers older rustups.
TOOLCHAIN="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)"
log "Rust toolchain $TOOLCHAIN (from rust-toolchain.toml)"
rustup toolchain install "$TOOLCHAIN" --profile minimal --component rustfmt --component clippy
rustup show active-toolchain >/dev/null
rustc --version

# 2. cargo-deny ---------------------------------------------------------------
if ((CARGO_DENY)); then
  if cargo deny --version 2>/dev/null | grep -q " $CARGO_DENY_VERSION\$"; then
    log "cargo-deny $CARGO_DENY_VERSION already installed"
  else
    log "installing cargo-deny $CARGO_DENY_VERSION"
    cargo install cargo-deny --locked --version "$CARGO_DENY_VERSION"
  fi
fi

# 3. .NET ---------------------------------------------------------------------
if [[ -z "$DOTNET_VERSION" ]]; then
  # Newest stable X.Y.Z of the reference pack; the runtime, ilasm and ILVerify packages ship
  # in lockstep with it.
  DOTNET_VERSION="$(curl -sSfL --retry 3 "$NUGET/microsoft.netcore.app.ref/index.json" \
    | tr -d ' \n' | grep -o '"[0-9.]*"' | tr -d '"' | grep -E "^$DOTNET_MAJOR\.0\.[0-9]+\$" \
    | sort -t. -k3,3n | tail -1)" || true
  [[ -n "$DOTNET_VERSION" ]] || die "could not find a .NET $DOTNET_MAJOR release on NuGet; pass --dotnet-version"
fi
log ".NET $DOTNET_VERSION"
SABIKO_DOTNET_VERSION="$DOTNET_VERSION" "$ROOT/scripts/setup-dotnet.sh" "${FORCE[@]}"
# shellcheck disable=SC1091
. "$ROOT/tools/env.sh"

# 4. shell profile --------------------------------------------------------------
if ((PROFILE)); then
  rc="$HOME/.bashrc"; [[ "${SHELL:-}" == */zsh ]] && rc="$HOME/.zshrc"
  line="[ -f \"$ROOT/tools/env.sh\" ] && . \"$ROOT/tools/env.sh\"  # Sabiko"
  if grep -qF "$ROOT/tools/env.sh" "$rc" 2>/dev/null; then
    log "$rc already sources tools/env.sh"
  else
    printf '\n%s\n' "$line" >> "$rc"; log "added tools/env.sh to $rc"
  fi
fi

# 5. optional check --------------------------------------------------------------
if ((CHECK)); then
  log "cargo build --workspace"
  cargo build --workspace
  log "cargo test --workspace (SABIKO_REQUIRE_DOTNET=1)"
  SABIKO_REQUIRE_DOTNET=1 cargo test --workspace
fi

log "done: rustc $(rustc --version | cut -d' ' -f2), .NET $DOTNET_VERSION in $DOTNET_ROOT"
log "run: source tools/env.sh"
