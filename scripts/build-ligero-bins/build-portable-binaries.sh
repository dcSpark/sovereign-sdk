#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: build-portable-binaries.sh [options]

Build and package portable Ligero native binaries into:

  crates/adapters/ligero/bins/
    <os>-<arch>/{bin,lib}/
    shader/

Then produce a single tarball containing the contents of `crates/adapters/ligero/bins/`.

Architectures:
  - linux-amd64   (Docker, linux/amd64)
  - linux-arm64   (Docker, linux/arm64)
  - macos-arm64   (native build on Apple Silicon)

Options:
  -a, --arch <name...>   Limit builds to the listed architectures.
                         Supported: linux-amd64 linux-x86_64 amd64 x86_64 linux-arm64 arm64 aarch64 macos-arm64 mac-arm64
  -o, --out <dir>        Output directory (default: <repo>/crates/adapters/ligero/bins)
  -h, --help             Show this help message.

Environment:
  DOCKER_IMAGE           Docker image for Linux builds (default: ubuntu:24.04)
  CMAKE_JOB_COUNT        Override parallel job count
  DAWN_GIT_REF           Dawn commit ref used for builds (default: cec4482eccee45696a7c0019e750c77f101ced04)
  LIGERO_REPO            Ligero prover git URL used to fetch shaders (default: https://github.com/nicarq/ligero-prover.git)
  LIGERO_GIT_REF         Ligero prover git ref used to fetch shaders (default: e0f521742e5693d99abbb480a926f5c32b9040b7)
  WABT_GIT_REF           WABT git ref used for builds (default: a55fb9466f2f886cf0c5bcadab97900f1e0a5789)
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

OUT_DIR="$REPO_ROOT/crates/adapters/ligero/bins"
DOCKER_IMAGE="${DOCKER_IMAGE:-ubuntu:24.04}"
DAWN_GIT_REF="${DAWN_GIT_REF:-cec4482eccee45696a7c0019e750c77f101ced04}"
LIGERO_REPO="${LIGERO_REPO:-https://github.com/nicarq/ligero-prover.git}"
LIGERO_GIT_REF="${LIGERO_GIT_REF:-e0f521742e5693d99abbb480a926f5c32b9040b7}"
WABT_GIT_REF="${WABT_GIT_REF:-a55fb9466f2f886cf0c5bcadab97900f1e0a5789}"

DEFAULT_ARCHES=("linux-amd64" "linux-arm64" "macos-arm64")
ARCHES=("${DEFAULT_ARCHES[@]}")

normalize_arch() {
  case "${1}" in
    linux-amd64|linux-x86_64|amd64|x86_64) echo "linux-amd64" ;;
    linux-arm64|arm64|aarch64) echo "linux-arm64" ;;
    macos-arm64|mac-arm64) echo "macos-arm64" ;;
    *) echo "" ;;
  esac
}

if [[ $# -gt 0 ]]; then
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -a|--arch)
        shift
        [[ $# -gt 0 ]] || { echo "error: --arch expects at least one value" >&2; usage; exit 1; }
        ARCHES=()
        while [[ $# -gt 0 && ${1:0:1} != "-" ]]; do
          norm="$(normalize_arch "$1")"
          [[ -n "$norm" ]] || { echo "error: unsupported arch '$1'" >&2; usage; exit 1; }
          ARCHES+=("$norm")
          shift
        done
        ;;
      -o|--out)
        shift
        [[ $# -gt 0 ]] || { echo "error: --out expects a directory path" >&2; usage; exit 1; }
        OUT_DIR="$1"
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        echo "error: unknown argument '$1'" >&2
        usage
        exit 1
        ;;
    esac
  done
fi

if [[ ${#ARCHES[@]} -eq 0 ]]; then
  echo "error: no architectures selected" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
STAGE_DIR="$OUT_DIR"

HOST_UID="$(id -u)"
HOST_GID="$(id -g)"

echo "==> Preparing output directory..."
# Migration: older checkouts used `bins/macos`. New canonical name is `bins/macos-arm64`.
if [[ -d "$OUT_DIR/macos" && ! -d "$OUT_DIR/macos-arm64" ]]; then
  mv "$OUT_DIR/macos" "$OUT_DIR/macos-arm64"
fi

# Ensure we don't keep stale duplicates from previous runs.
rm -rf "$OUT_DIR/shader"
mkdir -p "$OUT_DIR/shader"
rm -rf "$OUT_DIR/linux-amd64/shader" "$OUT_DIR/linux-arm64/shader" "$OUT_DIR/macos-arm64/shader" || true

ensure_shaders() {
  # If shader dir already has files, keep it.
  if [[ -d "$STAGE_DIR/shader" ]] && find "$STAGE_DIR/shader" -mindepth 1 -maxdepth 1 | read -r _; then
    return 0
  fi

  echo "==> Fetching shaders..."
  if ! command -v git >/dev/null 2>&1; then
    echo "error: git is required to fetch shaders but not found in PATH" >&2
    exit 1
  fi

  tmp="$(mktemp -d -t ligero-shader.XXXXXX)"
  trap 'rm -rf "$tmp"' RETURN
  mkdir -p "$tmp/ligero-prover"
  git -C "$tmp/ligero-prover" init -q
  git -C "$tmp/ligero-prover" remote add origin "$LIGERO_REPO"
  # Fetch just the requested commit (works even if it's not the tip of a branch)
  git -C "$tmp/ligero-prover" fetch -q --depth 1 origin "$LIGERO_GIT_REF"
  git -C "$tmp/ligero-prover" checkout -q FETCH_HEAD
  if [[ ! -d "$tmp/ligero-prover/shader" ]]; then
    echo "error: shader folder not found in $LIGERO_REPO ($LIGERO_GIT_REF)" >&2
    exit 1
  fi
  rm -rf "$STAGE_DIR/shader"
  mkdir -p "$STAGE_DIR/shader"
  cp -R "$tmp/ligero-prover/shader/." "$STAGE_DIR/shader/"
}

ensure_shaders

need_docker=false
for arch in "${ARCHES[@]}"; do
  if [[ "$arch" == linux-* ]]; then
    need_docker=true
  fi
done
if [[ "$need_docker" == "true" ]]; then
  if ! command -v docker >/dev/null 2>&1; then
    echo "error: docker is required for linux builds but not found in PATH" >&2
    exit 1
  fi
fi

build_linux() {
  local arch="$1"
  local platform=""
  case "$arch" in
    linux-amd64) platform="linux/amd64" ;;
    linux-arm64) platform="linux/arm64" ;;
    *) echo "internal error: build_linux called with '$arch'" >&2; exit 1 ;;
  esac

  echo "==> Building $arch via Docker ($platform)..."

  # Remove any previous staged output for this arch to avoid stale files.
  rm -rf "$OUT_DIR/$arch"

  docker run \
    --rm \
    --platform="$platform" \
    -e "HOST_UID=$HOST_UID" \
    -e "HOST_GID=$HOST_GID" \
    -e "CMAKE_JOB_COUNT=${CMAKE_JOB_COUNT:-}" \
    -e "DAWN_GIT_REF=$DAWN_GIT_REF" \
    -e "WABT_GIT_REF=$WABT_GIT_REF" \
    -v "$OUT_DIR:/out" \
    -v "$SCRIPT_DIR/portable/linux-build-and-stage.sh:/run.sh:ro" \
    -w / \
    "$DOCKER_IMAGE" \
    bash /run.sh --arch "$arch" --out /out --no-tar
}

build_macos_arm64() {
  echo "==> Building macos-arm64 locally..."
  if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: macos-arm64 build must be run on macOS" >&2
    exit 1
  fi
  if [[ "$(uname -m)" != "arm64" ]]; then
    echo "error: macos-arm64 build must be run on Apple Silicon (arm64)" >&2
    exit 1
  fi
  # Standalone script; stage into our shared multi-arch output and skip its tarball
  rm -rf "$OUT_DIR/macos-arm64"
  DAWN_GIT_REF="$DAWN_GIT_REF" WABT_GIT_REF="$WABT_GIT_REF" bash "$SCRIPT_DIR/portable/macos-build-and-stage.sh" --out "$OUT_DIR" --no-tar
}

# De-dupe arches while preserving order
UNIQ_ARCHES=()
for a in "${ARCHES[@]}"; do
  seen=false
  for b in "${UNIQ_ARCHES[@]}"; do
    if [[ "$a" == "$b" ]]; then
      seen=true
      break
    fi
  done
  if [[ "$seen" == "false" ]]; then
    UNIQ_ARCHES+=("$a")
  fi
done

for arch in "${UNIQ_ARCHES[@]}"; do
  case "$arch" in
    linux-amd64|linux-arm64) build_linux "$arch" ;;
    macos-arm64) build_macos_arm64 ;;
    *) echo "error: unsupported arch '$arch'" >&2; exit 1 ;;
  esac
done

echo "==> Creating tar.gz..."
TARBALL="$OUT_DIR/ligero-bins.tar.gz"
rm -f "$TARBALL"
(cd "$OUT_DIR" && tar --exclude="$(basename "$TARBALL")" -czf "$(basename "$TARBALL")" .)

echo "==> Done."
echo "    Folder:  $OUT_DIR"
echo "    Tarball: $TARBALL"


