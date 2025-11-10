#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

NODE_URL_DEFAULT="http://127.0.0.1:12346"
VERIFIER_URL_DEFAULT="http://127.0.0.1:8080"
NUM_DEPOSITS_DEFAULT=10
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
  --threads <n>            Tokio worker + blocking threads to use (default: detected CPU count)
  --debug                  Run tests without --release
  --release                Force --release (default)
  -h, --help               Show this help
EOF
}

NODE_URL="$NODE_URL_DEFAULT"
VERIFIER_URL="$VERIFIER_URL_DEFAULT"
NUM_DEPOSITS="$NUM_DEPOSITS_DEFAULT"

while (($#)); do
    case "$1" in
        --node-url)
            NODE_URL="$2"; shift 2;;
        --verifier-url)
            VERIFIER_URL="$2"; shift 2;;
        --num-deposits)
            NUM_DEPOSITS="$2"; shift 2;;
        --threads)
            TOKIO_THREADS="$2"; shift 2;;
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
echo "  Tokio threads: $TOKIO_THREADS"
echo ""

cd "$REPO_ROOT"

CARGO_CMD=(cargo run -p sov-rollup-ligero --bin e2e_runner_cli)
if [[ "$RUN_RELEASE" -eq 1 ]]; then
    CARGO_CMD+=(--release)
fi
CLI_ARGS=(--node-url "$NODE_URL" --verifier-url "$VERIFIER_URL" --num-deposits "$NUM_DEPOSITS")
CLI_ARGS+=("${EXTRA_RUN_ARGS[@]}")
CARGO_CMD+=(--)
CARGO_CMD+=("${CLI_ARGS[@]}")

"${CARGO_CMD[@]}"
