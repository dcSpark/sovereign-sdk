# TEE Attestation for Ligero Proof Verification

This directory contains scripts to generate and verify cryptographic attestations proving that Ligero proof verification happened inside a Trusted Execution Environment (TEE).

## Overview

The attestation protocol implements a challenge-response flow with the following guarantees:

1. **Freshness**: Challenger provides a random nonce to prevent replay attacks
2. **Binding**: Ephemeral signing key is cryptographically bound to TEE quote via REPORTDATA
3. **Integrity**: All artifacts (proof, quote, statement) are cryptographically hashed and signed
4. **Verifiability**: Any third party can independently verify the entire attestation chain

## Actors

- **Challenger**: Sends fresh nonce to initiate attestation
- **TD (Trusted Domain)**: Your code running inside the Confidential VM/TEE
- **Auditor/Verifier**: Public internet users who verify the attestation

## Protocol Flow

```
┌───────────┐           ┌─────────────┐           ┌──────────┐
│Challenger │           │TD (TEE)     │           │Auditor   │
└─────┬─────┘           └──────┬──────┘           └────┬─────┘
      │                        │                        │
      │  1. nonce (32 bytes)   │                        │
      ├───────────────────────>│                        │
      │                        │                        │
      │                        │ 2. Generate K_priv/K_pub
      │                        │ 3. H = SHA256(K_pub)  │
      │                        │ 4. Get TEE quote with H│
      │                        │ 5. Verify proof       │
      │                        │ 6. Sign statement     │
      │                        │                        │
      │  7. Attestation bundle │                        │
      │<───────────────────────┤                        │
      │                        │                        │
      │  8. Forward to auditor │                        │
      ├────────────────────────┼───────────────────────>│
      │                        │                        │
      │                        │  9. Verify attestation │
      │                        │<───────────────────────┤
      │                        │                        │
```

## Usage

### Step 1: Generate Attestation (inside TEE)

```bash
# 1. Generate a random nonce (Challenger sends this)
NONCE=$(openssl rand -hex 32)

# 2. Run the attestation script
./attest_verification.sh $NONCE [output_dir]
```

This generates an attestation bundle in `output_dir` (default: `attestation_<timestamp>`).

### Step 2: Send Bundle to Auditor

Copy the entire attestation directory to the auditor:

```bash
# Create tarball
tar czf attestation.tar.gz attestation_<timestamp>/

# Send to auditor (via any channel)
scp attestation.tar.gz auditor@example.com:
```

### Step 3: Verify Attestation (Auditor)

```bash
# Extract bundle
tar xzf attestation.tar.gz
cd attestation_<timestamp>/

# Verify (optionally set expected nonce)
EXPECTED_NONCE=<original_nonce> ./verify_attestation.sh

# Or verify with proof re-verification
EXPECTED_NONCE=<original_nonce> \
WEBGPU_VERIFIER=/path/to/webgpu_verifier \
./verify_attestation.sh
```

## Attestation Bundle Contents

Each attestation bundle contains:

| File | Description |
|------|-------------|
| `quote.dat` | TEE attestation quote (Intel/AMD/Google signed) |
| `statement.json` | Human-readable attestation statement |
| `statement.canonical.json` | Canonical JSON (what was signed) |
| `statement.sig` | Ed25519 signature over canonical statement |
| `ephemeral_pubkey.pem` | Ephemeral public key (PEM format) |
| `ephemeral_pubkey.hex` | Ephemeral public key (raw hex) |
| `proof_data.gz` | The Ligero proof that was verified |
| `verify_config.json` | Verification configuration |
| `verification_output.txt` | Full verifier output |
| `verify_attestation.sh` | Verification script for auditors |
| `MANIFEST.txt` | Human-readable manifest |

## Verification Steps

The verification script (`verify_attestation.sh`) performs the following checks:

### 1. Quote Verification
- ✅ Verifies TEE quote signature chain (Intel QPL/Google/AMD)
- ✅ Ensures quote is from legitimate TEE platform

### 2. Key Binding
- ✅ Extracts REPORTDATA from quote
- ✅ Computes `SHA256(ephemeral_pubkey)`
- ✅ Verifies it matches `REPORTDATA[0:32]`

### 3. Statement Signature
- ✅ Verifies Ed25519 signature over `statement.canonical.json`
- ✅ Uses `ephemeral_pubkey.pem`

### 4. Nonce Matching
- ✅ Verifies `statement.nonce` equals challenge nonce
- ✅ Prevents replay attacks

### 5. Hash Consistency
- ✅ Verifies `SHA256(quote.dat)` matches `statement.quote_hash`
- ✅ Verifies `SHA256(proof_data.gz)` matches `statement.proof_hash`

### 6. Proof Re-verification
- ✅ Re-runs Ligero verification on `proof_data.gz`
- ✅ Confirms verification succeeds

## Security Properties

### What is proven?
- ✅ A TEE with a specific identity (verified via quote) exists
- ✅ That TEE generated an ephemeral keypair
- ✅ That TEE verified the Ligero proof successfully
- ✅ The verification happened after the nonce was issued (freshness)
- ✅ All artifacts are cryptographically bound together

### What is NOT proven?
- ❌ Which specific code is running inside the TEE (requires MRENCLAVE/MRTD check)
- ❌ That the TEE hasn't been compromised (depends on platform security)
- ❌ Correctness of the proof itself (only that verification succeeded)

## TEE Platform Support

### Intel TDX
```bash
# Install Intel TDX tools
# https://github.com/intel/SGXDataCenterAttestationPrimitives

# Verify quote
tdx_verify_quote quote.dat
```

### Google Confidential VM
```bash
# Install Google TEE tools
# https://github.com/google/go-tpm-tools

# Verify attestation
google-tee-attest verify-quote --quote=quote.dat
```

### AMD SEV-SNP
```bash
# Install AMD SEV tools
# https://github.com/AMDESE/sev-tool

# Verify quote
sev-tool --verify quote.dat
```

### Development/Testing
The script generates **mock quotes** when no TEE platform is detected. These are clearly marked and should **never** be used in production.

## Example: Full Flow

```bash
# === On TEE (Prover) ===

# 1. Challenger sends nonce
NONCE="abcd1234..."

# 2. Generate attestation
cd /path/to/midnight-tx-generator
./attest_verification.sh $NONCE my_attestation

# 3. Send to auditor
tar czf attestation.tar.gz my_attestation/

# === On Auditor Machine ===

# 4. Extract and verify
tar xzf attestation.tar.gz
cd my_attestation/

# 5. Full verification
EXPECTED_NONCE="abcd1234..." \
WEBGPU_VERIFIER=/usr/local/bin/webgpu_verifier \
./verify_attestation.sh

# Output: ✓ ATTESTATION VERIFIED
```

## Statement Format

The signed statement (`statement.canonical.json`) contains:

```json
{
  "version": "1.0",
  "protocol": "ligero-tee-attestation",
  "timestamp": "2025-11-08T01:21:33Z",
  "nonce": "<challenger_nonce>",
  "verification": {
    "status": "SUCCESS",
    "proof_hash": "e0db4bad...",
    "config_hash": "ec120b74...",
    "verifier_version": "webgpu_verifier-1.0.0"
  },
  "attestation": {
    "quote_hash": "1e4e6fd7...",
    "pubkey_hash": "12d50b1d...",
    "reportdata": "12d50b1de8403881..."
  },
  "metadata": {
    "platform": "Linux",
    "architecture": "x86_64"
  }
}
```

## Integration with Production Systems

### 1. Add MRENCLAVE/MRTD Checks
Verify the TEE is running expected code:

```bash
# Extract MRTD from quote
MRTD=$(parse_quote_mrtd quote.dat)

# Compare with known good value
if [ "$MRTD" != "$EXPECTED_MRTD" ]; then
    echo "Wrong TEE code!"
    exit 1
fi
```

### 2. Implement Quote Verification
Replace mock verification with real platform verification:

```python
# Example with Intel DCAP
from dcap import verify_quote

result = verify_quote(quote_bytes)
if not result.is_valid:
    raise ValueError("Invalid TEE quote")
```

### 3. Store Attestations
Archive attestations for audit trail:

```bash
# Upload to immutable storage
aws s3 cp attestation.tar.gz \
    s3://audit-logs/$(date +%Y/%m/%d)/$NONCE.tar.gz \
    --storage-class GLACIER
```

## Troubleshooting

### "No TEE platform detected"
- **Cause**: Not running inside a TEE, or TEE device files not accessible
- **Solution**: Run inside a Confidential VM, or use mock quotes for testing

### "Statement signature verification failed"
- **Cause**: Files were modified after signing
- **Solution**: Re-generate attestation, ensure files aren't corrupted in transit

### "Proof verification failed"
- **Cause**: Invalid proof, or verifier misconfigured
- **Solution**: Check `verification_output.txt` for details

### "Quote verification failed"
- **Cause**: Quote signature invalid, or verification tools not installed
- **Solution**: Install platform-specific verification tools (tdx_verify_quote, etc.)

## References

- [Intel TDX Attestation](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-trust-domain-extensions.html)
- [Google Confidential Computing](https://cloud.google.com/confidential-computing)
- [AMD SEV-SNP](https://www.amd.com/en/developer/sev.html)
- [Ed25519 Signatures](https://ed25519.cr.yp.to/)

## License

Copyright (C) 2023-2025 Sovereign Labs  
Licensed under the Apache License, Version 2.0

