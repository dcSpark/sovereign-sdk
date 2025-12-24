#!/usr/bin/env bash
set -euo pipefail

##
# Standalone Linux portable build script (intended to run in Docker).
#
# This script:
# - Installs build dependencies
# - Clones and builds:
#     - depot_tools + Dawn (pinned commit) via gclient sync + bundled clang
#     - wabt
#     - nicarq/ligero-prover (branch)
# - Stages:
#     <out>/<arch>/{bin,lib}/
# - Produces a tarball by default.
#
# Usage:
#   linux-build-and-stage.sh --arch <linux-amd64|linux-arm64> [--out <dir>] [--no-tar]
#
# Output:
#   <out>/<arch>/...
#   <out>/ligero-<arch>.tar.gz   (unless --no-tar)

usage() {
  cat <<'EOF'
Usage: linux-build-and-stage.sh --arch <linux-amd64|linux-arm64> [--out <dir>] [--no-tar]

Options:
  --arch <arch>   linux-amd64 or linux-arm64 (required)
  --out <dir>     Output directory (default: /out if exists, else current directory)
  --no-tar        Only stage `<out>/<arch>`; do not create tarball

Environment:
  CMAKE_JOB_COUNT  Parallel build jobs
  DAWN_GIT_REF      Dawn commit (default: cec4482eccee45696a7c0019e750c77f101ced04)
  LIGERO_REPO       Ligero prover git URL (default: https://github.com/nicarq/ligero-prover.git)
  LIGERO_GIT_REF    Ligero prover git ref (default: 74aee0b356cf80fcc1497aec9189fcb643377295)
  WABT_GIT_REF      WABT git ref (default: a55fb9466f2f886cf0c5bcadab97900f1e0a5789)
EOF
}

ARCH=""
OUT_DIR=""
NO_TAR=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch)
      shift
      [[ $# -gt 0 ]] || { echo "error: --arch expects a value" >&2; usage; exit 1; }
      ARCH="$1"
      shift
      ;;
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

if [[ -z "$ARCH" ]]; then
  echo "error: --arch is required" >&2
  usage
  exit 1
fi

case "$ARCH" in
  linux-amd64|linux-arm64) ;;
  *) echo "error: unsupported arch '$ARCH'" >&2; usage; exit 1 ;;
esac

if [[ -z "$OUT_DIR" ]]; then
  if [[ -d /out ]]; then
    OUT_DIR="/out"
  else
    OUT_DIR="$PWD"
  fi
fi

OUT_DIR="$(mkdir -p "$OUT_DIR" && cd "$OUT_DIR" && pwd)"

HOST_UID="${HOST_UID:-0}"
HOST_GID="${HOST_GID:-0}"

export DEBIAN_FRONTEND=noninteractive

echo "==> [${ARCH}] Installing packages..."
apt-get update
apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  clang \
  cmake \
  curl \
  g++-13 \
  gcc-13 \
  git \
  libboost-all-dev \
  libgl1-mesa-dev \
  libglu1-mesa-dev \
  libgmp-dev \
  libmpfr-dev \
  libssl-dev \
  libtbb-dev \
  libvulkan-dev \
  libwayland-dev \
  libx11-dev \
  libx11-xcb-dev \
  libxkbcommon-dev \
  libxcursor-dev \
  libxi-dev \
  libxinerama-dev \
  libxrandr-dev \
  ninja-build \
  patchelf \
  pkg-config \
  python3 \
  python3-venv \
  python3-jinja2 \
  python3-pip \
  unzip \
  xz-utils
rm -rf /var/lib/apt/lists/*

JOBS="${CMAKE_JOB_COUNT:-}"
if [[ -z "$JOBS" ]]; then
  if command -v nproc >/dev/null 2>&1; then
    JOBS="$(nproc)"
  else
    JOBS="4"
  fi
fi

SYSROOT="/tmp/sysroot-${ARCH}"
TMP_ROOT="$(mktemp -d -t ligero-linux-build.XXXXXX)"
cleanup() { rm -rf "$TMP_ROOT"; }
trap cleanup EXIT

DAWN_GIT_REF="${DAWN_GIT_REF:-cec4482eccee45696a7c0019e750c77f101ced04}"
LIGERO_REPO="${LIGERO_REPO:-https://github.com/ligeroinc/ligero-prover.git}"
LIGERO_GIT_REF="${LIGERO_GIT_REF:-74aee0b356cf80fcc1497aec9189fcb643377295}"
WABT_GIT_REF="${WABT_GIT_REF:-a55fb9466f2f886cf0c5bcadab97900f1e0a5789}"

DEPOT_TOOLS_DIR="$TMP_ROOT/depot_tools"
DAWN_SRC="$TMP_ROOT/dawn"
WABT_SRC="$TMP_ROOT/wabt"
LIGERO_SRC="$TMP_ROOT/ligero-prover"

DAWN_BUILD_DIR="$TMP_ROOT/dawn-build"
WABT_BUILD_DIR="$TMP_ROOT/wabt-build"
LIGERO_BUILD_DIR="$TMP_ROOT/ligero-build"

rm -rf "$SYSROOT"
mkdir -p "$SYSROOT" "$DAWN_BUILD_DIR" "$WABT_BUILD_DIR" "$LIGERO_BUILD_DIR"

echo "==> [${ARCH}] Cloning depot_tools..."
git clone --depth 1 https://chromium.googlesource.com/chromium/tools/depot_tools.git "$DEPOT_TOOLS_DIR"
export PATH="$DEPOT_TOOLS_DIR:$PATH"

echo "==> [${ARCH}] Cloning Dawn..."
git clone https://dawn.googlesource.com/dawn "$DAWN_SRC"
cd "$DAWN_SRC"
git checkout "$DAWN_GIT_REF"

cp scripts/standalone.gclient .gclient
gclient sync

LLVM_BIN="$DAWN_SRC/third_party/llvm-build/Release+Asserts/bin"
if [[ ! -x "$LLVM_BIN/clang" || ! -x "$LLVM_BIN/clang++" ]]; then
  echo "error: expected bundled clang at '$LLVM_BIN' (did gclient sync succeed?)" >&2
  exit 1
fi

DAWN_CC="$LLVM_BIN/clang"
DAWN_CXX="$LLVM_BIN/clang++"

# On some hosts / emulation setups, gclient may fetch a Linux_x64 clang even when building in an arm64 container.
# If the bundled clang can't execute (e.g. "rosetta error" or missing x86_64 loader), fall back to system clang.
if ! "$DAWN_CC" --version >/dev/null 2>&1; then
  if command -v clang >/dev/null 2>&1 && command -v clang++ >/dev/null 2>&1; then
    echo "==> [${ARCH}] Bundled clang is not runnable; falling back to system clang/clang++"
    DAWN_CC="clang"
    DAWN_CXX="clang++"
  else
    echo "error: bundled clang is not runnable and system clang/clang++ not found" >&2
    exit 1
  fi
fi

echo "==> [${ARCH}] Configuring Dawn..."
cmake -S "$DAWN_SRC" -B "$DAWN_BUILD_DIR" -G Ninja \
  -DCMAKE_C_COMPILER="$DAWN_CC" \
  -DCMAKE_CXX_COMPILER="$DAWN_CXX" \
  -DCMAKE_BUILD_TYPE=Release \
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

echo "==> [${ARCH}] Building Dawn..."
ninja -C "$DAWN_BUILD_DIR" -j "$JOBS"

echo "==> [${ARCH}] Installing Dawn into sysroot..."
cmake --install "$DAWN_BUILD_DIR"

echo "==> [${ARCH}] Cloning wabt..."
mkdir -p "$WABT_SRC"
git -C "$WABT_SRC" init -q
git -C "$WABT_SRC" remote add origin https://github.com/WebAssembly/wabt.git
git -C "$WABT_SRC" fetch -q --depth 1 origin "$WABT_GIT_REF"
git -C "$WABT_SRC" checkout -q FETCH_HEAD
cd "$WABT_SRC"
git submodule update --init --recursive

echo "==> [${ARCH}] Building wabt..."
cmake -S "$WABT_SRC" -B "$WABT_BUILD_DIR" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER=gcc-13 \
  -DCMAKE_CXX_COMPILER=g++-13 \
  -DCMAKE_INSTALL_PREFIX="$SYSROOT" \
  -DBUILD_TESTS=OFF
cmake --build "$WABT_BUILD_DIR" --parallel "$JOBS"
cmake --install "$WABT_BUILD_DIR"

echo "==> [${ARCH}] Building Dawn via depot_tools + gclient (this may take a while)..."

echo "==> [${ARCH}] Cloning ligero-prover..."
mkdir -p "$LIGERO_SRC"
git -C "$LIGERO_SRC" init -q
git -C "$LIGERO_SRC" remote add origin "$LIGERO_REPO"
git -C "$LIGERO_SRC" fetch -q --depth 1 origin "$LIGERO_GIT_REF"
git -C "$LIGERO_SRC" checkout -q FETCH_HEAD

# echo "==> [${ARCH}] Patching ligero-prover for wabt compatibility..."
# TRANSPILER_HPP="$LIGERO_SRC/include/transpiler.hpp"
# if [[ -f "$TRANSPILER_HPP" ]] && ! grep -q "transpile_wabt_type(const wabt::Var" "$TRANSPILER_HPP"; then
#   # wabt newer API uses wabt::Var for ref.null type (Var::to_type()).
#   perl -0777 -i -pe 's/\}\n\n\/\/ ------------------------------------------------------------/\}\n\n\/\/ Newer wabt represents ref-null types as `wabt::Var` (which may carry an optional type).\n\/\/ Provide an overload so we can support both wabt APIs without pinning a specific version.\nvalue_kind transpile_wabt_type(const wabt::Var& var) {\n    return transpile_wabt_type(var.to_type());\n}\n\n\/\/ ------------------------------------------------------------/s' "$TRANSPILER_HPP"
# fi

echo "==> [${ARCH}] Building ligero-prover..."
cmake -S "$LIGERO_SRC" -B "$LIGERO_BUILD_DIR" -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER=gcc-13 \
  -DCMAKE_CXX_COMPILER=g++-13 \
  -DCMAKE_PREFIX_PATH="$SYSROOT"
cmake --build "$LIGERO_BUILD_DIR" --target webgpu_prover --parallel "$JOBS"
cmake --build "$LIGERO_BUILD_DIR" --target webgpu_verifier --parallel "$JOBS"

STAGE_ROOT="$OUT_DIR"
BIN_STAGE="$STAGE_ROOT/${ARCH}"
mkdir -p "$BIN_STAGE/bin" "$BIN_STAGE/lib"

install -m 0755 "$LIGERO_BUILD_DIR/webgpu_prover" "$BIN_STAGE/bin/webgpu_prover"
install -m 0755 "$LIGERO_BUILD_DIR/webgpu_verifier" "$BIN_STAGE/bin/webgpu_verifier"

set_rpath() {
  local f="$1"
  # Make binaries prefer bundled libs
  patchelf --set-rpath '$ORIGIN/../lib' "$f" || true
}

set_rpath "$BIN_STAGE/bin/webgpu_prover"
set_rpath "$BIN_STAGE/bin/webgpu_verifier"

should_skip_linux_lib() {
  local base="$1"
  case "$base" in
    linux-vdso.so.1|ld-linux-*.so.*|ld-linux.so.*) return 0 ;;
    libc.so.*|libm.so.*|libpthread.so.*|libdl.so.*|librt.so.*) return 0 ;;
    libanl.so.*|libresolv.so.*|libutil.so.*) return 0 ;;
    *) return 1 ;;
  esac
}

copy_needed_libs_linux() {
  local elf="$1"
  ldd "$elf" | awk '/=>/ {print $3}' | while read -r p; do
    [[ -n "$p" && -f "$p" ]] || continue
    local base
    base="$(basename "$p")"
    if should_skip_linux_lib "$base"; then
      continue
    fi
    if [[ ! -f "$BIN_STAGE/lib/$base" ]]; then
      cp -L "$p" "$BIN_STAGE/lib/$base"
      chmod 0644 "$BIN_STAGE/lib/$base" || true
    fi
  done
}

echo "==> [${ARCH}] Collecting shared libs..."
copy_needed_libs_linux "$BIN_STAGE/bin/webgpu_prover"
copy_needed_libs_linux "$BIN_STAGE/bin/webgpu_verifier"

# Also bring Dawn-provided shared libs from sysroot if any exist (best-effort).
if [[ -d "$SYSROOT/lib" ]]; then
  find "$SYSROOT/lib" -maxdepth 1 -type f -name "*.so*" -print0 2>/dev/null | while IFS= read -r -d '' lib; do
    base="$(basename "$lib")"
    if [[ ! -f "$BIN_STAGE/lib/$base" ]]; then
      cp -L "$lib" "$BIN_STAGE/lib/$base"
      chmod 0644 "$BIN_STAGE/lib/$base" || true
    fi
  done
fi

# Second pass: some copied libs bring new deps. Iterate a few times.
for _ in 1 2 3; do
  for f in "$BIN_STAGE/lib/"*.so*; do
    [[ -f "$f" ]] || continue
    copy_needed_libs_linux "$f" || true
  done
done

# Ensure bundled libs themselves can find their deps via RPATH if needed.
for f in "$BIN_STAGE/lib/"*.so*; do
  [[ -f "$f" ]] || continue
  patchelf --set-rpath '$ORIGIN' "$f" || true
done

echo "==> [${ARCH}] Stripping binaries (best-effort)..."
if command -v strip >/dev/null 2>&1; then
  strip --strip-unneeded "$BIN_STAGE/bin/webgpu_prover" || true
  strip --strip-unneeded "$BIN_STAGE/bin/webgpu_verifier" || true
fi

echo "==> [${ARCH}] Done staging to ${BIN_STAGE}"

if [[ "$NO_TAR" == "false" ]]; then
  TARBALL="$OUT_DIR/ligero-${ARCH}.tar.gz"
  rm -f "$TARBALL"
  echo "==> [${ARCH}] Creating tarball: $TARBALL"
  (cd "$OUT_DIR" && tar -czf "$(basename "$TARBALL")" "${ARCH}")
fi

chown -R "$HOST_UID:$HOST_GID" "$BIN_STAGE" || true

echo "==> [${ARCH}] Done."
echo "    Folder: $BIN_STAGE"
if [[ "$NO_TAR" == "false" ]]; then
  echo "    Tarball: $TARBALL"
fi


