#!/usr/bin/env bash
# Install the .NET toolchain Sabiko needs into ./tools:
#   - a .NET 10 runtime (`dotnet` muxer + shared framework)
#   - ilasm / ildasm native binaries
#   - ILVerify (dotnet-ilverify) and the net10.0 reference assemblies it needs
#
# Usage: scripts/setup-dotnet.sh [--from-nuget] [--force]
#
# By default an existing `dotnet` with a 10.x runtime on PATH is reused and only
# ilasm/ildasm/ILVerify are fetched. Otherwise the official dotnet-install script
# is tried, and if that host is unreachable (some sandboxes block dot.net) the
# runtime is assembled from NuGet packages instead (--from-nuget forces this).
#
# Afterwards: `source tools/env.sh`
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TOOLS="$ROOT/tools"
CACHE="$TOOLS/.cache"
DOTNET_VERSION="${SABIKO_DOTNET_VERSION:-10.0.13}"   # runtime / ilasm / ildasm / ilverify / ref pack
MUXER_VERSION="${SABIKO_MUXER_VERSION:-8.0.32}"      # last version of the standalone muxer package; it is version-agnostic
NUGET="https://api.nuget.org/v3-flatcontainer"

FROM_NUGET=0; FORCE=0
for a in "$@"; do
  case "$a" in
    --from-nuget) FROM_NUGET=1 ;;
    --force) FORCE=1 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown argument: $a" >&2; exit 2 ;;
  esac
done

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  RID=linux-x64 ;;
  Linux-aarch64) RID=linux-arm64 ;;
  Darwin-x86_64) RID=osx-x64 ;;
  Darwin-arm64)  RID=osx-arm64 ;;
  *) echo "unsupported platform $(uname -s)-$(uname -m); use scripts/setup-dotnet.ps1 on Windows" >&2; exit 1 ;;
esac

mkdir -p "$TOOLS/bin" "$CACHE"

log() { printf '\033[1;34m[setup-dotnet]\033[0m %s\n' "$*" >&2; }

# fetch_pkg <id> <version>  -> prints path of the .nupkg in the cache
fetch_pkg() {
  local id="$1" ver="$2" out="$CACHE/$1.$2.nupkg"
  if [[ ! -s "$out" || $FORCE == 1 ]]; then
    log "downloading $id $ver"
    curl -sSfL --retry 3 --max-time 600 -o "$out" "$NUGET/$id/$ver/$id.$ver.nupkg"
  fi
  printf '%s' "$out"
}

# extract <nupkg> <pattern-in-zip> <dest-dir>   (strips the directory part of the pattern)
extract() {
  local pkg="$1" pat="$2" dest="$3"
  mkdir -p "$dest"
  unzip -qo -j "$pkg" "$pat" -d "$dest"
}

have_dotnet10() {
  command -v "$1" >/dev/null 2>&1 && "$1" --list-runtimes 2>/dev/null | grep -q '^Microsoft.NETCore.App 10\.'
}

install_runtime_from_nuget() {
  local v="$DOTNET_VERSION" d="$TOOLS/dotnet"
  log "assembling .NET $v runtime from NuGet packages into $d"
  rm -rf "$d"; mkdir -p "$d/host/fxr/$v" "$d/shared/Microsoft.NETCore.App/$v"
  local rt; rt="$(fetch_pkg "microsoft.netcore.app.runtime.$RID" "$v")"
  extract "$rt" "runtimes/$RID/lib/net10.0/*" "$d/shared/Microsoft.NETCore.App/$v"
  extract "$rt" "runtimes/$RID/native/*" "$d/shared/Microsoft.NETCore.App/$v"
  # libhostfxr lives beside the shared framework in the pack but the host expects it under host/fxr.
  mv "$d/shared/Microsoft.NETCore.App/$v"/libhostfxr.* "$d/host/fxr/$v/"
  # The framework needs a .version / .deps.json; the pack ships Microsoft.NETCore.App.deps.json in data/ for newer versions.
  if unzip -l "$rt" | grep -q 'Microsoft.NETCore.App.deps.json'; then
    extract "$rt" "*/Microsoft.NETCore.App.deps.json" "$d/shared/Microsoft.NETCore.App/$v"
  fi
  local mx; mx="$(fetch_pkg "runtime.$RID.microsoft.netcore.dotnethost" "$MUXER_VERSION")"
  extract "$mx" "runtimes/$RID/native/dotnet" "$d"
  chmod +x "$d/dotnet" "$d/shared/Microsoft.NETCore.App/$v"/createdump 2>/dev/null || true
  chmod +x "$d/dotnet"
}

install_runtime_official() {
  local d="$TOOLS/dotnet"
  log "trying official dotnet-install script"
  local script="$CACHE/dotnet-install.sh"
  curl -sSfL --max-time 60 -o "$script" https://dot.net/v1/dotnet-install.sh || return 1
  bash "$script" --runtime dotnet --channel 10.0 --install-dir "$d" || return 1
}

# 1. runtime -----------------------------------------------------------------
DOTNET_BIN=""
if [[ $FROM_NUGET == 0 && $FORCE == 0 ]] && have_dotnet10 dotnet; then
  DOTNET_BIN="$(command -v dotnet)"; log "using existing $DOTNET_BIN"
elif [[ $FROM_NUGET == 0 && $FORCE == 0 ]] && have_dotnet10 "$TOOLS/dotnet/dotnet"; then
  DOTNET_BIN="$TOOLS/dotnet/dotnet"; log "using existing $DOTNET_BIN"
else
  if [[ $FROM_NUGET == 1 ]] || ! install_runtime_official; then
    [[ $FROM_NUGET == 1 ]] || log "dotnet-install unreachable, falling back to NuGet"
    install_runtime_from_nuget
  fi
  DOTNET_BIN="$TOOLS/dotnet/dotnet"
fi
DOTNET_ROOT_DIR="$(cd "$(dirname "$DOTNET_BIN")" && pwd)"

# 2. ilasm / ildasm ----------------------------------------------------------
for t in ilasm ildasm; do
  p="$(fetch_pkg "runtime.$RID.microsoft.netcore.$t" "$DOTNET_VERSION")"
  extract "$p" "runtimes/$RID/native/$t" "$TOOLS/bin"
  chmod +x "$TOOLS/bin/$t"
done

# 3. ILVerify + reference assemblies ----------------------------------------
p="$(fetch_pkg dotnet-ilverify "$DOTNET_VERSION")"
rm -rf "$TOOLS/ilverify"; extract "$p" "tools/net10.0/any/*" "$TOOLS/ilverify"
p="$(fetch_pkg microsoft.netcore.app.ref "$DOTNET_VERSION")"
rm -rf "$TOOLS/ref"; extract "$p" "ref/net10.0/*.dll" "$TOOLS/ref"

cat > "$TOOLS/bin/ilverify" <<SH
#!/usr/bin/env bash
# ILVerify wrapper: verifies the given assemblies against the net10.0 reference assemblies.
exec "$DOTNET_BIN" "$TOOLS/ilverify/ILVerify.dll" -r "$TOOLS/ref/*.dll" "\$@"
SH
chmod +x "$TOOLS/bin/ilverify"

# 4. env file ------------------------------------------------------------------
cat > "$TOOLS/env.sh" <<SH
# source this file: sets up the Sabiko .NET toolchain
export DOTNET_ROOT="$DOTNET_ROOT_DIR"
export DOTNET_CLI_TELEMETRY_OPTOUT=1
export DOTNET_NOLOGO=1
export SABIKO_DOTNET="$DOTNET_BIN"
export SABIKO_REF_DIR="$TOOLS/ref"
export PATH="$TOOLS/bin:$DOTNET_ROOT_DIR:\$PATH"
SH

log "done"
"$DOTNET_BIN" --list-runtimes
"$TOOLS/bin/ilasm" 2>&1 | head -1 || true
log "run: source tools/env.sh"
