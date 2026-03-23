#!/bin/sh
set -eu

usage() {
  cat >&2 <<'EOF'
Usage:
  scripts/mcp-external-stress.sh --wallets <N> --txs <N> [options]

Required:
  --wallets <N>        Number of concurrent MCP sessions (one wallet per session)
  --txs <N>            Transactions per wallet session

Options:
  --endpoint <URL>     MCP endpoint (default: https://midnight-l2-testnet.shinkai.com/mcp/mcp)
  --session-ids-file <PATH>
                      File with stable MCP session IDs to reuse across runs
                      (default: <repo>/.mcp-external-stress-session-ids.txt)
  --send-amount <N>    Amount (dust) per tx (default: 1000)
  --confirm            Poll confirmation (adds extra load)
  --rust-log <SPEC>    Override RUST_LOG (default: mcp_external_stress=info,rmcp=warn)
  -h, --help           Show help

Environment variables (optional, flags override):
  MCP_ENDPOINT
  MCP_STRESS_SESSION_IDS_FILE
  SEND_AMOUNT
  CONFIRM
  MCP_STRESS_RUST_LOG
EOF
}

WALLETS=""
TXS_PER_WALLET=""
SESSION_IDS_FILE=""

MCP_ENDPOINT="${MCP_ENDPOINT:-https://midnight-l2-testnet.shinkai.com/mcp/mcp}"
SEND_AMOUNT="${SEND_AMOUNT:-1000}"
CONFIRM="${CONFIRM:-0}"
RUST_LOG="${MCP_STRESS_RUST_LOG:-mcp_external_stress=info,rmcp=warn}"

# Backwards-compatible positional args: <wallets> <txs-per-wallet>
if [ "$#" -eq 2 ] && [ "${1#-}" = "$1" ] && [ "${2#-}" = "$2" ]; then
  WALLETS="$1"
  TXS_PER_WALLET="$2"
  shift 2
else
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --wallets|-w)
        WALLETS="${2:-}"
        shift 2
        ;;
      --txs|--txs-per-wallet|-t)
        TXS_PER_WALLET="${2:-}"
        shift 2
        ;;
      --endpoint|--mcp-endpoint)
        MCP_ENDPOINT="${2:-}"
        shift 2
        ;;
      --session-ids-file)
        SESSION_IDS_FILE="${2:-}"
        shift 2
        ;;
      --send-amount)
        SEND_AMOUNT="${2:-}"
        shift 2
        ;;
      --confirm)
        CONFIRM=1
        shift
        ;;
      --rust-log)
        RUST_LOG="${2:-}"
        shift 2
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      --)
        shift
        break
        ;;
      *)
        echo "Unknown argument: $1" >&2
        usage
        exit 2
        ;;
    esac
  done
fi

if [ -z "$WALLETS" ] || [ -z "$TXS_PER_WALLET" ]; then
  usage
  exit 2
fi

case "$WALLETS" in
  *[!0-9]*|'') echo "--wallets must be a non-negative integer" >&2; exit 2 ;;
esac
case "$TXS_PER_WALLET" in
  *[!0-9]*|'') echo "--txs must be a non-negative integer" >&2; exit 2 ;;
esac

export RUST_LOG

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

SESSION_IDS_FILE="${SESSION_IDS_FILE:-${MCP_STRESS_SESSION_IDS_FILE:-$REPO_ROOT/.mcp-external-stress-session-ids.txt}}"

# --- Tree-rebuild watcher ------------------------------------------------
# Tail the mcp-external server log in the background and surface any lines
# that indicate a full tree rebuild.  Only active while this script runs.
MCP_SERVER_LOG="${MCP_SERVER_LOG:-$REPO_ROOT/logs/mcp-external.log}"
_rebuild_watcher_pid=""

_cleanup_watcher() {
  if [ -n "$_rebuild_watcher_pid" ] && kill -0 "$_rebuild_watcher_pid" 2>/dev/null; then
    kill "$_rebuild_watcher_pid" 2>/dev/null || true
    wait "$_rebuild_watcher_pid" 2>/dev/null || true
  fi
}
trap _cleanup_watcher EXIT INT TERM

_extract_field() {
  # Extract a key=value field from a tracing log line.  Usage: _extract_field "key" "$line"
  echo "$1" | sed -n "s/.*$2=\([^ ]*\).*/\1/p"
}

if [ -f "$MCP_SERVER_LOG" ]; then
  # Watch for full-rebuild / cache-reset indicators in the server log.
  # Uses tail -f so it only prints NEW lines written after we start.
  tail -n 0 -f "$MCP_SERVER_LOG" 2>/dev/null \
    | while IFS= read -r line; do
        reason=""
        detail=""
        color="33"  # default: yellow (warning)

        case "$line" in
          *"root mismatch"*"falling back to full rebuild"*)
            reason="INCREMENTAL ROOT MISMATCH"
            rebuilt=$(_extract_field "$line" "rebuilt_root")
            expected=$(_extract_field "$line" "expected_root")
            attempt=$(_extract_field "$line" "attempt")
            max=$(_extract_field "$line" "max_attempts")
            notes=$(_extract_field "$line" "fetched_notes")
            start=$(_extract_field "$line" "start_offset")
            target=$(_extract_field "$line" "target_next_position")
            elapsed=$(_extract_field "$line" "elapsed_ms")
            detail="Incremental append produced wrong root — rebuilding entire tree from scratch."
            detail="$detail
         rebuilt_root=$rebuilt  expected_root=$expected
         start_offset=$start  target_next_position=$target  fetched_notes=$notes
         attempt=$attempt/$max  elapsed=${elapsed}ms"
            color="33" ;;

          *"incremental sync failed"*"falling back to full rebuild"*)
            reason="INCREMENTAL SYNC ERROR"
            attempt=$(_extract_field "$line" "attempt")
            max=$(_extract_field "$line" "max_attempts")
            cached_next=$(_extract_field "$line" "cached_next_position")
            notes_next=$(_extract_field "$line" "notes_next_position")
            cached_root=$(_extract_field "$line" "cached_root")
            expected_root=$(_extract_field "$line" "expected_root")
            detail="Incremental append threw an error — falling back to full rebuild."
            detail="$detail
         cached_next=$cached_next  notes_next=$notes_next
         cached_root=$cached_root  expected_root=$expected_root
         attempt=$attempt/$max" ;;

          *"depth mismatch"*"resetting"*)
            reason="DEPTH MISMATCH"
            cached=$(_extract_field "$line" "cached_depth")
            chain=$(_extract_field "$line" "chain_depth")
            detail="Chain tree depth changed ($cached -> $chain) — entire cache invalidated."
            color="31" ;;

          *"rewound"*"resetting"*"cache"*)
            reason="EVENT STREAM REWIND"
            cached_id=$(_extract_field "$line" "cached_event_id")
            db_id=$(_extract_field "$line" "db_event_id")
            detail="Indexer event stream went backwards (cached=$cached_id, db=$db_id) — cache invalidated."
            color="31" ;;

          *"full rebuild root mismatch"*)
            reason="ROOT MISMATCH AFTER REBUILD"
            rebuilt=$(_extract_field "$line" "rebuilt_root")
            expected=$(_extract_field "$line" "expected_root")
            target=$(_extract_field "$line" "target_next_position")
            notes=$(_extract_field "$line" "fetched_notes")
            detail="Tree rebuilt from scratch but root still doesn't match chain — possible data inconsistency!"
            detail="$detail
         rebuilt_root=$rebuilt  expected_root=$expected
         target_next_position=$target  fetched_notes=$notes"
            color="31" ;;

          *"full rebuild failed"*)
            reason="REBUILD FAILED"
            trigger=$(_extract_field "$line" "rebuild_trigger")
            depth=$(_extract_field "$line" "depth")
            cap=$(_extract_field "$line" "capacity")
            delta=$(_extract_field "$line" "delta")
            threshold=$(_extract_field "$line" "full_rebuild_threshold")
            attempt=$(_extract_field "$line" "attempt")
            max=$(_extract_field "$line" "max_attempts")
            cached_root=$(_extract_field "$line" "cached_root")
            expected_root=$(_extract_field "$line" "expected_root")
            cached_next=$(_extract_field "$line" "cached_next_position")
            notes_next=$(_extract_field "$line" "notes_next_position")
            detail="Full tree reconstruction failed — will retry."
            detail="$detail
         trigger=$trigger  depth=$depth  capacity=$cap  delta=$delta  threshold=$threshold
         cached_next=$cached_next  notes_next=$notes_next
         cached_root=$cached_root  expected_root=$expected_root
         attempt=$attempt/$max"
            color="31" ;;

          *"snapshot rebuild panicked"*|*"panicked during full rebuild"*)
            reason="REBUILD PANICKED"
            detail="Tree reconstruction hit a panic — this is a bug."
            color="31" ;;

          *"synced (full rebuild)"*)
            reason="FULL REBUILD OK"
            trigger=$(_extract_field "$line" "rebuild_trigger")
            depth=$(_extract_field "$line" "depth")
            cap=$(_extract_field "$line" "capacity")
            cached_before=$(_extract_field "$line" "cached_next_before")
            delta=$(_extract_field "$line" "delta")
            threshold=$(_extract_field "$line" "full_rebuild_threshold")
            mem=$(_extract_field "$line" "tree_mem_bytes")
            target=$(_extract_field "$line" "target_next_position")
            notes=$(_extract_field "$line" "fetched_notes")
            elapsed=$(_extract_field "$line" "elapsed_ms")
            tree_ms=$(_extract_field "$line" "tree_init_ms")
            fetch_ms=$(_extract_field "$line" "fetch_ms")
            apply_ms=$(_extract_field "$line" "apply_ms")
            # Human-readable trigger reason
            case "$trigger" in
              cold_cache)          trigger_desc="Cold cache (first sync after startup or cache reset)" ;;
              large_delta)         trigger_desc="Large delta (gap >= threshold, bulk rebuild is faster)" ;;
              incremental_fallback) trigger_desc="Incremental sync failed (root mismatch or error)" ;;
              *)                   trigger_desc="$trigger" ;;
            esac
            # Human-readable memory
            mem_display="$mem B"
            if [ -n "$mem" ] && [ "$mem" -gt 0 ] 2>/dev/null; then
              mem_mb=$((mem / 1048576))
              mem_display="${mem_mb} MiB (${mem} bytes)"
            fi
            detail="Tree successfully reconstructed from scratch via RPC notes endpoint."
            detail="$detail
         trigger:     $trigger_desc
         depth=$depth  capacity=$cap  tree_memory=$mem_display
         cached_before=$cached_before  target=$target  delta=$delta  threshold=$threshold
         fetched_notes=$notes
         elapsed=${elapsed}ms  (fetch=${fetch_ms}ms  tree_init=${tree_ms}ms  apply=${apply_ms}ms)
         --- Benchmark replication: depth=$depth  leaves=$target  trigger=$trigger ---"
            color="32" ;;

          *"synced from indexer DB (snapshot rebuild)"*)
            reason="SNAPSHOT REBUILD OK"
            trigger=$(_extract_field "$line" "rebuild_trigger")
            depth=$(_extract_field "$line" "depth")
            cached_before=$(_extract_field "$line" "cached_next_before")
            mem=$(_extract_field "$line" "tree_mem_bytes")
            tree_size=$(_extract_field "$line" "tree_size")
            new_notes=$(_extract_field "$line" "new_notes")
            elapsed=$(_extract_field "$line" "elapsed_ms")
            apply_ms=$(_extract_field "$line" "apply_ms")
            snap_ms=$(_extract_field "$line" "snapshot_fetch_ms")
            ordering=$(_extract_field "$line" "ordering_mode")
            snapshot_rows=$(_extract_field "$line" "snapshot_rows")
            mem_display="$mem B"
            if [ -n "$mem" ] && [ "$mem" -gt 0 ] 2>/dev/null; then
              mem_mb=$((mem / 1048576))
              mem_display="${mem_mb} MiB (${mem} bytes)"
            fi
            detail="Tree reconstructed from full indexer DB snapshot (mixed metadata forced re-order)."
            detail="$detail
         trigger:     mixed_metadata (some rows have rollup_height, some don't)
         depth=$depth  tree_size=$tree_size  tree_memory=$mem_display
         cached_before=$cached_before  new_notes=$new_notes  snapshot_rows=$snapshot_rows
         ordering=$ordering
         elapsed=${elapsed}ms  (snapshot_fetch=${snap_ms}ms  apply=${apply_ms}ms)
         --- Benchmark replication: depth=$depth  leaves=$tree_size  trigger=$trigger ---"
            color="32" ;;

          *"behind cache"*"retrying without reset"*)
            reason="STALE SNAPSHOT"
            cached=$(_extract_field "$line" "cached_next_position")
            notes=$(_extract_field "$line" "notes_next_position")
            attempt=$(_extract_field "$line" "attempt")
            max=$(_extract_field "$line" "max_attempts")
            detail="RPC snapshot is behind the cache ($notes < $cached) — retrying (not a rebuild yet)."
            detail="$detail
         attempt=$attempt/$max"
            color="36" ;;
        esac

        if [ -n "$reason" ]; then
          printf '\033[1;%sm[TREE REBUILD] %s\033[0m\n' "$color" "$reason"
          printf '\033[%sm         %s\033[0m\n' "$color" "$detail"
          printf '\033[2;37m         raw: %s\033[0m\n' "$line"
          echo ""
        fi
      done &
  _rebuild_watcher_pid=$!
  echo "[stress] Watching $MCP_SERVER_LOG for tree rebuilds (pid $_rebuild_watcher_pid)"
else
  echo "[stress] WARNING: Server log not found at $MCP_SERVER_LOG — tree-rebuild watcher disabled."
  echo "[stress]          Set MCP_SERVER_LOG to the mcp-external log path to enable it."
fi
# --------------------------------------------------------------------------

set -- \
  --mcp-endpoint "$MCP_ENDPOINT" \
  --wallets "$WALLETS" \
  --max-txs "$TXS_PER_WALLET" \
  --send-amount "$SEND_AMOUNT" \
  --duration-secs 0 \
  --session-ids-file "$SESSION_IDS_FILE" \
  "$@"

case "$CONFIRM" in
  1|true|TRUE|yes|YES) set -- "$@" --confirm ;;
esac

if command -v mcp-external-stress >/dev/null 2>&1; then
  exec mcp-external-stress "$@"
fi

exec cargo run -p mcp-external-stress --manifest-path "$REPO_ROOT/Cargo.toml" -- "$@"
