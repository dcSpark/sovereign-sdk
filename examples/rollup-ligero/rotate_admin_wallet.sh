#!/usr/bin/env bash
set -euo pipefail

# Rotate admin address references in demo genesis/config files.
#
# If --new-address is omitted, this script generates a random private key and derives
# the matching address via sov-cli. Always inject private keys via secure env/secret manager.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

DEFAULT_OLD_ADDRESS="sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf"

NEW_ADDRESS=""
NEW_KEY_HEX=""
OLD_ADDRESS="$DEFAULT_OLD_ADDRESS"
INCLUDE_CELESTIA=0
DRY_RUN=0
KEY_OUT=""
SOV_CLI_PATH="${SOV_CLI:-}"
GENERATED_KEY_HEX=""
GENERATED_NEW_ADDRESS=0
USED_KEY_FLOW=0
USED_PROVIDED_KEY=0
TOKEN_DEPLOYER_KEY_FILE="${TOKEN_DEPLOYER_KEY_FILE:-$WORKSPACE_ROOT/examples/test-data/keys/token_deployer_private_key.json}"

usage() {
  cat <<'EOF'
Usage:
  ./rotate_admin_wallet.sh [--new-address <sov1...> | --new-key <hex>] [options]

Required:
  (none)

Options:
  --new-key <hex>            32-byte private key (64 hex chars, optional 0x prefix). The
                             script derives address from this key and syncs key files.
  --old-address <sov1...>    Address currently present in files (default: demo admin)
  --include-celestia         Also update examples/test-data/genesis/demo/celestia/*.json
  --key-out <path>           Save generated key JSON to this path (0600). Used only when
                             --new-key is set or --new-address is omitted.
  --sov-cli <path>           Path to sov-cli binary to derive generated address
  --dry-run                  Show what would change without writing files
  -h, --help                 Show this help

Notes:
  - If --new-key is set, address is derived from that key and key files are synced.
  - If neither --new-address nor --new-key is set, a random key is generated and synced.
  - If --new-address is set, this script updates addresses only (key files unchanged).
  - Sync target: examples/test-data/keys/token_deployer_private_key.json
EOF
}

fail() {
  echo "Error: $*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || fail "Missing required command: $1"
}

is_valid_sov_address() {
  local addr="$1"
  [[ "$addr" =~ ^sov1[023456789acdefghjklmnpqrstuvwxyz]+$ ]]
}

normalize_hex_32() {
  local s="$1"
  s="${s#0x}"
  s="${s#0X}"
  [[ "$s" =~ ^[0-9a-fA-F]{64}$ ]] || fail "Invalid key: expected 32-byte hex (64 chars)"
  printf '%s' "$s" | tr '[:upper:]' '[:lower:]'
}

generate_hex_32() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 32
  else
    # Fallback when openssl is unavailable.
    LC_ALL=C tr -dc 'a-f0-9' </dev/urandom | head -c 64
    echo
  fi
}

make_key_file_from_hex() {
  local sk_hex="$1"
  local address="$2"
  local out_path="$3"

  python3 - <<'PY' "$sk_hex" "$address" "$out_path"
import json,sys

sk = sys.argv[1].strip()
address = sys.argv[2].strip()
if sk.startswith("0x") or sk.startswith("0X"):
    sk = sk[2:]
try:
    b = bytes.fromhex(sk)
except Exception as e:
    raise SystemExit(f"invalid hex private key: {e}")
if len(b) != 32:
    raise SystemExit(f"private key must be 32 bytes, got {len(b)}")

doc = {
  "private_key": {"key_pair": list(b)},
  "address": address,
}

with open(sys.argv[3], "w", encoding="utf-8") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
PY
}

resolve_sov_cli() {
  if [[ -n "$SOV_CLI_PATH" ]]; then
    [[ -x "$SOV_CLI_PATH" ]] || fail "--sov-cli path is not executable: $SOV_CLI_PATH"
    echo "$SOV_CLI_PATH"
    return 0
  fi

  if [[ -x "$WORKSPACE_ROOT/target/release/sov-cli" ]]; then
    echo "$WORKSPACE_ROOT/target/release/sov-cli"
    return 0
  fi

  if [[ -x "$WORKSPACE_ROOT/target/debug/sov-cli" ]]; then
    echo "$WORKSPACE_ROOT/target/debug/sov-cli"
    return 0
  fi

  echo "sov-cli not found in target/{release,debug}; building release binary..." >&2
  (cd "$WORKSPACE_ROOT" && SKIP_GUEST_BUILD=1 cargo build -p sov-demo-rollup --bin sov-cli --release >/dev/null)
  [[ -x "$WORKSPACE_ROOT/target/release/sov-cli" ]] || fail "Failed to build sov-cli"
  echo "$WORKSPACE_ROOT/target/release/sov-cli"
}

derive_address_from_key() {
  local sk_hex="$1"
  local tmp_key tmp_wallet sov_cli show_json addr
  # BSD/macOS mktemp requires trailing Xs in the template (no suffix after XXXXXX).
  tmp_key="$(mktemp "${TMPDIR:-/tmp}/rotate_admin_key.XXXXXX")" || fail "Failed to create temp key file"
  tmp_wallet="$(mktemp -d "${TMPDIR:-/tmp}/rotate_admin_wallet.XXXXXX")" || fail "Failed to create temp wallet dir"

  # Address field is ignored by `sov-cli keys import`; the CLI derives it from private key.
  make_key_file_from_hex "$sk_hex" "$DEFAULT_OLD_ADDRESS" "$tmp_key"
  chmod 600 "$tmp_key"

  sov_cli="$(resolve_sov_cli)"

  SOV_WALLET_DIR="$tmp_wallet" "$sov_cli" keys import --nickname rotate-admin --path "$tmp_key" --skip-if-present >/dev/null
  show_json="$(SOV_WALLET_DIR="$tmp_wallet" "$sov_cli" keys show by-nickname rotate-admin)"
  addr="$(printf '%s' "$show_json" | jq -r '.address // empty')"
  [[ -n "$addr" ]] || fail "Failed to derive address from generated key via sov-cli"
  is_valid_sov_address "$addr" || fail "Derived invalid sov address: $addr"

  rm -f "$tmp_key"
  rm -rf "$tmp_wallet"

  printf '%s' "$addr"
}

sync_key_files() {
  local sk_hex="$1"
  local address="$2"

  if [[ -n "$KEY_OUT" ]]; then
    [[ ! -e "$KEY_OUT" ]] || fail "--key-out already exists: $KEY_OUT"
  fi

  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] would update key file: ${TOKEN_DEPLOYER_KEY_FILE#$WORKSPACE_ROOT/}"
    if [[ -n "$KEY_OUT" ]]; then
      echo "[dry-run] would write key file: $KEY_OUT"
    fi
    return 0
  fi

  mkdir -p "$(dirname "$TOKEN_DEPLOYER_KEY_FILE")"
  make_key_file_from_hex "$sk_hex" "$address" "$TOKEN_DEPLOYER_KEY_FILE"
  chmod 600 "$TOKEN_DEPLOYER_KEY_FILE"
  echo "updated key file: ${TOKEN_DEPLOYER_KEY_FILE#$WORKSPACE_ROOT/}"

  if [[ -n "$KEY_OUT" ]]; then
    mkdir -p "$(dirname "$KEY_OUT")"
    make_key_file_from_hex "$sk_hex" "$address" "$KEY_OUT"
    chmod 600 "$KEY_OUT"
    echo "wrote key file: $KEY_OUT"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --new-address)
      NEW_ADDRESS="${2:-}"
      shift 2
      ;;
    --new-key)
      NEW_KEY_HEX="${2:-}"
      shift 2
      ;;
    --old-address)
      OLD_ADDRESS="${2:-}"
      shift 2
      ;;
    --include-celestia)
      INCLUDE_CELESTIA=1
      shift
      ;;
    --key-out)
      KEY_OUT="${2:-}"
      shift 2
      ;;
    --sov-cli)
      SOV_CLI_PATH="${2:-}"
      shift 2
      ;;
    --sync-token-deployer-key)
      fail "--sync-token-deployer-key was removed; key sync is automatic with --new-key or when --new-address is omitted."
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "Unknown argument: $1 (use --help)"
      ;;
  esac
done

require_cmd jq
require_cmd rg
require_cmd python3

if [[ -n "$NEW_ADDRESS" && -n "$NEW_KEY_HEX" ]]; then
  fail "Use either --new-address or --new-key, not both."
fi

if [[ -n "$NEW_KEY_HEX" ]]; then
  NEW_KEY_HEX="$(normalize_hex_32 "$NEW_KEY_HEX")"
  NEW_ADDRESS="$(derive_address_from_key "$NEW_KEY_HEX")"
  USED_KEY_FLOW=1
  USED_PROVIDED_KEY=1
elif [[ -z "$NEW_ADDRESS" ]]; then
  GENERATED_KEY_HEX="$(generate_hex_32)"
  NEW_ADDRESS="$(derive_address_from_key "$GENERATED_KEY_HEX")"
  GENERATED_NEW_ADDRESS=1
  USED_KEY_FLOW=1
fi

if [[ -n "$KEY_OUT" && "$USED_KEY_FLOW" -eq 0 ]]; then
  fail "--key-out requires --new-key or omitting --new-address."
fi

is_valid_sov_address "$NEW_ADDRESS" || fail "Invalid --new-address format: $NEW_ADDRESS"
is_valid_sov_address "$OLD_ADDRESS" || fail "Invalid --old-address format: $OLD_ADDRESS"

if [[ "$USED_KEY_FLOW" -eq 1 ]]; then
  if [[ "$GENERATED_NEW_ADDRESS" -eq 1 ]]; then
    sync_key_files "$GENERATED_KEY_HEX" "$NEW_ADDRESS"
  else
    sync_key_files "$NEW_KEY_HEX" "$NEW_ADDRESS"
  fi
fi

JSON_FILTER='
def walk(f):
  . as $in
  | if type == "object" then
      reduce keys[] as $k ({}; . + { ($k): ($in[$k] | walk(f)) })
    elif type == "array" then
      map(walk(f))
    else
      f
    end;
walk(if type == "string" and . == $old then $new else . end)
'

FILES_JSON=(
  "$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock/access_pattern.json"
  "$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock/attester_incentives.json"
  "$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock/bank.json"
  "$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock/midnight_privacy.json"
  "$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock/operator_incentives.json"
  "$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock/prover_incentives.json"
  "$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock/sequencer_registry.json"
  "$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock/value_setter.json"
  "$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock/value_setter_zk.json"
)

if [[ "$INCLUDE_CELESTIA" -eq 1 ]]; then
  FILES_JSON+=(
    "$WORKSPACE_ROOT/examples/test-data/genesis/demo/celestia/access_pattern.json"
    "$WORKSPACE_ROOT/examples/test-data/genesis/demo/celestia/attester_incentives.json"
    "$WORKSPACE_ROOT/examples/test-data/genesis/demo/celestia/bank.json"
    "$WORKSPACE_ROOT/examples/test-data/genesis/demo/celestia/create_token.json"
    "$WORKSPACE_ROOT/examples/test-data/genesis/demo/celestia/midnight_privacy.json"
    "$WORKSPACE_ROOT/examples/test-data/genesis/demo/celestia/operator_incentives.json"
    "$WORKSPACE_ROOT/examples/test-data/genesis/demo/celestia/prover_incentives.json"
    "$WORKSPACE_ROOT/examples/test-data/genesis/demo/celestia/sequencer_registry.json"
    "$WORKSPACE_ROOT/examples/test-data/genesis/demo/celestia/value_setter.json"
    "$WORKSPACE_ROOT/examples/test-data/genesis/demo/celestia/value_setter_zk.json"
  )
fi

FILES_TOML=(
  "$WORKSPACE_ROOT/examples/rollup-ligero/rollup_config.toml"
  "$WORKSPACE_ROOT/examples/rollup-ligero/rollup_config_replica.toml"
)

for f in "${FILES_JSON[@]}" "${FILES_TOML[@]}"; do
  [[ -f "$f" ]] || fail "Expected file not found: $f"
done

changed=0

update_json_file() {
  local file="$1"
  local tmp
  tmp="$(mktemp)"
  jq --arg old "$OLD_ADDRESS" --arg new "$NEW_ADDRESS" "$JSON_FILTER" "$file" >"$tmp"

  if cmp -s "$file" "$tmp"; then
    rm -f "$tmp"
    return 0
  fi

  changed=$((changed + 1))
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] would update JSON: ${file#$WORKSPACE_ROOT/}"
    rm -f "$tmp"
  else
    mv "$tmp" "$file"
    echo "updated JSON: ${file#$WORKSPACE_ROOT/}"
  fi
}

update_toml_file() {
  local file="$1"
  local tmp
  tmp="$(mktemp)"

  awk -v new="$NEW_ADDRESS" '
    /^[[:space:]]*prover_address[[:space:]]*=/ { print "prover_address = \"" new "\""; next }
    /^[[:space:]]*rollup_address[[:space:]]*=/ { print "rollup_address = \"" new "\""; next }
    { print }
  ' "$file" >"$tmp"

  if cmp -s "$file" "$tmp"; then
    rm -f "$tmp"
    return 0
  fi

  changed=$((changed + 1))
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "[dry-run] would update TOML: ${file#$WORKSPACE_ROOT/}"
    rm -f "$tmp"
  else
    mv "$tmp" "$file"
    echo "updated TOML: ${file#$WORKSPACE_ROOT/}"
  fi
}

echo "Rotating admin address references:"
echo "  old: $OLD_ADDRESS"
echo "  new: $NEW_ADDRESS"
if [[ "$GENERATED_NEW_ADDRESS" -eq 1 ]]; then
  echo "  generated new admin key: yes"
  if [[ -n "$KEY_OUT" ]]; then
    echo "  generated key file: $KEY_OUT"
  else
    echo "  generated key file: no (not requested)"
  fi
  echo "  token deployer key file: ${TOKEN_DEPLOYER_KEY_FILE#$WORKSPACE_ROOT/} (synced)"
elif [[ "$USED_PROVIDED_KEY" -eq 1 ]]; then
  echo "  generated new admin key: no (used --new-key)"
  if [[ -n "$KEY_OUT" ]]; then
    echo "  key file output: $KEY_OUT"
  fi
  echo "  token deployer key file: ${TOKEN_DEPLOYER_KEY_FILE#$WORKSPACE_ROOT/} (synced)"
else
  echo "  generated new admin key: no"
  echo "  token deployer key file: unchanged (no private key provided)"
fi
if [[ "$INCLUDE_CELESTIA" -eq 1 ]]; then
  echo "  include celestia genesis: yes"
else
  echo "  include celestia genesis: no"
fi
if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "  mode: dry-run"
else
  echo "  mode: write"
fi
echo

for f in "${FILES_JSON[@]}"; do
  update_json_file "$f"
done

for f in "${FILES_TOML[@]}"; do
  update_toml_file "$f"
done

echo
if [[ "$changed" -eq 0 ]]; then
  echo "No file changes were needed."
else
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "Dry-run complete: $changed file(s) would be updated."
  else
    echo "Done: updated $changed file(s)."
  fi
fi

cat <<EOF

Next:
1) Ensure genesis funds the new address in bank.json.
2) Inject ADMIN_WALLET_PRIVATE_KEY for that new address from your secret manager.
3) Avoid demo key fallbacks in run scripts for production.
EOF

if [[ "$GENERATED_NEW_ADDRESS" -eq 1 ]]; then
  cat <<EOF

Generated values (store securely):
  GENERATED_ADMIN_ADDRESS=$NEW_ADDRESS
  GENERATED_ADMIN_WALLET_PRIVATE_KEY=$GENERATED_KEY_HEX
EOF
fi
