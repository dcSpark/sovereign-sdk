#!/bin/bash
set -e

echo "╔═══════════════════════════════════════════════════════╗"
echo "║     Proof Verification Fraud Detection Test          ║"
echo "╚═══════════════════════════════════════════════════════╝"
echo ""
echo "Goal: Verify that the Ligero proof verification actually works"
echo "by attempting to submit a proof for value=50 while claiming value=40"
echo ""

TEMP_DIR="./proof_fraud_test"
rm -rf "$TEMP_DIR"
mkdir -p "$TEMP_DIR"

echo "[1/4] Copying existing proof for value=50..."
cp tps_test_verifier_temp/value_tx_49.json "$TEMP_DIR/original.json"
echo "      ✓ Proof file copied"

echo ""
echo "[2/4] Creating fraudulent transaction..."
python3 << 'PYTHON'
import json
with open('./proof_fraud_test/original.json', 'r') as f:
    data = json.load(f)
original = data['set_value_with_proof']['value']
print(f"      Proof proves value: {original}")
data['set_value_with_proof']['value'] = 40
with open('./proof_fraud_test/fraud.json', 'w') as f:
    json.dump(data, f)
print(f"      Modified to claim: 40")
print(f"      ✓ FRAUD CREATED (proof=50, claim=40)")
PYTHON

echo ""
echo "[3/4] Signing fraudulent transaction..."
../../target/debug/sov-cli transactions sign \
    --generation 900 \
    --key-nickname "DANGER__DO_NOT_USE_WITH_REAL_MONEY" \
    --json-output \
    from-file value-setter-zk \
    --max-fee 100000000000 \
    --path "$TEMP_DIR/fraud.json" 2>/dev/null > "$TEMP_DIR/signed.json"

echo "      ✓ Transaction signed"

echo ""
echo "[4/4] Submitting to verifier service..."
cat "$TEMP_DIR/signed.json" | jq -r '.signed_tx' | sed 's/^0x//' | xxd -r -p | base64 | tr -d '\n' | jq -Rs '{body: .}' > "$TEMP_DIR/payload.json"

RESPONSE=$(curl -s -X POST http://127.0.0.1:8080/verify-and-submit \
    -H "Content-Type: application/json" \
    --data-binary @"$TEMP_DIR/payload.json")

echo "$RESPONSE" > "$TEMP_DIR/response.json"

echo ""
echo "╔═══════════════════════════════════════════════════════╗"
echo "║                   TEST RESULT                         ║"
echo "╚═══════════════════════════════════════════════════════╝"
echo ""

if echo "$RESPONSE" | jq -e '.tx_hash' > /dev/null 2>&1; then
    TX_HASH=$(echo "$RESPONSE" | jq -r '.tx_hash')
    echo "❌ ❌ ❌  CRITICAL VULNERABILITY  ❌ ❌ ❌"
    echo ""
    echo "THE VERIFIER ACCEPTED A FRAUDULENT PROOF!"
    echo ""
    echo "  Attack Details:"
    echo "    • Proof generated for value=50"
    echo "    • Transaction claimed value=40"
    echo "    • Verifier DID NOT detect the fraud"
    echo "    • Transaction accepted: $TX_HASH"
    echo ""
    echo "This means Ligero proof verification is NOT working correctly!"
    exit 1
elif echo "$RESPONSE" | jq -e '.error' > /dev/null 2>&1; then
    ERROR=$(echo "$RESPONSE" | jq -r '.error')
    echo "✅ ✅ ✅  PROOF VERIFICATION WORKING!  ✅ ✅ ✅"
    echo ""
    echo "The verifier successfully REJECTED the fraudulent proof!"
    echo ""
    echo "  Test Details:"
    echo "    • Proof was for value=50"
    echo "    • Transaction claimed value=40"
    echo "    • Verifier correctly detected the fraud"
    echo ""
    echo "Rejection reason:"
    echo "  $ERROR"
else
    echo "⚠️  Unexpected response:"
    echo "$RESPONSE"
fi

echo ""
echo "Test artifacts saved in: $TEMP_DIR/"

