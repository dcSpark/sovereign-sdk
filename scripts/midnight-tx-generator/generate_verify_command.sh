#!/bin/bash
# Generate a webgpu_verifier command that can be run on another machine
# This script reads the local JSON files and outputs a portable command
#
# Usage:
#   ./generate_verify_command.sh              # Generate with relative paths (portable)
#   ./generate_verify_command.sh --local      # Generate with absolute paths (for local testing)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

# Check if --local flag is passed
USE_ABSOLUTE_PATHS=false
if [ "$1" = "--local" ]; then
    USE_ABSOLUTE_PATHS=true
fi

echo -e "${BLUE}=== Generating webgpu_verifier command ===${NC}\n"

# Check required files exist
if [ ! -f "midnight_note_details.json" ]; then
    echo -e "${RED}Error: midnight_note_details.json not found${NC}"
    echo "Run deposit_and_withdraw.sh first to generate the required files"
    exit 1
fi

if [ ! -f "deposit_response.json" ]; then
    echo -e "${RED}Error: deposit_response.json not found${NC}"
    exit 1
fi

if [ ! -f "withdraw_response.json" ]; then
    echo -e "${RED}Error: withdraw_response.json not found${NC}"
    exit 1
fi

if [ ! -f "proof_data.gz" ]; then
    echo -e "${RED}Error: proof_data.gz not found${NC}"
    exit 1
fi

# Determine REPO_ROOT
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Generate the config JSON
if [ "$USE_ABSOLUTE_PATHS" = true ]; then
    # For macOS/Linux - adjust as needed
    if [[ "$OSTYPE" == "darwin"* ]]; then
        PROGRAM_PATH="$REPO_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm"
        SHADER_PATH="$REPO_ROOT/crates/adapters/ligero/bins/macos/shader"
    else
        PROGRAM_PATH="$REPO_ROOT/crates/adapters/ligero/guest/bins/programs/note_spend_guest.wasm"
        SHADER_PATH="$REPO_ROOT/crates/adapters/ligero/bins/linux-amd64/shader"
    fi
    CONFIG=$(python3 - <<PY
import json, pathlib
root = pathlib.Path('.')

note = json.loads(root.joinpath('midnight_note_details.json').read_text())
deposit = json.loads(root.joinpath('deposit_response.json').read_text())
withdraw = json.loads(root.joinpath('withdraw_response.json').read_text())

def arr_to_hex(arr):
    return ''.join(f'{b:02x}' for b in arr)

pool_deposit = next(e for e in deposit['sequencer_response']['events']
                    if e['key'] == 'ValueMidnightPrivacy/PoolDeposit')['value']['pool_deposit']
note_created = next(e for e in withdraw['sequencer_response']['events']
                    if e['key'] == 'ValueMidnightPrivacy/NoteCreated')['value']['note_created']
note_spent = next(e for e in withdraw['sequencer_response']['events']
                  if e['key'] == 'ValueMidnightPrivacy/NoteSpent')['value']['note_spent']
pool_withdraw = next(e for e in withdraw['sequencer_response']['events']
                     if e['key'] == 'ValueMidnightPrivacy/PoolWithdraw')['value']['pool_withdraw']

cfg = {
  "program": "$PROGRAM_PATH",
  "shader-path": "$SHADER_PATH",
  "packing": 8192,
  "private-indices": [2,3,4,5,6] + list(range(8,24)) + [28,29,30],
  "args": [
      {"hex": note["domain"]},
      {"str": str(note["amount"])},
      {"hex": "0"*64},  # REDACTED: rho (private)
      {"hex": "0"*64},  # REDACTED: recipient (private)
      {"hex": "0"*64},  # REDACTED: nf_key (private)
      {"str": "0"},     # REDACTED: position (private)
      {"str": "16"},
      *({"hex": "0"*64} for _ in range(16)),  # REDACTED: siblings (private)
      {"hex": arr_to_hex(note_spent["anchor_root"])},
      {"hex": arr_to_hex(note_spent["nullifier"])},
      {"str": str(pool_withdraw["amount"])},
      {"str": "1"},
      {"str": "0"},     # REDACTED: change value (private)
      {"hex": "0"*64},  # REDACTED: change rho (private)
      {"hex": "0"*64},  # REDACTED: change recipient (private)
      {"hex": arr_to_hex(note_created["commitment"])}
  ]
}

print(json.dumps(cfg))
PY
)
else
    CONFIG=$(python3 - <<'PY'
import json, pathlib
root = pathlib.Path('.')

note = json.loads(root.joinpath('midnight_note_details.json').read_text())
deposit = json.loads(root.joinpath('deposit_response.json').read_text())
withdraw = json.loads(root.joinpath('withdraw_response.json').read_text())

def arr_to_hex(arr):
    return ''.join(f'{b:02x}' for b in arr)

pool_deposit = next(e for e in deposit['sequencer_response']['events']
                    if e['key'] == 'ValueMidnightPrivacy/PoolDeposit')['value']['pool_deposit']
note_created = next(e for e in withdraw['sequencer_response']['events']
                    if e['key'] == 'ValueMidnightPrivacy/NoteCreated')['value']['note_created']
note_spent = next(e for e in withdraw['sequencer_response']['events']
                  if e['key'] == 'ValueMidnightPrivacy/NoteSpent')['value']['note_spent']
pool_withdraw = next(e for e in withdraw['sequencer_response']['events']
                     if e['key'] == 'ValueMidnightPrivacy/PoolWithdraw')['value']['pool_withdraw']

cfg = {
  "program": "note_spend_guest.wasm",
  "shader-path": "shader",
  "packing": 8192,
  "private-indices": [2,3,4,5,6] + list(range(8,24)) + [28,29,30],
  "args": [
      {"hex": note["domain"]},
      {"str": str(note["amount"])},
      {"hex": "0"*64},  # REDACTED: rho (private)
      {"hex": "0"*64},  # REDACTED: recipient (private)
      {"hex": "0"*64},  # REDACTED: nf_key (private)
      {"str": "0"},     # REDACTED: position (private)
      {"str": "16"},
      *({"hex": "0"*64} for _ in range(16)),  # REDACTED: siblings (private)
      {"hex": arr_to_hex(note_spent["anchor_root"])},
      {"hex": arr_to_hex(note_spent["nullifier"])},
      {"str": str(pool_withdraw["amount"])},
      {"str": "1"},
      {"str": "0"},     # REDACTED: change value (private)
      {"hex": "0"*64},  # REDACTED: change rho (private)
      {"hex": "0"*64},  # REDACTED: change recipient (private)
      {"hex": arr_to_hex(note_created["commitment"])}
  ]
}

print(json.dumps(cfg))
PY
)
fi

# Save the config to a file
echo "$CONFIG" > verify_config.json

echo -e "${GREEN}✓ Generated verify_config.json${NC}\n"

# Generate the command
echo -e "${YELLOW}Command to run on the verifier machine:${NC}\n"

cat > verify_command.sh <<'EOF'
#!/bin/bash
# Verification command - portable to any machine with webgpu_verifier
# 
# Prerequisites:
# 1. Copy proof_data.gz to the same directory as this script
# 2. Copy verify_config.json to the same directory as this script
# 3. Ensure webgpu_verifier binary is in PATH or adjust the path below
# 4. Ensure shader directory is available or adjust the path in verify_config.json

VERIFIER="${WEBGPU_VERIFIER:-./webgpu_verifier}"

if [ ! -f "proof_data.gz" ]; then
    echo "Error: proof_data.gz not found in current directory"
    exit 1
fi

if [ ! -f "verify_config.json" ]; then
    echo "Error: verify_config.json not found in current directory"
    exit 1
fi

echo "Running verification..."
echo "Verifier: $VERIFIER"
echo "Config: verify_config.json"
echo "Proof: proof_data.gz ($(wc -c < proof_data.gz) bytes)"
echo ""

$VERIFIER "$(cat verify_config.json)"
EOF

chmod +x verify_command.sh

echo -e "${GREEN}✓ Generated verify_command.sh${NC}\n"

if [ "$USE_ABSOLUTE_PATHS" = true ]; then
    echo -e "${GREEN}=== Local Testing Mode ===${NC}"
    echo ""
    echo -e "${BLUE}Generated with absolute paths for local testing${NC}"
    echo ""
    echo -e "${BLUE}Test locally:${NC}"
    echo "  WEBGPU_VERIFIER=$REPO_ROOT/crates/adapters/ligero/bins/macos/bin/webgpu_verifier ./verify_command.sh"
    echo ""
    echo -e "${YELLOW}Note: This config contains absolute paths and won't work on another machine.${NC}"
    echo -e "${YELLOW}Run without --local flag to generate portable config.${NC}"
else
    echo -e "${GREEN}=== Portable Mode ===${NC}"
    echo ""
    echo -e "${BLUE}Files to copy to the verifier machine:${NC}"
    echo "  1. proof_data.gz ($(wc -c < proof_data.gz) bytes)"
    echo "  2. verify_config.json"
    echo "  3. verify_command.sh (optional - contains the command)"
    echo "  4. note_spend_guest.wasm (the WASM program)"
    echo "  5. shader/ directory (Ligero shader files)"
    echo ""

    echo -e "${BLUE}On the verifier machine:${NC}"
    echo "  1. Place all files in the same directory"
    echo "  2. Adjust paths in verify_config.json if needed"
    echo "  3. Set WEBGPU_VERIFIER environment variable to webgpu_verifier binary path"
    echo "  4. Run: ./verify_command.sh"
    echo ""
    echo -e "${BLUE}Or run directly:${NC}"
    echo ""
    echo "  ./webgpu_verifier \"\$(cat verify_config.json)\""
    echo ""
fi

echo -e "${GREEN}=== Done ===${NC}"
echo ""
echo -e "${YELLOW}Note: Private arguments have been redacted (replaced with zeros).${NC}"
echo -e "${YELLOW}The verifier only sees public inputs (domain, anchor, nullifier, amounts, commitments).${NC}"

