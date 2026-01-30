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

TEE_RESET="${TEE_RESET:-}"

ensure_genesis_dir() {
  if [[ ! -d "$TEE_GENESIS_DIR" ]]; then
    echo "Creating TEE genesis dir: $TEE_GENESIS_DIR_REL"
    cp -R "$TEE_GENESIS_SRC_DIR" "$TEE_GENESIS_DIR"
  fi

  python3 - <<PY
import json
from pathlib import Path

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
export SOV_TEE_SKIP_ORACLE="${SOV_TEE_SKIP_ORACLE:-1}"

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
echo "SOV_TEE_SKIP_ORACLE=$SOV_TEE_SKIP_ORACLE"
echo "ROLLUP_CARGO_FEATURES=$ROLLUP_CARGO_FEATURES"
echo ""
echo "Starting all services (run_all.sh)..."
echo ""

exec bash "$SCRIPT_DIR/run_all.sh" \
  --rollup-config-path "$TEE_ROLLUP_CONFIG_REL" \
  --genesis-config-dir "$TEE_GENESIS_DIR_REL" \
  "$@"
