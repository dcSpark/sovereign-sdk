#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

if [[ -f "$SCRIPT_DIR/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$SCRIPT_DIR/.env"
  set +a
fi

usage() {
  cat <<'USAGE'
Usage:
  pool_blacklist_admin.sh add-admin <SOV_ADDRESS>
  pool_blacklist_admin.sh remove-admin <SOV_ADDRESS>
  pool_blacklist_admin.sh freeze <PRIVPOOL_ADDRESS>
  pool_blacklist_admin.sh unfreeze <PRIVPOOL_ADDRESS>
  pool_blacklist_admin.sh list-admins
  pool_blacklist_admin.sh list-frozen

Environment (can be set via scripts/pool-blacklist-admin/.env):
  ROLLUP_RPC_URL        Default: http://localhost:12346
  POOL_ADMIN_SK         32-byte hex private key (with or without 0x)
  POOL_ADMIN_KEY_FILE   Path to key JSON file (overrides POOL_ADMIN_SK)
  MAX_FEE               Default: 1000000
  CHAIN_ID              Optional (auto-fetched from /rollup/schema if unset; fallback 4321)
  WAIT_FOR_PROCESSING   1/0 (default: 1)
  SOV_CLI               Optional path to sov-cli binary

Notes:
  - add-admin/remove-admin must be signed by the midnight-privacy *module admin*.
  - freeze/unfreeze must be signed by a midnight-privacy *pool admin*.
USAGE
}

die() {
  echo "Error: $*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"
}

ROLLUP_RPC_URL="${ROLLUP_RPC_URL:-http://localhost:12346}"
MAX_FEE="${MAX_FEE:-1000000}"
WAIT_FOR_PROCESSING="${WAIT_FOR_PROCESSING:-1}"

cmd="${1:-}"
arg="${2:-}"
[[ -n "$cmd" ]] || { usage; exit 1; }

case "$cmd" in
  add-admin|remove-admin|freeze|unfreeze|list-admins|list-frozen) ;;
  -h|--help|help) usage; exit 0 ;;
  *) usage; die "Unknown command: $cmd" ;;
esac

case "$cmd" in
  list-admins)
    require_cmd curl
    curl -fsS "${ROLLUP_RPC_URL%/}/modules/midnight-privacy/blacklist/admins" \
      -H 'Accept: application/json'
    echo ""
    exit 0
    ;;
  list-frozen)
    require_cmd curl
    curl -fsS "${ROLLUP_RPC_URL%/}/modules/midnight-privacy/blacklist/frozen" \
      -H 'Accept: application/json'
    echo ""
    exit 0
    ;;
esac

[[ -n "$arg" ]] || die "Missing argument for $cmd"

resolve_sov_cli() {
  if [[ -n "${SOV_CLI:-}" ]]; then
    [[ -x "$SOV_CLI" ]] || die "SOV_CLI is set but not executable: $SOV_CLI"
    echo "$SOV_CLI"
    return 0
  fi

  if [[ -x "$REPO_ROOT/target/release/sov-cli" ]]; then
    echo "$REPO_ROOT/target/release/sov-cli"
    return 0
  fi
  if [[ -x "$REPO_ROOT/target/debug/sov-cli" ]]; then
    echo "$REPO_ROOT/target/debug/sov-cli"
    return 0
  fi

  require_cmd cargo
  (cd "$REPO_ROOT" && SKIP_GUEST_BUILD=1 cargo build -p sov-demo-rollup --bin sov-cli) >/dev/null
  [[ -x "$REPO_ROOT/target/debug/sov-cli" ]] || die "Failed to build sov-cli"
  echo "$REPO_ROOT/target/debug/sov-cli"
}

fetch_chain_id() {
  local url="$1"
  if [[ -n "${CHAIN_ID:-}" ]]; then
    echo "$CHAIN_ID"
    return 0
  fi
  if command -v curl >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
    local schema
    schema="$(curl -fsS "${url%/}/rollup/schema" 2>/dev/null || true)"
    if [[ -n "$schema" ]]; then
      local cid
      cid="$(python3 - <<'PY' "$schema" 2>/dev/null || true
import json,sys
try:
  j=json.loads(sys.argv[1])
  print(j["schema"]["chain_data"]["chain_id"])
except Exception:
  pass
PY
)"
      if [[ -n "$cid" ]]; then
        echo "$cid"
        return 0
      fi
    fi
  fi
  echo "4321"
}

make_key_file_from_hex() {
  local sk_hex="$1"
  local out_path="$2"

  require_cmd python3

  python3 - <<'PY' "$sk_hex" "$out_path"
import json,sys

sk = sys.argv[1].strip()
if sk.startswith("0x") or sk.startswith("0X"):
    sk = sk[2:]
try:
    b = bytes.fromhex(sk)
except Exception as e:
    raise SystemExit(f"invalid hex private key: {e}")
if len(b) != 32:
    raise SystemExit(f"private key must be 32 bytes, got {len(b)}")

# The `address` field is not used by `sov-cli keys import` (it derives the address from the key),
# but it must be a valid address string for JSON parsing.
placeholder_address = "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf"

doc = {
  "private_key": {"key_pair": list(b)},
  "address": placeholder_address,
}

with open(sys.argv[2], "w", encoding="utf-8") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
PY
}

wallet_dir_cleanup=0
wallet_dir="${SOV_WALLET_DIR:-}"
if [[ -z "$wallet_dir" ]]; then
  wallet_dir_cleanup=1
  wallet_dir="$(mktemp -d "${TMPDIR:-/tmp}/sov_cli_wallet.XXXXXX")"
fi

cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  if [[ "$wallet_dir_cleanup" -eq 1 ]]; then
    rm -rf "$wallet_dir" || true
  fi
  exit "$rc"
}
trap cleanup EXIT INT TERM

export SOV_WALLET_DIR="$wallet_dir"

sov_cli="$(resolve_sov_cli)"

key_file="${POOL_ADMIN_KEY_FILE:-}"
if [[ -n "$key_file" ]]; then
  [[ -f "$key_file" ]] || die "POOL_ADMIN_KEY_FILE not found: $key_file"
else
  sk="${POOL_ADMIN_SK:-}"
  if [[ -z "$sk" ]]; then
    default_key="$REPO_ROOT/examples/test-data/keys/token_deployer_private_key.json"
    if [[ -f "$default_key" ]]; then
      key_file="$default_key"
    else
      die "Set POOL_ADMIN_SK or POOL_ADMIN_KEY_FILE (see .env.example)"
    fi
  else
    key_file="$wallet_dir/pool_admin_key.json"
    make_key_file_from_hex "$sk" "$key_file"
  fi
fi

chain_id="$(fetch_chain_id "$ROLLUP_RPC_URL")"

# Ensure wallet has the signing key and a node URL configured.
"$sov_cli" keys import --nickname pool-admin --path "$key_file" --skip-if-present >/dev/null
"$sov_cli" keys activate by-nickname pool-admin >/dev/null 2>&1 || true
"$sov_cli" node set-url "$ROLLUP_RPC_URL" >/dev/null

tx_json=""
case "$cmd" in
  add-admin)
    tx_json="$(printf '{"add_pool_admin":{"admin":"%s"}}' "$arg")"
    ;;
  remove-admin)
    tx_json="$(printf '{"remove_pool_admin":{"admin":"%s"}}' "$arg")"
    ;;
  freeze)
    tx_json="$(printf '{"freeze_address":{"address":"%s"}}' "$arg")"
    ;;
  unfreeze)
    tx_json="$(printf '{"unfreeze_address":{"address":"%s"}}' "$arg")"
    ;;
esac

"$sov_cli" transactions clean >/dev/null
"$sov_cli" transactions import from-string midnight-privacy \
  --json "$tx_json" \
  --chain-id "$chain_id" \
  --max-fee "$MAX_FEE" >/dev/null

submit_args=()
if [[ "$WAIT_FOR_PROCESSING" == "1" || "$WAIT_FOR_PROCESSING" == "true" ]]; then
  submit_args+=(--wait-for-processing)
fi

echo "Submitting $cmd via $ROLLUP_RPC_URL (chain_id=$chain_id, max_fee=$MAX_FEE)..."
"$sov_cli" node submit-batch "${submit_args[@]}" by-nickname pool-admin
