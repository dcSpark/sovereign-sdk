#!/usr/bin/env bash
set -euo pipefail

##
# Standalone macOS (arm64) portable build script.
#
# This script:
# - Ensures Homebrew + required formulae
# - Clones and builds:
#     - Dawn (pinned commit)
#     - wabt
#     - nicarq/ligero-prover (branch)
# - Stages:
#     <out>/macos-arm64/{bin,lib}/
# - Produces a tarball by default.
#
# Usage:
#   macos-build-and-stage.sh [--out <dir>] [--no-tar]
#
# Output:
#   <out>/macos-arm64/...
#   <out>/ligero-macos-arm64.tar.gz   (unless --no-tar)

usage() {
  cat <<'EOF'
Usage: macos-build-and-stage.sh [--out <dir>] [--no-tar]

Options:
  --out <dir>   Output directory (default: current directory)
  --no-tar      Only stage `<out>/macos-arm64`; do not create tarball

Environment:
  CMAKE_JOB_COUNT   Parallel build jobs
  DAWN_GIT_REF      Dawn commit (default: cec4482eccee45696a7c0019e750c77f101ced04)
  LIGERO_REPO       Ligero prover git URL (default: https://github.com/nicarq/ligero-prover.git)
  LIGERO_BRANCH     Ligero prover git branch (default: nico/improvements)
EOF
}

OUT_DIR="$PWD"
NO_TAR=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out)
      shift
      [[ $# -gt 0 ]] || { echo "error: --out expects a path" >&2; usage; exit 1; }
      OUT_DIR="$1"
      shift
      ;;
    --no-tar)
      NO_TAR=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown arg '$1'" >&2
      usage
      exit 1
      ;;
  esac
done

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
  echo "error: must run on macOS arm64" >&2
  exit 1
fi

DAWN_GIT_REF="${DAWN_GIT_REF:-cec4482eccee45696a7c0019e750c77f101ced04}"
LIGERO_REPO="${LIGERO_REPO:-https://github.com/nicarq/ligero-prover.git}"
LIGERO_BRANCH="${LIGERO_BRANCH:-nico/improvements}"

JOBS="${CMAKE_JOB_COUNT:-}"
if [[ -z "$JOBS" ]]; then
  JOBS="$(sysctl -n hw.ncpu 2>/dev/null || echo 8)"
fi

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

if ! command -v brew >/dev/null 2>&1; then
  echo "error: Homebrew is required but not found." >&2
  echo "Install it with:" >&2
  echo '  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"' >&2
  exit 1
fi

echo "==> Ensuring required Homebrew formulae..."
REQUIRED_FORMULAE=(cmake gmp mpfr libomp llvm boost)
for f in "${REQUIRED_FORMULAE[@]}"; do
  if ! brew list --formula "$f" >/dev/null 2>&1; then
    echo "==> Installing $f..."
    brew install "$f"
  fi
done

# IMPORTANT:
# Homebrew LLVM uses its own libc++ headers which can mismatch the system libc++ at link-time,
# causing errors like: `Undefined symbols ... std::__1::__hash_memory`.
# Default to AppleClang (matches system libc++). Opt into Homebrew LLVM via USE_HOMEBREW_LLVM=1.
if [[ "${USE_HOMEBREW_LLVM:-0}" == "1" ]]; then
  LLVM_PREFIX="$(brew --prefix llvm)"
  export CC="${CC:-$LLVM_PREFIX/bin/clang}"
  export CXX="${CXX:-$LLVM_PREFIX/bin/clang++}"
else
  export CC="${CC:-$(xcrun --find clang)}"
  export CXX="${CXX:-$(xcrun --find clang++)}"
fi

if [[ ! -x "$CC" || ! -x "$CXX" ]]; then
  echo "error: C/C++ compilers not found (CC='$CC', CXX='$CXX')" >&2
  exit 1
fi

TMP_ROOT="$(mktemp -d -t ligero-macos-build.XXXXXX)"
cleanup() {
  rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

DAWN_SRC="$TMP_ROOT/dawn"
WABT_SRC="$TMP_ROOT/wabt"
LIGERO_SRC="$TMP_ROOT/ligero-prover"

SYSROOT="$TMP_ROOT/sysroot"

DAWN_BUILD="$TMP_ROOT/dawn-build"
WABT_BUILD="$TMP_ROOT/wabt-build"
LIGERO_BUILD="$TMP_ROOT/ligero-build"

mkdir -p "$SYSROOT" "$DAWN_BUILD" "$WABT_BUILD" "$LIGERO_BUILD"

echo "==> Cloning Dawn..."
git clone https://dawn.googlesource.com/dawn "$DAWN_SRC"
cd "$DAWN_SRC"
git checkout "$DAWN_GIT_REF"

echo "==> Building Dawn..."
cmake -S "$DAWN_SRC" -B "$DAWN_BUILD" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER="$CC" \
  -DCMAKE_CXX_COMPILER="$CXX" \
  -DDAWN_FETCH_DEPENDENCIES=ON \
  -DDAWN_ENABLE_VULKAN=ON \
  -DDAWN_BUILD_MONOLITHIC_LIBRARY=STATIC \
  -DDAWN_BUILD_TESTS=OFF \
  -DDAWN_BUILD_SAMPLES=OFF \
  -DDAWN_BUILD_PROTOBUF=OFF \
  -DTINT_BUILD_TESTS=OFF \
  -DTINT_BUILD_CMD_TOOLS=OFF \
  -DTINT_BUILD_FUZZERS=OFF \
  -DTINT_BUILD_BENCHMARKS=OFF \
  -DTINT_BUILD_TINTD=OFF \
  -DTINT_BUILD_IR_BINARY=OFF \
  -DDAWN_ENABLE_INSTALL=ON \
  -DCMAKE_INSTALL_PREFIX="$SYSROOT"
cmake --build "$DAWN_BUILD" --parallel "$JOBS"
cmake --install "$DAWN_BUILD"

echo "==> Cloning wabt..."
git clone https://github.com/WebAssembly/wabt.git "$WABT_SRC"
cd "$WABT_SRC"
git submodule update --init

echo "==> Building wabt..."
cmake -S "$WABT_SRC" -B "$WABT_BUILD" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER="$CC" \
  -DCMAKE_CXX_COMPILER="$CXX" \
  -DBUILD_TESTS=OFF \
  -DCMAKE_INSTALL_PREFIX="$SYSROOT"
cmake --build "$WABT_BUILD" --parallel "$JOBS"
cmake --install "$WABT_BUILD"

echo "==> Cloning ligero-prover..."
git clone "$LIGERO_REPO" -b "$LIGERO_BRANCH" "$LIGERO_SRC"

echo "==> Patching ligero-prover for wabt compatibility..."
TRANSPILER_HPP="$LIGERO_SRC/include/transpiler.hpp"
if [[ -f "$TRANSPILER_HPP" ]] && ! grep -q "transpile_wabt_type(const wabt::Var" "$TRANSPILER_HPP"; then
  # wabt newer API uses wabt::Var for ref.null type (Var::to_type()).
  perl -0777 -i -pe 's/\}\n\n\/\/ ------------------------------------------------------------/\}\n\n\/\/ Newer wabt represents ref-null types as `wabt::Var` (which may carry an optional type).\n\/\/ Provide an overload so we can support both wabt APIs without pinning a specific version.\nvalue_kind transpile_wabt_type(const wabt::Var& var) {\n    return transpile_wabt_type(var.to_type());\n}\n\n\/\/ ------------------------------------------------------------/s' "$TRANSPILER_HPP"
fi

echo "==> Building ligero-prover..."
BREW_PREFIX="$(brew --prefix)"
cmake -S "$LIGERO_SRC" -B "$LIGERO_BUILD" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER="$CC" \
  -DCMAKE_CXX_COMPILER="$CXX" \
  -DCMAKE_PREFIX_PATH="$SYSROOT;$BREW_PREFIX"
cmake --build "$LIGERO_BUILD" --target webgpu_prover --parallel "$JOBS"
cmake --build "$LIGERO_BUILD" --target webgpu_verifier --parallel "$JOBS"

STAGE_ROOT="$OUT_DIR"
BIN_STAGE="$STAGE_ROOT/macos-arm64"

rm -rf "$BIN_STAGE"
mkdir -p "$BIN_STAGE/bin" "$BIN_STAGE/lib"

echo "==> Staging shader folder..."
mkdir -p "$STAGE_ROOT/shader"
if ! find "$STAGE_ROOT/shader" -mindepth 1 -maxdepth 1 | read -r _; then
  cp -R "$LIGERO_SRC/shader/." "$STAGE_ROOT/shader/"
fi

echo "==> Staging binaries..."
install -m 0755 "$LIGERO_BUILD/webgpu_prover" "$BIN_STAGE/bin/webgpu_prover"
install -m 0755 "$LIGERO_BUILD/webgpu_verifier" "$BIN_STAGE/bin/webgpu_verifier"

# Prefer bundled libs.
install_name_tool -add_rpath "@executable_path/../lib" "$BIN_STAGE/bin/webgpu_prover" || true
install_name_tool -add_rpath "@executable_path/../lib" "$BIN_STAGE/bin/webgpu_verifier" || true

is_system_dylib() {
  case "$1" in
    /usr/lib/*|/System/*) return 0 ;;
    *) return 1 ;;
  esac
}

deps_for() {
  # Print dependency paths from otool -L output, excluding the first line (the binary itself)
  otool -L "$1" | tail -n +2 | awk '{print $1}'
}

copy_dylib() {
  local p="$1"
  [[ -f "$p" ]] || return 0
  local base
  base="$(basename "$p")"
  if [[ ! -f "$BIN_STAGE/lib/$base" ]]; then
    cp -L "$p" "$BIN_STAGE/lib/$base"
    chmod 0644 "$BIN_STAGE/lib/$base" || true
    # Make the dylib itself load relative deps
    install_name_tool -id "@rpath/$base" "$BIN_STAGE/lib/$base" || true
    install_name_tool -add_rpath "@loader_path" "$BIN_STAGE/lib/$base" || true
  fi
}

rewrite_dep() {
  local target="$1"
  local old="$2"
  local base
  base="$(basename "$old")"
  install_name_tool -change "$old" "@rpath/$base" "$target" || true
}

echo "==> Collecting dylibs..."
for bin in "$BIN_STAGE/bin/webgpu_prover" "$BIN_STAGE/bin/webgpu_verifier"; do
  while read -r dep; do
    [[ -n "$dep" ]] || continue
    if is_system_dylib "$dep"; then
      continue
    fi
    copy_dylib "$dep"
    rewrite_dep "$bin" "$dep"
  done < <(deps_for "$bin")
done

# Iterate on copied dylibs too (best-effort)
for _ in 1 2 3; do
  for lib in "$BIN_STAGE/lib/"*.dylib; do
    [[ -f "$lib" ]] || continue
    while read -r dep; do
      [[ -n "$dep" ]] || continue
      if is_system_dylib "$dep"; then
        continue
      fi
      copy_dylib "$dep"
      rewrite_dep "$lib" "$dep"
    done < <(deps_for "$lib")
  done
done

if [[ "$NO_TAR" == "false" ]]; then
  TARBALL="$OUT_DIR/ligero-macos-arm64.tar.gz"
  rm -f "$TARBALL"
  echo "==> Creating tarball: $TARBALL"
  (cd "$OUT_DIR" && tar -czf "$(basename "$TARBALL")" "macos-arm64")
fi

echo "==> Done."
echo "    Folder: $BIN_STAGE"
if [[ "$NO_TAR" == "false" ]]; then
  echo "    Tarball: $TARBALL"
fi


