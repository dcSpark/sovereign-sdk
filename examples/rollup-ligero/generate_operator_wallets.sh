#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

OUTPUT=""
FORMAT="json"
FORCE=0
SOV_CLI_PATH="${SOV_CLI:-}"

usage() {
  cat <<'USAGE'
Generate dedicated operator wallet private keys and derived addresses for production.

Usage:
  ./generate_operator_wallets.sh [options]

Options:
  --output <path>         Write output to file instead of stdout
  --format <json|dotenv|export>
                          json:   {"KEY":"value", ...}
                          dotenv: KEY=value
                          export: export KEY="value"
  --force                 Overwrite --output file if it exists
  --sov-cli <path>        Path to sov-cli binary (optional)
  -h, --help              Show this help

Generated keys/addresses:
  ADMIN_WALLET_PRIVATE_KEY / ADMIN_ADDRESS
  SEQUENCER_WALLET_PRIVATE_KEY / SEQUENCER_ADDRESS
  PAYMASTER_WALLET_PRIVATE_KEY / PAYMASTER_ADDRESS
  PROVER_WALLET_PRIVATE_KEY / PROVER_ADDRESS
  ATTESTER_WALLET_PRIVATE_KEY / ATTESTER_ADDRESS

Notes:
  - Save output in your secret manager (do not commit it).
  - This script only generates keys and addresses; it does not change genesis files.
USAGE
}

fail() {
  echo "Error: $*" >&2
  exit 1
}

rand_hex_32() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 32
    return 0
  fi

  if command -v python3 >/dev/null 2>&1; then
    python3 - <<'PY'
import secrets
print(secrets.token_hex(32))
PY
    return 0
  fi

  LC_ALL=C tr -dc 'a-f0-9' </dev/urandom | head -c 64
  echo
}

make_key_file_from_hex() {
  local sk_hex="$1"
  local out_path="$2"

  python3 - <<'PY' "$sk_hex" "$out_path"
import json
import sys

sk = sys.argv[1].strip()
if sk.startswith(("0x", "0X")):
    sk = sk[2:]

try:
    key_bytes = bytes.fromhex(sk)
except Exception as exc:
    raise SystemExit(f"invalid private key hex: {exc}")

if len(key_bytes) != 32:
    raise SystemExit(f"private key must be 32 bytes, got {len(key_bytes)}")

doc = {
    "private_key": {"key_pair": list(key_bytes)},
    # Address is ignored by `sov-cli keys import`; it is re-derived from private_key.
    "address": "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf",
}

with open(sys.argv[2], "w", encoding="utf-8") as fp:
    json.dump(doc, fp, indent=2)
    fp.write("\n")
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

  command -v cargo >/dev/null 2>&1 || fail "sov-cli not found and cargo is unavailable; pass --sov-cli <path>"
  echo "sov-cli not found in target/{release,debug}; building release binary..." >&2
  (cd "$WORKSPACE_ROOT" && SKIP_GUEST_BUILD=1 cargo build -p sov-demo-rollup --bin sov-cli --release >/dev/null)
  [[ -x "$WORKSPACE_ROOT/target/release/sov-cli" ]] || fail "Failed to build sov-cli"
  echo "$WORKSPACE_ROOT/target/release/sov-cli"
}

derive_address_from_key() {
  local sov_cli_bin="$1"
  local sk_hex="$2"
  local tmp_key tmp_wallet show_json addr

  tmp_key="$(mktemp "${TMPDIR:-/tmp}/operator_key.XXXXXX")" || fail "Failed to create temp key file"
  tmp_wallet="$(mktemp -d "${TMPDIR:-/tmp}/operator_wallet.XXXXXX")" || fail "Failed to create temp wallet dir"

  make_key_file_from_hex "$sk_hex" "$tmp_key"
  chmod 600 "$tmp_key"

  SOV_WALLET_DIR="$tmp_wallet" "$sov_cli_bin" keys import --nickname generated --path "$tmp_key" --skip-if-present >/dev/null
  show_json="$(SOV_WALLET_DIR="$tmp_wallet" "$sov_cli_bin" keys show by-nickname generated)"
  addr="$(printf '%s' "$show_json" | jq -r '.address // empty')"

  rm -f "$tmp_key"
  rm -rf "$tmp_wallet"

  [[ -n "$addr" ]] || fail "Failed to derive address from generated key"
  [[ "$addr" =~ ^sov1[023456789acdefghjklmnpqrstuvwxyz]+$ ]] || fail "Derived invalid address: $addr"
  printf '%s' "$addr"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)
      OUTPUT="${2:-}"
      shift 2
      ;;
    --format)
      FORMAT="${2:-}"
      shift 2
      ;;
    --force)
      FORCE=1
      shift
      ;;
    --sov-cli)
      SOV_CLI_PATH="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "Unknown argument: $1"
      ;;
  esac
done

command -v jq >/dev/null 2>&1 || fail "Missing required command: jq"
command -v python3 >/dev/null 2>&1 || fail "Missing required command: python3"

[[ "$FORMAT" == "json" || "$FORMAT" == "dotenv" || "$FORMAT" == "export" ]] || fail "--format must be json, dotenv, or export"

if [[ -n "$OUTPUT" && -e "$OUTPUT" && "$FORCE" -ne 1 ]]; then
  fail "Output file already exists: $OUTPUT (use --force to overwrite)"
fi

SOV_CLI_BIN="$(resolve_sov_cli)"

ADMIN_WALLET_PRIVATE_KEY="$(rand_hex_32)"
ADMIN_ADDRESS="$(derive_address_from_key "$SOV_CLI_BIN" "$ADMIN_WALLET_PRIVATE_KEY")"

SEQUENCER_WALLET_PRIVATE_KEY="$(rand_hex_32)"
SEQUENCER_ADDRESS="$(derive_address_from_key "$SOV_CLI_BIN" "$SEQUENCER_WALLET_PRIVATE_KEY")"

PAYMASTER_WALLET_PRIVATE_KEY="$(rand_hex_32)"
PAYMASTER_ADDRESS="$(derive_address_from_key "$SOV_CLI_BIN" "$PAYMASTER_WALLET_PRIVATE_KEY")"

PROVER_WALLET_PRIVATE_KEY="$(rand_hex_32)"
PROVER_ADDRESS="$(derive_address_from_key "$SOV_CLI_BIN" "$PROVER_WALLET_PRIVATE_KEY")"

ATTESTER_WALLET_PRIVATE_KEY="$(rand_hex_32)"
ATTESTER_ADDRESS="$(derive_address_from_key "$SOV_CLI_BIN" "$ATTESTER_WALLET_PRIVATE_KEY")"

render() {
  if [[ "$FORMAT" == "json" ]]; then
    cat <<JSON
{
  "ADMIN_WALLET_PRIVATE_KEY": "$ADMIN_WALLET_PRIVATE_KEY",
  "ADMIN_ADDRESS": "$ADMIN_ADDRESS",
  "SEQUENCER_WALLET_PRIVATE_KEY": "$SEQUENCER_WALLET_PRIVATE_KEY",
  "SEQUENCER_ADDRESS": "$SEQUENCER_ADDRESS",
  "PAYMASTER_WALLET_PRIVATE_KEY": "$PAYMASTER_WALLET_PRIVATE_KEY",
  "PAYMASTER_ADDRESS": "$PAYMASTER_ADDRESS",
  "PROVER_WALLET_PRIVATE_KEY": "$PROVER_WALLET_PRIVATE_KEY",
  "PROVER_ADDRESS": "$PROVER_ADDRESS",
  "ATTESTER_WALLET_PRIVATE_KEY": "$ATTESTER_WALLET_PRIVATE_KEY",
  "ATTESTER_ADDRESS": "$ATTESTER_ADDRESS"
}
JSON
    return
  fi

  if [[ "$FORMAT" == "export" ]]; then
    cat <<EXPORT
export ADMIN_WALLET_PRIVATE_KEY="$ADMIN_WALLET_PRIVATE_KEY"
export ADMIN_ADDRESS="$ADMIN_ADDRESS"
export SEQUENCER_WALLET_PRIVATE_KEY="$SEQUENCER_WALLET_PRIVATE_KEY"
export SEQUENCER_ADDRESS="$SEQUENCER_ADDRESS"
export PAYMASTER_WALLET_PRIVATE_KEY="$PAYMASTER_WALLET_PRIVATE_KEY"
export PAYMASTER_ADDRESS="$PAYMASTER_ADDRESS"
export PROVER_WALLET_PRIVATE_KEY="$PROVER_WALLET_PRIVATE_KEY"
export PROVER_ADDRESS="$PROVER_ADDRESS"
export ATTESTER_WALLET_PRIVATE_KEY="$ATTESTER_WALLET_PRIVATE_KEY"
export ATTESTER_ADDRESS="$ATTESTER_ADDRESS"
EXPORT
    return
  fi

  cat <<DOTENV
ADMIN_WALLET_PRIVATE_KEY=$ADMIN_WALLET_PRIVATE_KEY
ADMIN_ADDRESS=$ADMIN_ADDRESS
SEQUENCER_WALLET_PRIVATE_KEY=$SEQUENCER_WALLET_PRIVATE_KEY
SEQUENCER_ADDRESS=$SEQUENCER_ADDRESS
PAYMASTER_WALLET_PRIVATE_KEY=$PAYMASTER_WALLET_PRIVATE_KEY
PAYMASTER_ADDRESS=$PAYMASTER_ADDRESS
PROVER_WALLET_PRIVATE_KEY=$PROVER_WALLET_PRIVATE_KEY
PROVER_ADDRESS=$PROVER_ADDRESS
ATTESTER_WALLET_PRIVATE_KEY=$ATTESTER_WALLET_PRIVATE_KEY
ATTESTER_ADDRESS=$ATTESTER_ADDRESS
DOTENV
}

if [[ -n "$OUTPUT" ]]; then
  mkdir -p "$(dirname "$OUTPUT")"
  umask 077
  render >"$OUTPUT"
  chmod 600 "$OUTPUT"
  echo "Wrote operator wallets to: $OUTPUT"
else
  render
fi
