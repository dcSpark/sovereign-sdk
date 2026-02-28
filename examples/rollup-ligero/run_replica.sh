#!/usr/bin/env bash
set -euo pipefail

# Self-contained script to run a read-only replica node with TEE support
# Generates its own oracle keypair and genesis - does not depend on tee_local.sh

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Replica-specific paths
REPLICA_DATA_DIR_REL="${REPLICA_DATA_DIR_REL:-demo_data_replica}"
REPLICA_DATA_DIR="$SCRIPT_DIR/$REPLICA_DATA_DIR_REL"

REPLICA_GENESIS_DIR_REL="${REPLICA_GENESIS_DIR_REL:-$REPLICA_DATA_DIR_REL/genesis}"
REPLICA_GENESIS_DIR="$SCRIPT_DIR/$REPLICA_GENESIS_DIR_REL"
GENESIS_SRC_DIR="$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock"

ORACLE_KEYPAIR_ENV_REL="${ORACLE_KEYPAIR_ENV_REL:-$REPLICA_DATA_DIR_REL/oracle_keypair.env}"
ORACLE_KEYPAIR_ENV="$SCRIPT_DIR/$ORACLE_KEYPAIR_ENV_REL"

# Oracle configuration for replica
REPLICA_ORACLE_PORT="${REPLICA_ORACLE_PORT:-8090}"
REPLICA_ORACLE_BIND="${REPLICA_ORACLE_BIND:-127.0.0.1:$REPLICA_ORACLE_PORT}"

# Reset flag - set REPLICA_RESET=1 to wipe and regenerate
REPLICA_RESET="${REPLICA_RESET:-}"

# Track child processes for cleanup
PIDS=()

cleanup() {
    local exit_code=$?
    trap - INT TERM EXIT
    set +e
    if [ "${#PIDS[@]}" -gt 0 ]; then
        echo ""
        echo "Stopping replica services..."
        for pid in "${PIDS[@]}"; do
            if kill -0 "$pid" 2>/dev/null; then
                echo "  - Stopping pid $pid"
                kill -TERM "$pid" 2>/dev/null || true
            fi
        done
        sleep 2
        for pid in "${PIDS[@]}"; do
            if kill -0 "$pid" 2>/dev/null; then
                kill -KILL "$pid" 2>/dev/null || true
            fi
        done
    fi
    exit "$exit_code"
}

trap cleanup INT TERM EXIT

wait_for_port() {
    local name="$1"
    local host="$2"
    local port="$3"
    local pid="$4"
    local timeout="${5:-60}"
    local waited=0

    if [[ "$host" == "0.0.0.0" || "$host" == "::" ]]; then
        host="127.0.0.1"
    fi

    while (( waited < timeout )); do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "Error: $name exited before opening $host:$port"
            return 1
        fi
        if (echo > "/dev/tcp/$host/$port") >/dev/null 2>&1; then
            echo "$name is listening on $host:$port"
            return 0
        fi
        sleep 1
        waited=$((waited + 1))
    done

    echo "Error: Timed out waiting for $name on $host:$port"
    return 1
}

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
        echo "Failed to extract oracle keypair from openssl output"
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
    if [[ ! -d "$REPLICA_GENESIS_DIR" ]]; then
        echo "Creating replica genesis dir: $REPLICA_GENESIS_DIR_REL"
        mkdir -p "$REPLICA_DATA_DIR"
        cp -R "$GENESIS_SRC_DIR" "$REPLICA_GENESIS_DIR"
    fi

    ensure_oracle_keypair

    python3 - <<PY
import json
from pathlib import Path

# Update prover_incentives.json with oracle pubkey
p = Path(r"""$REPLICA_GENESIS_DIR""") / "prover_incentives.json"
pi = json.loads(p.read_text())
pi["tee_oracle_pubkeys"] = [r"""$TEE_ORACLE_PUBKEY_HEX"""]
p.write_text(json.dumps(pi, indent=4) + "\\n")
print("Updated tee_oracle_pubkeys in", p)

# Update chain_state.json with operating_mode=tee
p = Path(r"""$REPLICA_GENESIS_DIR""") / "chain_state.json"
d = json.loads(p.read_text())
d["operating_mode"] = "tee"
p.write_text(json.dumps(d, indent=4) + "\n")
print("Updated operating_mode=tee in", p)
PY
}

# Handle reset
if [[ -n "$REPLICA_RESET" ]]; then
    echo "REPLICA_RESET is set; wiping $REPLICA_DATA_DIR_REL"
    rm -rf "$REPLICA_DATA_DIR"
fi

# Create data directory
mkdir -p "$REPLICA_DATA_DIR"

# Setup genesis and oracle keypair
ensure_genesis_dir

# Ligero configuration
export LIGERO_PACKING=8192

# Default log level
export RUST_LOG="${RUST_LOG:-info}"

# TEE mock attestation - enables mock mode for TEE verification
export SOV_TEE_MOCK_ATTESTATION="${SOV_TEE_MOCK_ATTESTATION:-1}"

ROLLUP_BIN="$WORKSPACE_ROOT/target/release/sov-rollup-ligero"
ORACLE_BIN="$WORKSPACE_ROOT/target/release/oracle"

if [[ ! -f "$ROLLUP_BIN" ]]; then
  echo "ERROR: Rollup binary not found at $ROLLUP_BIN"
  echo "Run: cargo build --release -p sov-rollup-ligero --features sov-modules-rollup-blueprint/tee"
  exit 1
fi

if [[ ! -f "$ORACLE_BIN" ]]; then
  echo "ERROR: Oracle binary not found at $ORACLE_BIN"
  echo "Run: cargo build --release -p oracle"
  exit 1
fi

# Configure oracle
export ORACLE_SERVER_BIND_ADDRESS="$REPLICA_ORACLE_BIND"
export ORACLE_SIGNING_KEY_HEX="${ORACLE_SIGNING_KEY_HEX}"
export ORACLE_DEV_ACCEPT_ALL="${ORACLE_DEV_ACCEPT_ALL:-true}"
export ORACLE_POLICIES_DIR="$SCRIPT_DIR/policies"

echo ""
echo "Starting oracle for replica..."
echo "  Oracle Bind: $REPLICA_ORACLE_BIND"

"$ORACLE_BIN" &
ORACLE_PID=$!
PIDS+=("$ORACLE_PID")

# Wait for oracle to be ready
ORACLE_HOST="${REPLICA_ORACLE_BIND%:*}"
wait_for_port "oracle" "$ORACLE_HOST" "$REPLICA_ORACLE_PORT" "$ORACLE_PID"

echo ""
echo "========================================"
echo "Replica Node (TEE mode)"
echo "========================================"
echo "  Config: rollup_config_replica.toml"
echo "  API Port: 12347"
echo "  Prometheus Port: 13201"
echo "  Storage: $REPLICA_DATA_DIR_REL/"
echo "  Genesis: $REPLICA_GENESIS_DIR_REL"
echo "  Oracle: $REPLICA_ORACLE_BIND (pid $ORACLE_PID)"
echo "  SOV_TEE_MOCK_ATTESTATION=$SOV_TEE_MOCK_ATTESTATION"
echo ""

cd "$WORKSPACE_ROOT/examples/rollup-ligero"

# Use different Prometheus port than primary (13200) to allow running both on same machine
# Note: We use a regular command (not exec) so the cleanup trap can stop the oracle
"$ROLLUP_BIN" \
    --rollup-config-path rollup_config_replica.toml \
    --prometheus-exporter-bind "0.0.0.0:13201" \
    --genesis-config-dir "$REPLICA_GENESIS_DIR_REL" \
    "$@"
