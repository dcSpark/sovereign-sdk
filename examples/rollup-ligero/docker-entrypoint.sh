#!/usr/bin/env bash
set -euo pipefail

MCS_HOME="${MCS_HOME:-/mcs}"
MCS_CONFIG_DIR="${MCS_CONFIG_DIR:-/etc/mcs}"
MCS_SHARE_DIR="${MCS_SHARE_DIR:-/usr/share/mcs}"
REPLICA_DATA_DIR="${REPLICA_DATA_DIR:-$MCS_HOME/demo_data_replica}"
REPLICA_GENESIS_DIR="${REPLICA_GENESIS_DIR:-$REPLICA_DATA_DIR/genesis}"
GENESIS_SRC_DIR="${GENESIS_SRC_DIR:-$MCS_SHARE_DIR/test-data/genesis/demo/mock}"
ORACLE_KEYPAIR_ENV="${ORACLE_KEYPAIR_ENV:-/run/mcs/oracle_keypair.env}"
ORACLE_PUBKEY_ENV="${ORACLE_PUBKEY_ENV:-$MCS_CONFIG_DIR/oracle_pubkey.env}"
ORACLE_BIND="${ORACLE_SERVER_BIND_ADDRESS:-127.0.0.1:8090}"

ORACLE_PID=""
ROLLUP_PID=""

truthy() {
    case "${1:-}" in
        1|true|TRUE|yes|YES|on|ON) return 0 ;;
        *) return 1 ;;
    esac
}

local_oracle_enabled() {
    ! truthy "${NO_ORACLE:-0}" && truthy "${ENABLE_ORACLE:-0}"
}

cleanup() {
    trap - INT TERM EXIT
    if [[ -n "$ROLLUP_PID" ]] && kill -0 "$ROLLUP_PID" 2>/dev/null; then
        kill -TERM "$ROLLUP_PID" 2>/dev/null || true
    fi
    if [[ -n "$ORACLE_PID" ]] && kill -0 "$ORACLE_PID" 2>/dev/null; then
        kill -TERM "$ORACLE_PID" 2>/dev/null || true
    fi
}

trap cleanup INT TERM EXIT

wait_for_oracle() {
    local host="${ORACLE_BIND%:*}"
    local port="${ORACLE_BIND##*:}"

    if [[ "$host" == "0.0.0.0" || "$host" == "::" ]]; then
        host="127.0.0.1"
    fi

    for _ in $(seq 1 60); do
        if ! kill -0 "$ORACLE_PID" 2>/dev/null; then
            echo "ERROR: oracle exited before opening $host:$port" >&2
            exit 1
        fi
        if (echo >"/dev/tcp/$host/$port") >/dev/null 2>&1; then
            echo "oracle is listening on $host:$port"
            return
        fi
        sleep 1
    done

    echo "ERROR: timed out waiting for oracle on $host:$port" >&2
    exit 1
}

load_or_create_oracle_keypair() {
    mkdir -p "$(dirname "$ORACLE_KEYPAIR_ENV")"

    if [[ -f "$ORACLE_KEYPAIR_ENV" ]]; then
        # shellcheck disable=SC1090
        source "$ORACLE_KEYPAIR_ENV"
    fi

    if ! local_oracle_enabled; then
        if [[ -z "${TEE_ORACLE_PUBKEY_HEX:-}" && -f "$ORACLE_PUBKEY_ENV" ]]; then
            # shellcheck disable=SC1090
            source "$ORACLE_PUBKEY_ENV"
        fi

        if [[ -z "${TEE_ORACLE_PUBKEY_HEX:-}" ]]; then
            echo "ERROR: external oracle mode requires TEE_ORACLE_PUBKEY_HEX for genesis." >&2
            echo "Set it to the public key used by the oracle at tee_attestation_oracle_url." >&2
            exit 1
        fi
        export TEE_ORACLE_PUBKEY_HEX
        return
    fi

    if [[ -n "${ORACLE_SIGNING_KEY_HEX:-}" && -n "${TEE_ORACLE_PUBKEY_HEX:-}" ]]; then
        export ORACLE_SIGNING_KEY_HEX TEE_ORACLE_PUBKEY_HEX
        return
    fi

    if [[ -n "${ORACLE_SIGNING_KEY_HEX:-}" || -n "${TEE_ORACLE_PUBKEY_HEX:-}" ]]; then
        echo "ERROR: set both ORACLE_SIGNING_KEY_HEX and TEE_ORACLE_PUBKEY_HEX, or set neither." >&2
        exit 1
    fi

    echo "generating oracle keypair at $ORACLE_KEYPAIR_ENV"
    local tmpdir priv_hex pub_hex
    tmpdir="$(mktemp -d)"
    openssl genpkey -algorithm ED25519 -out "$tmpdir/key.pem" >/dev/null 2>&1

    priv_hex="$(openssl pkey -in "$tmpdir/key.pem" -text -noout | awk '
        /^priv:/{flag=1;next}
        /^pub:/{flag=0}
        flag{gsub(/[^0-9a-fA-F]/,""); printf $0}
        END{print ""}')"
    pub_hex="$(openssl pkey -in "$tmpdir/key.pem" -text -noout | awk '
        /^pub:/{flag=1;next}
        flag{gsub(/[^0-9a-fA-F]/,""); printf $0}
        END{print ""}')"

    rm -rf "$tmpdir"

    if [[ ${#priv_hex} -ne 64 || ${#pub_hex} -ne 64 ]]; then
        echo "ERROR: failed to parse generated oracle keypair" >&2
        exit 1
    fi

    cat >"$ORACLE_KEYPAIR_ENV" <<EOF
ORACLE_SIGNING_KEY_HEX=0x$priv_hex
TEE_ORACLE_PUBKEY_HEX=0x$pub_hex
EOF

    # shellcheck disable=SC1090
    source "$ORACLE_KEYPAIR_ENV"
    export ORACLE_SIGNING_KEY_HEX TEE_ORACLE_PUBKEY_HEX
}

prepare_replica_genesis() {
    if truthy "${REPLICA_RESET:-0}"; then
        rm -rf "$REPLICA_DATA_DIR"
    fi

    mkdir -p "$REPLICA_DATA_DIR"
    if [[ ! -d "$REPLICA_GENESIS_DIR" ]]; then
        cp -R "$GENESIS_SRC_DIR" "$REPLICA_GENESIS_DIR"
    fi

    load_or_create_oracle_keypair

    python3 - <<PY
import json
from pathlib import Path

genesis = Path(r"""$REPLICA_GENESIS_DIR""")

p = genesis / "prover_incentives.json"
d = json.loads(p.read_text())
d["tee_oracle_pubkeys"] = [r"""$TEE_ORACLE_PUBKEY_HEX"""]
p.write_text(json.dumps(d, indent=4) + "\\n")

p = genesis / "chain_state.json"
d = json.loads(p.read_text())
d["operating_mode"] = "tee"
p.write_text(json.dumps(d, indent=4) + "\\n")
PY
}

start_oracle_if_enabled() {
    if ! local_oracle_enabled; then
        echo "oracle disabled"
        return
    fi

    export ORACLE_SERVER_BIND_ADDRESS="$ORACLE_BIND"
    export ORACLE_SIGNING_KEY_HEX
    export ORACLE_DEV_ACCEPT_ALL="${ORACLE_DEV_ACCEPT_ALL:-false}"
    export ORACLE_POLICIES_DIR="${ORACLE_POLICIES_DIR:-$MCS_CONFIG_DIR/oracle-policies}"

    echo "starting oracle on $ORACLE_BIND"
    oracle &
    ORACLE_PID="$!"
    wait_for_oracle
}

if [[ "${1:-}" == "replica" ]]; then
    shift
elif [[ "${1:-}" == "bash" || "${1:-}" == "sh" || "${1:-}" == "oracle" || "${1:-}" == "sov-rollup-ligero" ]]; then
    exec "$@"
fi

export RUST_LOG="${RUST_LOG:-info}"
export SOV_TEE_MOCK_ATTESTATION="${SOV_TEE_MOCK_ATTESTATION:-0}"
export SOV_TEE_ATTESTATION_URL="${SOV_TEE_ATTESTATION_URL:-http://attestation:8000/}"
export LIGERO_PACKING="${LIGERO_PACKING:-8192}"

prepare_replica_genesis
start_oracle_if_enabled

echo "starting replica"
sov-rollup-ligero \
    --rollup-config-path "${ROLLUP_CONFIG_PATH:-$MCS_CONFIG_DIR/rollup_config_replica.toml}" \
    --prometheus-exporter-bind "${PROMETHEUS_EXPORTER_BIND:-0.0.0.0:13201}" \
    --genesis-config-dir "$REPLICA_GENESIS_DIR" \
    "$@" &

ROLLUP_PID="$!"
wait "$ROLLUP_PID"
