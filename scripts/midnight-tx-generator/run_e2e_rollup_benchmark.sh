#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

NODE_URL_DEFAULT="http://127.0.0.1:12346"
VERIFIER_URL_DEFAULT="http://127.0.0.1:8080"
NUM_DEPOSITS_DEFAULT=10
WALLET_SOURCE_DEFAULT="${E2E_WALLET_SOURCE:-${CONTINUOUS_WALLET_SOURCE:-genesis}}"
DYNAMIC_FUND_GAS_RESERVE_DEFAULT="${E2E_DYNAMIC_FUND_GAS_RESERVE:-${DYNAMIC_FUND_GAS_RESERVE:-1000000}}"
RUN_RELEASE=1
TOKIO_THREADS=""
EXTRA_RUN_ARGS=()

print_usage() {
    cat <<'EOF'
Usage: run_e2e_rollup_benchmark.sh [options] [-- <additional test args>]

Options:
  --node-url <url>         Sequencer REST URL (default: http://127.0.0.1:12346)
  --verifier-url <url>     Proof verifier endpoint (default: http://127.0.0.1:8080)
  --num-deposits <n>       Number of deposits/transfers to submit (default: 10)
  --wallet-source <mode>   Wallet source: genesis|dynamic (default: env or genesis)
  --dynamic-fund-gas-reserve <n>
                           Extra gas token reserve per wallet in dynamic mode
                           (default: env or 1000000)
  --deposits-only          Submit deposits only (skip transfer phase)
  --threads <n>            Tokio worker + blocking threads to use (default: detected CPU count)
  --cache                  Enable proof caching (disabled by default)
  --verify                 Enable local proof verification (disabled by default)
  --debug                  Run tests without --release
  --release                Force --release (default)
  -h, --help               Show this help
EOF
}

NODE_URL="$NODE_URL_DEFAULT"
VERIFIER_URL="$VERIFIER_URL_DEFAULT"
NUM_DEPOSITS="$NUM_DEPOSITS_DEFAULT"
WALLET_SOURCE="$WALLET_SOURCE_DEFAULT"
DYNAMIC_FUND_GAS_RESERVE="$DYNAMIC_FUND_GAS_RESERVE_DEFAULT"
USE_CACHE=0
USE_VERIFY=0
DEPOSITS_ONLY=0

while (($#)); do
    case "$1" in
        --node-url)
            NODE_URL="$2"; shift 2;;
        --verifier-url)
            VERIFIER_URL="$2"; shift 2;;
        --num-deposits)
            NUM_DEPOSITS="$2"; shift 2;;
        --wallet-source)
            WALLET_SOURCE="$2"; shift 2;;
        --dynamic-fund-gas-reserve)
            DYNAMIC_FUND_GAS_RESERVE="$2"; shift 2;;
        --deposits-only)
            DEPOSITS_ONLY=1; shift;;
        --threads)
            TOKIO_THREADS="$2"; shift 2;;
        --cache)
            USE_CACHE=1; shift;;
        --verify)
            USE_VERIFY=1; shift;;
        --debug)
            RUN_RELEASE=0; shift;;
        --release)
            RUN_RELEASE=1; shift;;
        -h|--help)
            print_usage; exit 0;;
        --)
            shift
            EXTRA_RUN_ARGS=("$@")
            break;;
        *)
            echo "Unknown option: $1" >&2
            print_usage
            exit 1;;
    esac
done

case "$WALLET_SOURCE" in
    genesis|dynamic) ;;
    *)
        echo "Invalid --wallet-source '$WALLET_SOURCE' (expected: genesis|dynamic)" >&2
        exit 1
        ;;
esac

if [[ "$WALLET_SOURCE" == "dynamic" ]]; then
    if [[ -z "${ADMIN_WALLET_PRIVATE_KEY:-}" ]]; then
        echo "ADMIN_WALLET_PRIVATE_KEY is required when --wallet-source dynamic" >&2
        exit 1
    fi
else
    GENESIS_KEYPAIRS_PATH="$REPO_ROOT/examples/test-data/genesis/demo/mock/generated_keypairs.json"
    if [[ ! -f "$GENESIS_KEYPAIRS_PATH" ]]; then
        echo "Missing genesis keypairs at $GENESIS_KEYPAIRS_PATH" >&2
        echo "Use --wallet-source dynamic (with ADMIN_WALLET_PRIVATE_KEY) or generate demo keypairs." >&2
        exit 1
    fi
fi

if [[ -z "$TOKIO_THREADS" ]]; then
    if command -v nproc >/dev/null 2>&1; then
        TOKIO_THREADS="$(nproc)"
    else
        TOKIO_THREADS="$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"
    fi
fi

export TOKIO_WORKER_THREADS="$TOKIO_THREADS"
export TOKIO_MAX_BLOCKING_THREADS="$TOKIO_THREADS"

echo "Running e2e benchmark with:"
echo "  Node URL:      $NODE_URL"
echo "  Verifier URL:  $VERIFIER_URL"
echo "  Deposits:      $NUM_DEPOSITS"
echo "  Wallet source: $WALLET_SOURCE"
if [[ "$WALLET_SOURCE" == "dynamic" ]]; then
    echo "  Dyn reserve:   $DYNAMIC_FUND_GAS_RESERVE"
fi
echo "  Deposits only: $(if [[ "$DEPOSITS_ONLY" -eq 1 ]]; then echo "yes"; else echo "no"; fi)"
echo "  Tokio threads: $TOKIO_THREADS"
echo "  Proof cache:   $(if [[ "$USE_CACHE" -eq 1 ]]; then echo "enabled"; else echo "disabled"; fi)"
echo "  Verification:  $(if [[ "$USE_VERIFY" -eq 1 ]]; then echo "enabled"; else echo "disabled"; fi)"
echo ""

cd "$REPO_ROOT"

CARGO_CMD=(cargo run -p midnight-e2e-benchmarks --bin e2e_runner_cli)
if [[ "$RUN_RELEASE" -eq 1 ]]; then
    CARGO_CMD+=(--release)
fi
CLI_ARGS=(--node-url "$NODE_URL" --verifier-url "$VERIFIER_URL" --num-deposits "$NUM_DEPOSITS")
CLI_ARGS+=(--wallet-source "$WALLET_SOURCE")
if [[ "$WALLET_SOURCE" == "dynamic" ]]; then
    CLI_ARGS+=(--dynamic-fund-gas-reserve "$DYNAMIC_FUND_GAS_RESERVE")
fi
if [[ "$USE_CACHE" -eq 1 ]]; then
    CLI_ARGS+=(--cache)
fi
if [[ "$USE_VERIFY" -eq 1 ]]; then
    CLI_ARGS+=(--verify)
fi
if [[ "$DEPOSITS_ONLY" -eq 1 ]]; then
    CLI_ARGS+=(--deposits-only)
fi
if [[ "${#EXTRA_RUN_ARGS[@]}" -gt 0 ]]; then
    CLI_ARGS+=("${EXTRA_RUN_ARGS[@]}")
fi
CARGO_CMD+=(--)
CARGO_CMD+=("${CLI_ARGS[@]}")

"${CARGO_CMD[@]}"
