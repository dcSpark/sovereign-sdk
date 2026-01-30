#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

TEE_DATA_DIR_REL="${TEE_DATA_DIR_REL:-demo_data_tee}"
TEE_DATA_DIR="$SCRIPT_DIR/$TEE_DATA_DIR_REL"

TEE_ROLLUP_CONFIG_REL="${TEE_ROLLUP_CONFIG_REL:-rollup_config_tee_local.toml}"
TEE_ROLLUP_CONFIG="$SCRIPT_DIR/$TEE_ROLLUP_CONFIG_REL"

TEE_GENESIS_DIR_REL="${TEE_GENESIS_DIR_REL:-$TEE_DATA_DIR_REL/genesis}"
TEE_GENESIS_DIR="$SCRIPT_DIR/$TEE_GENESIS_DIR_REL"
TEE_GENESIS_SRC_DIR="$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock"

ORACLE_KEYPAIR_ENV_REL="${ORACLE_KEYPAIR_ENV_REL:-$TEE_DATA_DIR_REL/oracle_keypair.env}"
ORACLE_KEYPAIR_ENV="$SCRIPT_DIR/$ORACLE_KEYPAIR_ENV_REL"

TEE_RESET="${TEE_RESET:-}"

ensure_oracle_keypair() {
  if [[ -f "$ORACLE_KEYPAIR_ENV" ]]; then
    # shellcheck disable=SC1090
    source "$ORACLE_KEYPAIR_ENV"
    return
  fi

  echo "Generating local oracle Ed25519 keypair: $ORACLE_KEYPAIR_ENV_REL"
  local tmpdir
  tmpdir="$(mktemp -d)"
  openssl genpkey -algorithm ED25519 -out "$tmpdir/key.pem" >/dev/null 2>&1

  local priv_hex pub_hex
  priv_hex="$(
    openssl pkey -in "$tmpdir/key.pem" -text -noout | awk '
      BEGIN{flag=0}
      /^priv:/{flag=1;next}
      /^pub:/{flag=0}
      flag{gsub(/[^0-9a-fA-F]/,""); printf $0}
      END{print ""}'
  )"
  pub_hex="$(
    openssl pkey -in "$tmpdir/key.pem" -text -noout | awk '
      BEGIN{flag=0}
      /^pub:/{flag=1;next}
      flag{gsub(/[^0-9a-fA-F]/,""); printf $0}
      END{print ""}'
  )"

  rm -rf "$tmpdir"

  if [[ ${#priv_hex} -ne 64 || ${#pub_hex} -ne 64 ]]; then
    echo "❌ Failed to extract oracle keypair from openssl output"
    echo "priv_hex_len=${#priv_hex} pub_hex_len=${#pub_hex}"
    exit 1
  fi

  cat >"$ORACLE_KEYPAIR_ENV" <<EOF
ORACLE_SIGNING_KEY_HEX=0x$priv_hex
TEE_ORACLE_PUBKEY_HEX=0x$pub_hex
EOF

  # shellcheck disable=SC1090
  source "$ORACLE_KEYPAIR_ENV"
}

ensure_genesis_dir() {
  if [[ ! -d "$TEE_GENESIS_DIR" ]]; then
    echo "Creating TEE genesis dir: $TEE_GENESIS_DIR_REL"
    cp -R "$TEE_GENESIS_SRC_DIR" "$TEE_GENESIS_DIR"
  fi

  ensure_oracle_keypair

  python3 - <<PY
import json
from pathlib import Path

p = Path(r"""$TEE_GENESIS_DIR""") / "prover_incentives.json"
pi = json.loads(p.read_text())
pi["tee_oracle_pubkeys"] = [r"""$TEE_ORACLE_PUBKEY_HEX"""]
p.write_text(json.dumps(pi, indent=4) + "\\n")
print("Updated tee_oracle_pubkeys in", p)

p = Path(r"""$TEE_GENESIS_DIR""") / "chain_state.json"
d = json.loads(p.read_text())
d["operating_mode"] = "tee"
p.write_text(json.dumps(d, indent=4) + "\n")
print("Updated operating_mode=tee in", p)
PY
}

ensure_rollup_config() {
  if [[ ! -f "$TEE_ROLLUP_CONFIG" ]]; then
    echo "Creating TEE rollup config: $TEE_ROLLUP_CONFIG_REL"
    cp "$SCRIPT_DIR/rollup_config.toml" "$TEE_ROLLUP_CONFIG"
  fi

  python3 - <<PY
from pathlib import Path

p = Path(r"""$TEE_ROLLUP_CONFIG""")
s = p.read_text()
s = s.replace('connection_string = "sqlite://demo_data/da.sqlite?mode=rwc"',
              'connection_string = "sqlite://$TEE_DATA_DIR_REL/da.sqlite?mode=rwc"')
s = s.replace('path = "demo_data"', 'path = "$TEE_DATA_DIR_REL"')
s = s.replace('worker_tx_path = "demo_data/midnight_tx_json"',
              'worker_tx_path = "$TEE_DATA_DIR_REL/midnight_tx_json"')
p.write_text(s)
print("Patched", p)
PY
}

if [[ -n "$TEE_RESET" ]]; then
  echo "TEE_RESET is set; wiping $TEE_DATA_DIR_REL"
  rm -rf "$TEE_DATA_DIR"
fi

mkdir -p "$TEE_DATA_DIR/midnight_tx_json"

ensure_genesis_dir
ensure_rollup_config

# Enable mock attestation by default for local dev (no Azure/MAA required).
export SOV_TEE_MOCK_ATTESTATION="${SOV_TEE_MOCK_ATTESTATION:-1}"

# Start a local oracle that signs attestations. In mock mode, we configure it to accept all.
export START_ORACLE="${START_ORACLE:-1}"
export ORACLE_DEV_ACCEPT_ALL="${ORACLE_DEV_ACCEPT_ALL:-1}"
ensure_oracle_keypair
export ORACLE_SIGNING_KEY_HEX="${ORACLE_SIGNING_KEY_HEX}"

# Make aux services (indexer/metrics) use the same DA DB + index DB.
export DA_CONNECTION_STRING="${DA_CONNECTION_STRING:-sqlite://${TEE_DATA_DIR_REL}/da.sqlite?mode=rwc}"
export INDEX_DB="${INDEX_DB:-sqlite://${TEE_DATA_DIR_REL}/wallet_index.sqlite?mode=rwc}"
export INDEXER_DB_CONNECTION_STRING="${INDEXER_DB_CONNECTION_STRING:-$INDEX_DB}"

# Make verifier service read the same rollup config.
export ROLLUP_CONFIG_PATH="${ROLLUP_CONFIG_PATH:-$TEE_ROLLUP_CONFIG}"

# Make run_rollup.sh seed assets into the correct directory.
export ROLLUP_DATA_DIR="${ROLLUP_DATA_DIR:-$TEE_DATA_DIR_REL}"

# Build the rollup with TEE support enabled.
export ROLLUP_CARGO_FEATURES="${ROLLUP_CARGO_FEATURES:-sov-modules-rollup-blueprint/tee}"

echo ""
echo "========================================"
echo "TEE Local (mock attestation)"
echo "========================================"
echo "Genesis dir:        $TEE_GENESIS_DIR_REL"
echo "Rollup config:      $TEE_ROLLUP_CONFIG_REL"
echo "Data dir:           $TEE_DATA_DIR_REL"
echo "SOV_TEE_MOCK_ATTESTATION=$SOV_TEE_MOCK_ATTESTATION"
echo "START_ORACLE=$START_ORACLE"
echo "ORACLE_DEV_ACCEPT_ALL=$ORACLE_DEV_ACCEPT_ALL"
echo "ORACLE_SIGNING_KEY_HEX=(set)"
echo "ROLLUP_CARGO_FEATURES=$ROLLUP_CARGO_FEATURES"
echo ""
echo "Starting all services (run_all.sh)..."
echo ""

exec bash "$SCRIPT_DIR/run_all.sh" \
  --rollup-config-path "$TEE_ROLLUP_CONFIG_REL" \
  --genesis-config-dir "$TEE_GENESIS_DIR_REL" \
  "$@"
