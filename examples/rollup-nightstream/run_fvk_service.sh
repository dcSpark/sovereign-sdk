#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${SERVICE_TARGET_DIR:-release}"
BIN="$WORKSPACE_ROOT/target/$TARGET_DIR/midnight-fvk-service"

export RUST_LOG="${RUST_LOG:-info}"
export MIDNIGHT_FVK_SERVICE_BIND="${MIDNIGHT_FVK_SERVICE_BIND:-127.0.0.1:8088}"

normalize_hex() {
  local s="${1:-}"
  s="$(printf "%s" "$s" | tr -d '[:space:]')"
  s="${s#0x}"
  s="${s#0X}"
  printf "%s" "$s" | tr '[:upper:]' '[:lower:]'
}

if [[ -n "${POOL_FVK_PK:-}" ]]; then
  pool_pk_norm="$(normalize_hex "$POOL_FVK_PK")"
  env_pk_norm="$(normalize_hex "${MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX:-}")"

  if [[ -n "$env_pk_norm" && "$env_pk_norm" != "$pool_pk_norm" ]]; then
    echo "Error: POOL_FVK_PK != MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX"
    exit 1
  fi

  if [[ -z "${MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX:-}" ]]; then
    export MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX="$POOL_FVK_PK"
  fi
fi

DEMO_DATA_DIR="$SCRIPT_DIR/demo_data"
mkdir -p "$DEMO_DATA_DIR"
export MIDNIGHT_FVK_SERVICE_DB="${MIDNIGHT_FVK_SERVICE_DB:-postgresql://admin:1234@localhost:5432/fvk}"

if [[ -z "${MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX:-}" ]]; then
  if [[ ! -f "$WORKSPACE_ROOT/crates/utils/midnight-fvk-service/.env" ]]; then
    echo "Error: midnight-fvk-service signing key not configured."
    echo "Set MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX and MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX,"
    echo "or create $WORKSPACE_ROOT/crates/utils/midnight-fvk-service/.env"
    exit 1
  fi
fi

echo "Midnight FVK Service"
echo "  Bind: $MIDNIGHT_FVK_SERVICE_BIND"
echo "  DB:   $MIDNIGHT_FVK_SERVICE_DB"

if [[ ! -f "$BIN" ]]; then
  echo "ERROR: Binary not found at $BIN"
  echo "Run: cargo build ${TARGET_DIR/release/--release }-p midnight-fvk-service"
  exit 1
fi

cd "$WORKSPACE_ROOT"
exec "$BIN" serve
