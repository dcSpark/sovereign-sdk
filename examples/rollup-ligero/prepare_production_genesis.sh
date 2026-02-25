#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

SOURCE_GENESIS_DIR="$WORKSPACE_ROOT/examples/test-data/genesis/demo/mock"
OUTPUT_GENESIS_DIR="$WORKSPACE_ROOT/examples/test-data/genesis/production/mock"

WALLETS_JSON=""
ADMIN_ADDRESS=""
SEQUENCER_ADDRESS=""
PAYMASTER_ADDRESS=""
PROVER_ADDRESS=""
ATTESTER_ADDRESS=""
REWARD_ADDRESS=""

ADMIN_BALANCE=""
SEQUENCER_BALANCE=""
PAYMASTER_BALANCE=""
PROVER_BALANCE=""
ATTESTER_BALANCE=""
REWARD_BALANCE=""

DRY_RUN=0
FORCE=0

usage() {
  cat <<'USAGE'
Create a production genesis directory from demo/mock with dedicated operator addresses
and minimal funded wallets (no 5k deterministic test wallets).

Usage:
  ./prepare_production_genesis.sh [options]

Options:
  --source-genesis-dir <path>   Source genesis directory (default: examples/test-data/genesis/demo/mock)
  --output-genesis-dir <path>   Output genesis directory (default: examples/test-data/genesis/production/mock)

  --wallets-json <path>         JSON from ./generate_operator_wallets.sh (address fields are read)

  --admin-address <sov1...>
  --sequencer-address <sov1...>
  --paymaster-address <sov1...>
  --prover-address <sov1...>
  --attester-address <sov1...>
  --reward-address <sov1...>    Optional (default: prover address)

  --admin-balance <u128>
  --sequencer-balance <u128>
  --paymaster-balance <u128>
  --prover-balance <u128>
  --attester-balance <u128>
  --reward-balance <u128>

  --dry-run                     Validate and print summary without writing output dir
  --force                       Overwrite output directory if it exists
  -h, --help                    Show this help

Expected result:
  - Updates module admin/operator addresses in genesis JSON files
  - Rewrites bank admins and funded balances to a minimal role-based set
  - Removes generated_keypairs.json from output genesis
  - Keeps sequencer bond safety check (sequencer balance >= seq_bond)
USAGE
}

fail() {
  echo "Error: $*" >&2
  exit 1
}

is_valid_sov_address() {
  local addr="$1"
  [[ "$addr" =~ ^sov1[023456789acdefghjklmnpqrstuvwxyz]+$ ]]
}

is_nonnegative_integer() {
  [[ "$1" =~ ^[0-9]+$ ]]
}

json_field_or_empty() {
  local file="$1"
  local field="$2"
  jq -r --arg field "$field" '.[$field] // empty' "$file"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source-genesis-dir)
      SOURCE_GENESIS_DIR="${2:-}"
      shift 2
      ;;
    --output-genesis-dir)
      OUTPUT_GENESIS_DIR="${2:-}"
      shift 2
      ;;
    --wallets-json)
      WALLETS_JSON="${2:-}"
      shift 2
      ;;
    --admin-address)
      ADMIN_ADDRESS="${2:-}"
      shift 2
      ;;
    --sequencer-address)
      SEQUENCER_ADDRESS="${2:-}"
      shift 2
      ;;
    --paymaster-address)
      PAYMASTER_ADDRESS="${2:-}"
      shift 2
      ;;
    --prover-address)
      PROVER_ADDRESS="${2:-}"
      shift 2
      ;;
    --attester-address)
      ATTESTER_ADDRESS="${2:-}"
      shift 2
      ;;
    --reward-address)
      REWARD_ADDRESS="${2:-}"
      shift 2
      ;;
    --admin-balance)
      ADMIN_BALANCE="${2:-}"
      shift 2
      ;;
    --sequencer-balance)
      SEQUENCER_BALANCE="${2:-}"
      shift 2
      ;;
    --paymaster-balance)
      PAYMASTER_BALANCE="${2:-}"
      shift 2
      ;;
    --prover-balance)
      PROVER_BALANCE="${2:-}"
      shift 2
      ;;
    --attester-balance)
      ATTESTER_BALANCE="${2:-}"
      shift 2
      ;;
    --reward-balance)
      REWARD_BALANCE="${2:-}"
      shift 2
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --force)
      FORCE=1
      shift
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

[[ -d "$SOURCE_GENESIS_DIR" ]] || fail "Source genesis directory not found: $SOURCE_GENESIS_DIR"

if [[ -n "$WALLETS_JSON" ]]; then
  [[ -f "$WALLETS_JSON" ]] || fail "Wallets JSON not found: $WALLETS_JSON"
  jq -e 'type == "object"' "$WALLETS_JSON" >/dev/null || fail "--wallets-json must be a top-level JSON object"

  [[ -n "$ADMIN_ADDRESS" ]] || ADMIN_ADDRESS="$(json_field_or_empty "$WALLETS_JSON" "ADMIN_ADDRESS")"
  [[ -n "$SEQUENCER_ADDRESS" ]] || SEQUENCER_ADDRESS="$(json_field_or_empty "$WALLETS_JSON" "SEQUENCER_ADDRESS")"
  [[ -n "$PAYMASTER_ADDRESS" ]] || PAYMASTER_ADDRESS="$(json_field_or_empty "$WALLETS_JSON" "PAYMASTER_ADDRESS")"
  [[ -n "$PROVER_ADDRESS" ]] || PROVER_ADDRESS="$(json_field_or_empty "$WALLETS_JSON" "PROVER_ADDRESS")"
  [[ -n "$ATTESTER_ADDRESS" ]] || ATTESTER_ADDRESS="$(json_field_or_empty "$WALLETS_JSON" "ATTESTER_ADDRESS")"
  [[ -n "$REWARD_ADDRESS" ]] || REWARD_ADDRESS="$(json_field_or_empty "$WALLETS_JSON" "REWARD_ADDRESS")"
fi

[[ -n "$ADMIN_ADDRESS" ]] || fail "Missing admin address (use --admin-address or --wallets-json)"
[[ -n "$SEQUENCER_ADDRESS" ]] || fail "Missing sequencer address (use --sequencer-address or --wallets-json)"
[[ -n "$PAYMASTER_ADDRESS" ]] || fail "Missing paymaster address (use --paymaster-address or --wallets-json)"
[[ -n "$PROVER_ADDRESS" ]] || fail "Missing prover address (use --prover-address or --wallets-json)"
[[ -n "$ATTESTER_ADDRESS" ]] || fail "Missing attester address (use --attester-address or --wallets-json)"

if [[ -z "$REWARD_ADDRESS" ]]; then
  REWARD_ADDRESS="$PROVER_ADDRESS"
fi

for addr in "$ADMIN_ADDRESS" "$SEQUENCER_ADDRESS" "$PAYMASTER_ADDRESS" "$PROVER_ADDRESS" "$ATTESTER_ADDRESS" "$REWARD_ADDRESS"; do
  is_valid_sov_address "$addr" || fail "Invalid address format: $addr"
done

for bal in "$ADMIN_BALANCE" "$SEQUENCER_BALANCE" "$PAYMASTER_BALANCE" "$PROVER_BALANCE" "$ATTESTER_BALANCE" "$REWARD_BALANCE"; do
  if [[ -n "$bal" ]]; then
    is_nonnegative_integer "$bal" || fail "Invalid balance (must be integer): $bal"
  fi
done

effective_output="$OUTPUT_GENESIS_DIR"
cleanup_dir=""

if [[ "$DRY_RUN" -eq 1 ]]; then
  cleanup_dir="$(mktemp -d "${TMPDIR:-/tmp}/production_genesis_dry_run.XXXXXX")"
  effective_output="$cleanup_dir/genesis"
else
  if [[ -e "$effective_output" ]]; then
    if [[ "$FORCE" -eq 1 ]]; then
      rm -rf "$effective_output"
    else
      fail "Output path already exists: $effective_output (use --force to overwrite)"
    fi
  fi
fi

mkdir -p "$(dirname "$effective_output")"
cp -R "$SOURCE_GENESIS_DIR" "$effective_output"

summary_json="$(
python3 - <<'PY' \
  "$effective_output" \
  "$ADMIN_ADDRESS" "$SEQUENCER_ADDRESS" "$PAYMASTER_ADDRESS" "$PROVER_ADDRESS" "$ATTESTER_ADDRESS" "$REWARD_ADDRESS" \
  "$ADMIN_BALANCE" "$SEQUENCER_BALANCE" "$PAYMASTER_BALANCE" "$PROVER_BALANCE" "$ATTESTER_BALANCE" "$REWARD_BALANCE"
import json
import sys
from pathlib import Path

(
    output_dir,
    admin_addr,
    seq_addr,
    paymaster_addr,
    prover_addr,
    attester_addr,
    reward_addr,
    admin_balance_opt,
    seq_balance_opt,
    paymaster_balance_opt,
    prover_balance_opt,
    attester_balance_opt,
    reward_balance_opt,
) = sys.argv[1:]

root = Path(output_dir)

files = {
    "access_pattern": root / "access_pattern.json",
    "midnight_privacy": root / "midnight_privacy.json",
    "value_setter": root / "value_setter.json",
    "value_setter_zk": root / "value_setter_zk.json",
    "sequencer_registry": root / "sequencer_registry.json",
    "paymaster": root / "paymaster.json",
    "operator_incentives": root / "operator_incentives.json",
    "prover_incentives": root / "prover_incentives.json",
    "attester_incentives": root / "attester_incentives.json",
    "bank": root / "bank.json",
}

for name, path in files.items():
    if not path.is_file():
        raise SystemExit(f"missing required genesis file: {path}")

def load(path: Path):
    with path.open("r", encoding="utf-8") as fp:
        return json.load(fp)

def dump(path: Path, data):
    with path.open("w", encoding="utf-8") as fp:
        json.dump(data, fp, indent=2)
        fp.write("\n")

access_pattern = load(files["access_pattern"])
midnight_privacy = load(files["midnight_privacy"])
value_setter = load(files["value_setter"])
value_setter_zk = load(files["value_setter_zk"])
sequencer_registry = load(files["sequencer_registry"])
paymaster = load(files["paymaster"])
operator_incentives = load(files["operator_incentives"])
prover_incentives = load(files["prover_incentives"])
attester_incentives = load(files["attester_incentives"])
bank = load(files["bank"])

source_balances = bank.get("gas_token_config", {}).get("address_and_balances", [])
if not isinstance(source_balances, list):
    raise SystemExit("bank.json gas_token_config.address_and_balances must be a list")

source_balance_map = {}
for entry in source_balances:
    if not isinstance(entry, list) or len(entry) != 2:
        continue
    addr, amount = entry
    if isinstance(addr, str) and isinstance(amount, str) and amount.isdigit():
        source_balance_map[addr] = amount

# Demo defaults in this repo.
old_admin = "sov1lzkjgdaz08su3yevqu6ceywufl35se9f33kztu5cu2spja5hyyf"
old_paymaster = "sov1x3jtvq0zwhj2ucsc4hqugskvralrulxvf53vwtkred93s85ar2a"

operator_default = source_balance_map.get(old_admin, "5000000000000")
paymaster_default = source_balance_map.get(old_paymaster, "5000000000000000")

def choose_balance(opt: str, fallback: str) -> str:
    if opt and opt.isdigit():
        return opt
    return fallback

admin_balance = choose_balance(admin_balance_opt, operator_default)
seq_balance = choose_balance(seq_balance_opt, operator_default)
paymaster_balance = choose_balance(paymaster_balance_opt, paymaster_default)
prover_balance = choose_balance(prover_balance_opt, operator_default)
attester_balance = choose_balance(attester_balance_opt, operator_default)
reward_balance = choose_balance(reward_balance_opt, operator_default)

access_pattern["admin"] = admin_addr
midnight_privacy["admin"] = admin_addr
value_setter["admin"] = admin_addr
value_setter_zk["admin"] = admin_addr

seq_cfg = sequencer_registry.get("sequencer_config")
if not isinstance(seq_cfg, dict):
    raise SystemExit("sequencer_registry.json missing object: sequencer_config")
seq_cfg["seq_rollup_address"] = seq_addr

payers = paymaster.get("payers")
if not isinstance(payers, list) or not payers:
    raise SystemExit("paymaster.json missing non-empty array: payers")
for payer in payers:
    if isinstance(payer, dict):
        payer["payer_address"] = paymaster_addr

operator_incentives["reward_address"] = reward_addr

initial_provers = prover_incentives.get("initial_provers")
prover_stake = "200000"
if isinstance(initial_provers, list) and initial_provers and isinstance(initial_provers[0], list) and len(initial_provers[0]) >= 2:
    existing = initial_provers[0][1]
    if isinstance(existing, str) and existing.isdigit():
        prover_stake = existing
prover_incentives["initial_provers"] = [[prover_addr, prover_stake]]

initial_attesters = attester_incentives.get("initial_attesters")
attester_stake = "200000"
if isinstance(initial_attesters, list) and initial_attesters and isinstance(initial_attesters[0], list) and len(initial_attesters[0]) >= 2:
    existing = initial_attesters[0][1]
    if isinstance(existing, str) and existing.isdigit():
        attester_stake = existing
attester_incentives["initial_attesters"] = [[attester_addr, attester_stake]]

gtc = bank.get("gas_token_config")
if not isinstance(gtc, dict):
    raise SystemExit("bank.json missing object: gas_token_config")
gtc["admins"] = [admin_addr]

ordered = [
    (seq_addr, seq_balance),
    (paymaster_addr, paymaster_balance),
    (admin_addr, admin_balance),
    (prover_addr, prover_balance),
    (attester_addr, attester_balance),
    (reward_addr, reward_balance),
]

merged = {}
order = []
for addr, amount in ordered:
    amount_int = int(amount)
    if addr not in merged:
        merged[addr] = amount_int
        order.append(addr)
    else:
        if amount_int > merged[addr]:
            merged[addr] = amount_int

new_balances = [[addr, str(merged[addr])] for addr in order]
gtc["address_and_balances"] = new_balances

# Validation: sequencer must cover bond.
seq_bond_raw = seq_cfg.get("seq_bond", "0")
try:
    seq_bond = int(seq_bond_raw)
except Exception:
    raise SystemExit(f"invalid seq_bond in sequencer_registry.json: {seq_bond_raw}")

seq_balance_final = merged.get(seq_addr, 0)
if seq_balance_final < seq_bond:
    raise SystemExit(
        f"sequencer balance ({seq_balance_final}) is below seq_bond ({seq_bond}); increase --sequencer-balance"
    )

required_addrs = [admin_addr, seq_addr, paymaster_addr, prover_addr, attester_addr, reward_addr]
missing = [addr for addr in required_addrs if addr not in merged]
if missing:
    raise SystemExit(f"required addresses missing from bank balances: {missing}")

# Write files.
dump(files["access_pattern"], access_pattern)
dump(files["midnight_privacy"], midnight_privacy)
dump(files["value_setter"], value_setter)
dump(files["value_setter_zk"], value_setter_zk)
dump(files["sequencer_registry"], sequencer_registry)
dump(files["paymaster"], paymaster)
dump(files["operator_incentives"], operator_incentives)
dump(files["prover_incentives"], prover_incentives)
dump(files["attester_incentives"], attester_incentives)
dump(files["bank"], bank)

removed_generated_keypairs = False
generated_keypairs = root / "generated_keypairs.json"
if generated_keypairs.exists():
    generated_keypairs.unlink()
    removed_generated_keypairs = True

summary = {
    "admin_address": admin_addr,
    "sequencer_address": seq_addr,
    "paymaster_address": paymaster_addr,
    "prover_address": prover_addr,
    "attester_address": attester_addr,
    "reward_address": reward_addr,
    "funded_wallet_count": len(new_balances),
    "sequencer_balance": str(seq_balance_final),
    "sequencer_bond": str(seq_bond),
    "removed_generated_keypairs": removed_generated_keypairs,
}

print(json.dumps(summary))
PY
)"

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "Dry-run complete. No output directory was written."
  echo "Summary:"
  printf '%s\n' "$summary_json" | jq .
  rm -rf "$cleanup_dir"
else
  echo "Production genesis written to: $effective_output"
  echo "Summary:"
  printf '%s\n' "$summary_json" | jq .
  echo
  echo "Next: run with --genesis-config-dir $effective_output"
fi
