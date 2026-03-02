#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

export RUST_LOG="${RUST_LOG:-info}"
export MIDNIGHT_FVK_SERVICE_BIND="${MIDNIGHT_FVK_SERVICE_BIND:-127.0.0.1:8088}"

normalize_hex() {
  local s="${1:-}"
  s="$(printf "%s" "$s" | tr -d '[:space:]')"
  s="${s#0x}"
  s="${s#0X}"
  printf "%s" "$s" | tr '[:upper:]' '[:lower:]'
}

dotenv_get() {
  local file="$1"
  local key="$2"
  if [[ ! -f "$file" ]]; then
    return 0
  fi
  local line=""
  line="$(grep -E "^[[:space:]]*(export[[:space:]]+)?${key}[[:space:]]*=" "$file" 2>/dev/null | tail -n 1 || true)"
  if [[ -z "$line" ]]; then
    return 0
  fi
  line="$(printf "%s" "$line" | sed -E "s/^[[:space:]]*(export[[:space:]]+)?${key}[[:space:]]*=//")"
  line="${line%$'\r'}"
  line="${line%%#*}"
  line="$(printf "%s" "$line" | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//')"
  if [[ "$line" == \"*\" && "$line" == *\" ]]; then
    line="${line#\"}"
    line="${line%\"}"
  elif [[ "$line" == \'*\' && "$line" == *\' ]]; then
    line="${line#\'}"
    line="${line%\'}"
  fi
  printf "%s" "$line"
}

FVK_SERVICE_DOTENV="$WORKSPACE_ROOT/crates/utils/midnight-fvk-service/.env"

if [[ -n "${POOL_FVK_PK:-}" ]]; then
  pool_pk_norm="$(normalize_hex "$POOL_FVK_PK")"
  dotenv_pk="$(dotenv_get "$FVK_SERVICE_DOTENV" "MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX")"
  dotenv_pk_norm="$(normalize_hex "$dotenv_pk")"
  env_pk_norm="$(normalize_hex "${MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX:-}")"

  # If the service PK is configured in its .env, it must match POOL_FVK_PK exactly.
  if [[ -n "$dotenv_pk_norm" && "$dotenv_pk_norm" != "$pool_pk_norm" ]]; then
    echo "Error: POOL_FVK_PK does not match midnight-fvk-service .env signing pk."
    echo "  POOL_FVK_PK=$POOL_FVK_PK"
    echo "  $FVK_SERVICE_DOTENV has MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX=$dotenv_pk"
    exit 1
  fi

  # If the service PK is provided via env, it must also match.
  if [[ -n "$env_pk_norm" && "$env_pk_norm" != "$pool_pk_norm" ]]; then
    echo "Error: POOL_FVK_PK != MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX (env mismatch)."
    echo "  POOL_FVK_PK=$POOL_FVK_PK"
    echo "  MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX=${MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX:-}"
    exit 1
  fi

  # If no PK is configured anywhere, default the service PK to POOL_FVK_PK.
  if [[ -z "${MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX:-}" && -z "$dotenv_pk_norm" ]]; then
    export MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX="$POOL_FVK_PK"
  fi
fi

DEMO_DATA_DIR="$WORKSPACE_ROOT/examples/rollup-ligero/demo_data"
mkdir -p "$DEMO_DATA_DIR"
export MIDNIGHT_FVK_SERVICE_DB="${MIDNIGHT_FVK_SERVICE_DB:-sqlite://$DEMO_DATA_DIR/midnight_fvk_service.sqlite?mode=rwc}"

if [[ -z "${MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX:-}" ]]; then
  # The service binary will also attempt to load its own `.env` file from the crate directory.
  # Fail early only if the `.env` file is missing too.
  if [[ ! -f "$WORKSPACE_ROOT/crates/utils/midnight-fvk-service/.env" ]]; then
    echo "Error: midnight-fvk-service signing key not configured."
    echo "Set MIDNIGHT_FVK_SERVICE_SIGNING_SK_HEX and MIDNIGHT_FVK_SERVICE_SIGNING_PK_HEX,"
    echo "or create $WORKSPACE_ROOT/crates/utils/midnight-fvk-service/.env (run: cargo run -p midnight-fvk-service -- keygen)."
    exit 1
  fi
fi

BIN="$WORKSPACE_ROOT/target/release/midnight-fvk-service"
if [[ ! -f "$BIN" ]]; then
  echo "ERROR: Binary not found at $BIN"
  echo "Run: cargo build --release -p midnight-fvk-service"
  exit 1
fi

echo "🚀 Starting midnight-fvk-service..."
echo "   MIDNIGHT_FVK_SERVICE_BIND=$MIDNIGHT_FVK_SERVICE_BIND"
echo "   MIDNIGHT_FVK_SERVICE_DB=$MIDNIGHT_FVK_SERVICE_DB"
echo ""

exec "$BIN" serve
