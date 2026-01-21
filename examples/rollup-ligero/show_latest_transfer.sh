#!/usr/bin/env bash
set -euo pipefail

# Script to show the latest transfer transaction from the indexer
# Usage: ./show_latest_transfer.sh [OPTIONS]
#
# Options:
#   -u, --url URL         Indexer URL (default: http://localhost:13100)
#   -r, --raw             Show raw JSON output without formatting
#   -a, --all             Show all recent transactions (not just transfers)
#   -n, --limit N         Number of transactions to fetch (default: 10)
#   -d, --decrypt         Decrypt transaction notes using FVK service
#   -f, --fvk FVK         Provide FVK directly (hex, 64 chars)
#   --fvk-url URL         FVK service URL (default: http://localhost:8088)
#   --fvk-token TOKEN     FVK service admin token (or set MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN)
#   -h, --help            Show this help message

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Default values
INDEXER_URL="${INDEXER_URL:-http://localhost:13100}"
FVK_SERVICE_URL="${MIDNIGHT_FVK_SERVICE_URL:-http://localhost:8088}"
FVK_ADMIN_TOKEN="${MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN:-}"
RAW_OUTPUT=false
ALL_TYPES=false
DECRYPT=false
FVK_DIRECT=""
LIMIT=10

# Color codes for pretty output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

usage() {
  echo "Usage: $0 [OPTIONS]"
  echo ""
  echo "Show the latest transfer transaction from the indexer."
  echo ""
  echo "Options:"
  echo "  -u, --url URL         Indexer URL (default: http://localhost:13100)"
  echo "  -r, --raw             Show raw JSON output without formatting"
  echo "  -a, --all             Show all recent transactions (not just transfers)"
  echo "  -n, --limit N         Number of transactions to fetch (default: 10)"
  echo "  -d, --decrypt         Decrypt transaction notes using FVK service"
  echo "  -f, --fvk FVK         Provide FVK directly (hex, 64 chars)"
  echo "  --fvk-url URL         FVK service URL (default: http://localhost:8088)"
  echo "  --fvk-token TOKEN     FVK service admin token"
  echo "  -h, --help            Show this help message"
  echo ""
  echo "Environment variables:"
  echo "  INDEXER_URL                         Indexer URL (overridden by -u flag)"
  echo "  MIDNIGHT_FVK_SERVICE_URL            FVK service URL (overridden by --fvk-url)"
  echo "  MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN    FVK service admin token (overridden by --fvk-token)"
  echo ""
  echo "Examples:"
  echo "  $0                           # Show latest transfer"
  echo "  $0 -a                        # Show all recent transactions"
  echo "  $0 -r                        # Show raw JSON"
  echo "  $0 -d                        # Show with decrypted notes (requires FVK service)"
  echo "  $0 -f <fvk_hex>              # Decrypt with specific FVK"
  echo "  $0 -u http://localhost:13100 # Use custom indexer URL"
}

# Parse arguments
while [[ $# -gt 0 ]]; do
  case $1 in
    -u|--url)
      INDEXER_URL="$2"
      shift 2
      ;;
    -r|--raw)
      RAW_OUTPUT=true
      shift
      ;;
    -a|--all)
      ALL_TYPES=true
      shift
      ;;
    -n|--limit)
      LIMIT="$2"
      shift 2
      ;;
    -d|--decrypt)
      DECRYPT=true
      shift
      ;;
    -f|--fvk)
      FVK_DIRECT="$2"
      DECRYPT=true
      shift 2
      ;;
    --fvk-url)
      FVK_SERVICE_URL="$2"
      shift 2
      ;;
    --fvk-token)
      FVK_ADMIN_TOKEN="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1"
      usage
      exit 1
      ;;
  esac
done

# Check if jq is available
if ! command -v jq &> /dev/null; then
  echo "Error: jq is required but not installed."
  echo "Install it with: brew install jq (macOS) or apt install jq (Linux)"
  exit 1
fi

# Check if curl is available
if ! command -v curl &> /dev/null; then
  echo "Error: curl is required but not installed."
  exit 1
fi

# Convert hex string to bech32m format (privacy pool addresses)
hex_to_bech32m() {
  local hex="$1"
  local prefix="${2:-privpool}"
  
  # Use Python for bech32m encoding (inline implementation)
  python3 << PYEOF
import sys

CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"
BECH32M_CONST = 0x2bc830a3

def bech32_polymod(values):
    GEN = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3]
    chk = 1
    for v in values:
        b = chk >> 25
        chk = ((chk & 0x1ffffff) << 5) ^ v
        for i in range(5):
            chk ^= GEN[i] if ((b >> i) & 1) else 0
    return chk

def bech32_hrp_expand(hrp):
    return [ord(x) >> 5 for x in hrp] + [0] + [ord(x) & 31 for x in hrp]

def bech32m_create_checksum(hrp, data):
    values = bech32_hrp_expand(hrp) + data
    polymod = bech32_polymod(values + [0, 0, 0, 0, 0, 0]) ^ BECH32M_CONST
    return [(polymod >> 5 * (5 - i)) & 31 for i in range(6)]

def convertbits(data, frombits, tobits, pad=True):
    acc = 0
    bits = 0
    ret = []
    maxv = (1 << tobits) - 1
    for value in data:
        acc = (acc << frombits) | value
        bits += frombits
        while bits >= tobits:
            bits -= tobits
            ret.append((acc >> bits) & maxv)
    if pad and bits:
        ret.append((acc << (tobits - bits)) & maxv)
    return ret

def encode_bech32m(hrp, data_bytes):
    data5 = convertbits(data_bytes, 8, 5)
    checksum = bech32m_create_checksum(hrp, data5)
    return hrp + "1" + "".join([CHARSET[d] for d in data5 + checksum])

hex_str = "${hex}"
prefix = "${prefix}"

try:
    data_bytes = bytes.fromhex(hex_str)
    result = encode_bech32m(prefix, data_bytes)
    print(result)
except Exception as e:
    print(hex_str)  # Return original on error
PYEOF
}

# Check if a field should be displayed as bech32m address
is_address_field() {
  local key="$1"
  case "$key" in
    recipient|sender_id|sender|privacy_sender|privacy_recipient)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

# Fetch transactions from indexer
fetch_transactions() {
  local response
  response=$(curl -s -w "\n%{http_code}" "${INDEXER_URL}/transactions?limit=${LIMIT}" 2>/dev/null) || {
    echo -e "${RED}Error: Failed to connect to indexer at ${INDEXER_URL}${NC}"
    echo "Make sure the indexer is running (run_indexer.sh or run_all.sh)"
    exit 1
  }

  local http_code
  http_code=$(echo "$response" | tail -n1)
  local body
  body=$(echo "$response" | sed '$d')

  if [[ "$http_code" != "200" ]]; then
    echo -e "${RED}Error: Indexer returned HTTP $http_code${NC}"
    echo "$body" | jq . 2>/dev/null || echo "$body"
    exit 1
  fi

  echo "$body"
}

# Fetch FVK from service by commitment
fetch_fvk_by_commitment() {
  local fvk_commitment="$1"
  
  if [[ -z "$FVK_ADMIN_TOKEN" ]]; then
    echo -e "${YELLOW}Warning: No FVK service admin token provided.${NC}" >&2
    echo -e "Set MIDNIGHT_FVK_SERVICE_ADMIN_TOKEN or use --fvk-token" >&2
    return 1
  fi

  local response
  response=$(curl -s -w "\n%{http_code}" \
    -H "Authorization: Bearer ${FVK_ADMIN_TOKEN}" \
    "${FVK_SERVICE_URL}/v1/fvk/${fvk_commitment}" 2>/dev/null) || {
    echo -e "${RED}Error: Failed to connect to FVK service at ${FVK_SERVICE_URL}${NC}" >&2
    return 1
  }

  local http_code
  http_code=$(echo "$response" | tail -n1)
  local body
  body=$(echo "$response" | sed '$d')

  if [[ "$http_code" == "404" ]]; then
    echo -e "${YELLOW}FVK not found for commitment: ${fvk_commitment:0:16}...${NC}" >&2
    return 1
  elif [[ "$http_code" != "200" ]]; then
    echo -e "${RED}Error: FVK service returned HTTP $http_code${NC}" >&2
    echo "$body" | jq -r '.error // .' 2>/dev/null || echo "$body" >&2
    return 1
  fi

  echo "$body" | jq -r '.fvk // empty'
}

# Extract fvk_commitment from encrypted notes
extract_fvk_commitment() {
  local tx="$1"
  
  # Try to get fvk_commitment from encrypted_notes
  local fvk_commitment
  fvk_commitment=$(echo "$tx" | jq -r '.encrypted_notes[0].fvk_commitment // empty' 2>/dev/null)
  
  if [[ -z "$fvk_commitment" || "$fvk_commitment" == "null" ]]; then
    # Try view_attestations
    fvk_commitment=$(echo "$tx" | jq -r '.view_attestations[0].fvk_commitment // empty' 2>/dev/null)
    if [[ -n "$fvk_commitment" && "$fvk_commitment" != "null" ]]; then
      # view_attestations might have array format, convert to hex
      if echo "$fvk_commitment" | jq -e 'type == "array"' >/dev/null 2>&1; then
        fvk_commitment=$(echo "$fvk_commitment" | jq -r 'map(. | tostring | if length == 1 then "0" + . else . end) | join("")' | xxd -r -p | xxd -p | tr -d '\n')
      fi
    fi
  fi
  
  echo "$fvk_commitment"
}

# Fetch wallet transactions with decryption
fetch_wallet_txs_with_decrypt() {
  local address="$1"
  local fvk="$2"
  
  local response
  response=$(curl -s -w "\n%{http_code}" \
    -X POST \
    -H "Content-Type: application/json" \
    -d "{\"vfk\": \"${fvk}\"}" \
    "${INDEXER_URL}/wallets/${address}?type=transfer&limit=1" 2>/dev/null) || {
    echo -e "${RED}Error: Failed to connect to indexer${NC}" >&2
    return 1
  }

  local http_code
  http_code=$(echo "$response" | tail -n1)
  local body
  body=$(echo "$response" | sed '$d')

  if [[ "$http_code" != "200" ]]; then
    echo -e "${RED}Error: Indexer returned HTTP $http_code${NC}" >&2
    return 1
  fi

  echo "$body"
}

# Get decrypted transaction details
get_decrypted_tx() {
  local tx="$1"
  local fvk="$2"
  local tx_hash
  tx_hash=$(echo "$tx" | jq -r '.tx_hash // empty')
  
  # If we have a direct FVK, use wallet endpoint with it
  if [[ -n "$fvk" ]]; then
    local sender
    sender=$(echo "$tx" | jq -r '.sender // empty')
    local privacy_sender
    privacy_sender=$(echo "$tx" | jq -r '.privacy_sender // empty')
    local recipient
    recipient=$(echo "$tx" | jq -r '.recipient // .privacy_recipient // empty')
    
    # Try addresses in order: sender first (most likely to work), then privacy_sender, then recipient
    local addresses=()
    if [[ -n "$sender" && "$sender" != "null" ]]; then
      addresses+=("$sender")
    fi
    if [[ -n "$privacy_sender" && "$privacy_sender" != "null" && "$privacy_sender" != "$sender" ]]; then
      addresses+=("$privacy_sender")
    fi
    if [[ -n "$recipient" && "$recipient" != "null" ]]; then
      addresses+=("$recipient")
    fi
    
    for address in "${addresses[@]}"; do
      local result
      result=$(fetch_wallet_txs_with_decrypt "$address" "$fvk" 2>/dev/null) || continue
      if [[ -n "$result" ]]; then
        # Find the matching transaction by tx_hash
        local decrypted_item
        decrypted_item=$(echo "$result" | jq -c --arg hash "$tx_hash" '.items[] | select(.tx_hash == $hash) // empty' 2>/dev/null | head -1)
        if [[ -n "$decrypted_item" && "$decrypted_item" != "null" ]]; then
          echo "$decrypted_item"
          return 0
        fi
        # Fall back to first item if no exact match
        decrypted_item=$(echo "$result" | jq -c '.items[0] // empty' 2>/dev/null)
        if [[ -n "$decrypted_item" && "$decrypted_item" != "null" ]]; then
          local has_decrypted
          has_decrypted=$(echo "$decrypted_item" | jq -r '.decrypted_notes // empty')
          if [[ -n "$has_decrypted" && "$has_decrypted" != "null" && "$has_decrypted" != "[]" ]]; then
            echo "$decrypted_item"
            return 0
          fi
        fi
      fi
    done
  fi
  
  # Return original if no decryption available
  echo "$tx"
}

# Format timestamp
format_timestamp() {
  local ts_ms="$1"
  if command -v gdate &> /dev/null; then
    gdate -d "@$((ts_ms / 1000))" "+%Y-%m-%d %H:%M:%S UTC" 2>/dev/null || echo "$ts_ms"
  elif date --version 2>/dev/null | grep -q GNU; then
    date -d "@$((ts_ms / 1000))" "+%Y-%m-%d %H:%M:%S UTC" 2>/dev/null || echo "$ts_ms"
  else
    # macOS date
    date -r "$((ts_ms / 1000))" "+%Y-%m-%d %H:%M:%S UTC" 2>/dev/null || echo "$ts_ms"
  fi
}

# Truncate long strings
truncate_string() {
  local str="$1"
  local max_len="${2:-64}"
  if [[ ${#str} -gt $max_len ]]; then
    echo "${str:0:$((max_len-3))}..."
  else
    echo "$str"
  fi
}

# Print transaction details
print_transaction() {
  local tx="$1"
  local index="$2"
  local show_decrypt="${3:-false}"
  
  local tx_hash kind timestamp_ms sender recipient amount status
  tx_hash=$(echo "$tx" | jq -r '.tx_hash // "N/A"')
  kind=$(echo "$tx" | jq -r '.kind // "unknown"')
  timestamp_ms=$(echo "$tx" | jq -r '.timestamp_ms // 0')
  sender=$(echo "$tx" | jq -r '.sender // .privacy_sender // "N/A"')
  recipient=$(echo "$tx" | jq -r '.recipient // .privacy_recipient // "N/A"')
  amount=$(echo "$tx" | jq -r '.amount // "N/A"')
  status=$(echo "$tx" | jq -r '.status // "unknown"')
  anchor_root=$(echo "$tx" | jq -r '.anchor_root // "N/A"')
  nullifier=$(echo "$tx" | jq -r '.nullifier // "N/A"')

  local formatted_time
  formatted_time=$(format_timestamp "$timestamp_ms")

  # Status color
  local status_color
  case "$status" in
    "successful"|"success") status_color="${GREEN}" ;;
    "failed"|"error") status_color="${RED}" ;;
    *) status_color="${YELLOW}" ;;
  esac

  # Kind color
  local kind_color
  case "$kind" in
    "transfer") kind_color="${CYAN}" ;;
    "deposit") kind_color="${GREEN}" ;;
    "withdraw") kind_color="${YELLOW}" ;;
    *) kind_color="${NC}" ;;
  esac

  echo ""
  echo -e "${BOLD}═══════════════════════════════════════════════════════════════════${NC}"
  echo -e "${BOLD}Transaction #${index}${NC}"
  echo -e "${BOLD}═══════════════════════════════════════════════════════════════════${NC}"
  echo ""
  echo -e "  ${BOLD}Type:${NC}        ${kind_color}${kind}${NC}"
  echo -e "  ${BOLD}Status:${NC}      ${status_color}${status}${NC}"
  echo -e "  ${BOLD}Timestamp:${NC}   ${formatted_time}"
  echo -e "  ${BOLD}TX Hash:${NC}     ${tx_hash}"
  echo ""
  
  if [[ "$sender" != "N/A" && "$sender" != "null" ]]; then
    echo -e "  ${BOLD}Sender:${NC}      $(truncate_string "$sender" 70)"
  fi
  
  if [[ "$recipient" != "N/A" && "$recipient" != "null" ]]; then
    echo -e "  ${BOLD}Recipient:${NC}   $(truncate_string "$recipient" 70)"
  fi
  
  if [[ "$amount" != "N/A" && "$amount" != "null" ]]; then
    echo -e "  ${BOLD}Amount:${NC}      ${amount}"
  fi

  if [[ "$kind" == "transfer" || "$kind" == "withdraw" ]]; then
    echo ""
    echo -e "  ${BOLD}Privacy Details:${NC}"
    if [[ "$anchor_root" != "N/A" && "$anchor_root" != "null" ]]; then
      echo -e "    Anchor Root: $(truncate_string "$anchor_root" 60)"
    fi
    if [[ "$nullifier" != "N/A" && "$nullifier" != "null" ]]; then
      echo -e "    Nullifier:   $(truncate_string "$nullifier" 60)"
    fi
  fi

  # Show events if present
  local events_count
  events_count=$(echo "$tx" | jq -r '.events // [] | length')
  if [[ "$events_count" -gt 0 ]]; then
    echo ""
    echo -e "  ${BOLD}Events (${events_count}):${NC}"
    echo "$tx" | jq -r '.events[]? | "    - \(.key // "unknown")"' 2>/dev/null || true
  fi

  # Show encrypted notes info
  local encrypted_count
  encrypted_count=$(echo "$tx" | jq -r '.encrypted_notes // [] | length')
  if [[ "$encrypted_count" -gt 0 ]]; then
    echo ""
    echo -e "  ${BOLD}Encrypted Notes (${encrypted_count}):${NC}"
    local i=0
    while [[ $i -lt $encrypted_count ]]; do
      local cm fvk_commitment
      cm=$(echo "$tx" | jq -r ".encrypted_notes[$i].cm // \"N/A\"")
      fvk_commitment=$(echo "$tx" | jq -r ".encrypted_notes[$i].fvk_commitment // \"N/A\"")
      echo -e "    Note $((i+1)):"
      echo -e "      Commitment: $(truncate_string "$cm" 50)"
      echo -e "      FVK Commit: $(truncate_string "$fvk_commitment" 50)"
      i=$((i+1))
    done
  fi

  # Show decrypted notes if available
  local decrypted_notes
  decrypted_notes=$(echo "$tx" | jq -r '.decrypted_notes // empty')
  if [[ -n "$decrypted_notes" && "$decrypted_notes" != "null" && "$decrypted_notes" != "[]" ]]; then
    local decrypted_count
    decrypted_count=$(echo "$decrypted_notes" | jq 'length')
    echo ""
    echo -e "  ${BOLD}${GREEN}Decrypted Notes (${decrypted_count}):${NC}"
    local i=0
    while [[ $i -lt $decrypted_count ]]; do
      echo -e "    ${GREEN}Note $((i+1)):${NC}"
      # Iterate over all keys in the decrypted note and display them
      local note_json
      note_json=$(echo "$decrypted_notes" | jq -c ".[$i]")
      echo "$note_json" | jq -r 'to_entries | sort_by(.key) | .[] | "\(.key)|\(.value)"' 2>/dev/null | while IFS='|' read -r key val; do
        if [[ -n "$val" && "$val" != "null" ]]; then
          # Capitalize first letter of each word and replace underscores with spaces
          local display_key display_val
          display_key=$(echo "$key" | sed 's/_/ /g' | awk '{for(j=1;j<=NF;j++) $j=toupper(substr($j,1,1)) substr($j,2)}1')
          # Pad the key for alignment (max 20 chars)
          printf -v padded_key "%-20s" "$display_key:"
          
          # Check if this is an address field that should be converted to bech32m
          if is_address_field "$key"; then
            # Convert hex to bech32m and show full value
            display_val=$(hex_to_bech32m "$val")
            echo -e "      ${GREEN}${padded_key} ${display_val}${NC}"
          else
            # For non-address fields, show value (truncate only very long non-essential fields)
            case "$key" in
              domain|rho|cm|ct|nonce|mac)
                # These are cryptographic values - show truncated
                echo -e "      ${GREEN}${padded_key} $(truncate_string "$val" 64)${NC}"
                ;;
              *)
                # Show full value for other fields
                echo -e "      ${GREEN}${padded_key} ${val}${NC}"
                ;;
            esac
          fi
        fi
      done
      i=$((i+1))
    done
  elif [[ "$show_decrypt" == "true" && "$encrypted_count" -gt 0 ]]; then
    echo ""
    echo -e "  ${YELLOW}Note: Encrypted notes present but could not be decrypted.${NC}"
    echo -e "  ${YELLOW}Make sure you have the correct FVK for this transaction.${NC}"
  fi

  echo ""
}

# Try to get FVK for decryption
resolve_fvk() {
  local tx="$1"
  
  # If FVK provided directly, use it
  if [[ -n "$FVK_DIRECT" ]]; then
    echo "$FVK_DIRECT"
    return 0
  fi
  
  # Try to fetch from FVK service
  local fvk_commitment
  fvk_commitment=$(extract_fvk_commitment "$tx")
  
  if [[ -n "$fvk_commitment" && "$fvk_commitment" != "null" ]]; then
    local fvk
    fvk=$(fetch_fvk_by_commitment "$fvk_commitment" 2>/dev/null) || true
    if [[ -n "$fvk" ]]; then
      echo "$fvk"
      return 0
    fi
  fi
  
  return 1
}

# Main execution
main() {
  echo -e "${BOLD}Fetching transactions from ${INDEXER_URL}...${NC}"
  
  if $DECRYPT; then
    if [[ -n "$FVK_DIRECT" ]]; then
      echo -e "Using provided FVK for decryption"
    else
      echo -e "Decryption enabled (FVK service: ${FVK_SERVICE_URL})"
    fi
  fi
  echo ""

  local transactions
  transactions=$(fetch_transactions)

  if $RAW_OUTPUT; then
    echo "$transactions" | jq .
    exit 0
  fi

  # Extract items array
  local items
  items=$(echo "$transactions" | jq '.items // []')
  
  local total_count
  total_count=$(echo "$items" | jq 'length')

  if [[ "$total_count" -eq 0 ]]; then
    echo -e "${YELLOW}No transactions found.${NC}"
    exit 0
  fi

  if $ALL_TYPES; then
    echo -e "${BOLD}Showing all recent transactions (${total_count} found):${NC}"
    local index=1
    echo "$items" | jq -c '.[]' | while read -r tx; do
      local final_tx="$tx"
      if $DECRYPT; then
        local fvk
        fvk=$(resolve_fvk "$tx" 2>/dev/null) || true
        if [[ -n "$fvk" ]]; then
          final_tx=$(get_decrypted_tx "$tx" "$fvk" 2>/dev/null) || final_tx="$tx"
        fi
      fi
      print_transaction "$final_tx" "$index" "$DECRYPT"
      index=$((index + 1))
    done
  else
    # Find the latest transfer
    local transfer
    transfer=$(echo "$items" | jq -c '[.[] | select(.kind == "transfer")] | first // empty')

    if [[ -z "$transfer" || "$transfer" == "null" ]]; then
      echo -e "${YELLOW}No transfer transactions found.${NC}"
      echo ""
      echo "Available transaction types in the last ${LIMIT} transactions:"
      echo "$items" | jq -r '.[].kind' | sort | uniq -c | while read count kind; do
        echo "  - $kind: $count"
      done
      echo ""
      echo "Use -a flag to show all transaction types."
      exit 0
    fi

    # Try to decrypt if requested
    local final_transfer="$transfer"
    if $DECRYPT; then
      local fvk
      fvk=$(resolve_fvk "$transfer" 2>/dev/null) || true
      if [[ -n "$fvk" ]]; then
        echo -e "${GREEN}Found FVK, attempting decryption...${NC}"
        final_transfer=$(get_decrypted_tx "$transfer" "$fvk" 2>/dev/null) || final_transfer="$transfer"
      else
        echo -e "${YELLOW}Could not obtain FVK for decryption.${NC}"
      fi
    fi

    echo -e "${BOLD}Latest Transfer Transaction:${NC}"
    print_transaction "$final_transfer" "1" "$DECRYPT"
  fi

  echo -e "${BOLD}═══════════════════════════════════════════════════════════════════${NC}"
  echo ""
}

main
