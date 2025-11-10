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
