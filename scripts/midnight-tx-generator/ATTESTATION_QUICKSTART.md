# TEE Attestation Quick Start Guide

Complete guide for generating and verifying Ligero proof attestations in Intel TDX.

## 🎯 What This System Does

Cryptographically proves that a **Ligero zero-knowledge proof was verified inside an Intel TDX Trusted Execution Environment**.

## 📋 Prerequisites

### On TEE Instance (where proof is verified)
```bash
# Required
- Intel TDX guest VM with /dev/tdx_guest
- libtdx_attest library installed
- webgpu_verifier binary
- Python 3 with cryptography library

# Install dependencies
pip3 install cryptography
```

### On Verifier Machine (can be anywhere)
```bash
# Required for full verification
- Python 3 with cryptography library
- Go 1.19+ (for quote verification)
- webgpu_verifier binary (for proof re-verification)

# Install quote verification
go install github.com/google/go-tdx-guest/tools/check@latest
export PATH="$PATH:$HOME/go/bin"
```

## 🚀 Quick Start

### Step 1: Generate Proof (on TEE)
```bash
cd ~/ligero-verifier
./deposit_and_withdraw.sh
# Generates: proof_data.gz, verify_config.json, etc.
```

### Step 2: Generate Attestation (on TEE)
```bash
# Generate a challenge nonce
NONCE=$(openssl rand -hex 32)

# Run attestation
WEBGPU_VERIFIER=/path/to/webgpu_verifier \
./attest_verification.sh $NONCE ./attestation_output
```

**Output:**
```
=== TEE Attestation for Ligero Proof Verification ===
Step 1: Generate ephemeral Ed25519 keypair inside TEE
  ✓ Keypair generated
Step 2: Prepare REPORTDATA with SHA256(K_pub)
  ✓ REPORTDATA prepared
Step 3: Generate TEE attestation quote
  ✓ TDX quote generated
Step 4: Verify Ligero proof inside TEE
  ✓ Proof verification PASSED
Step 5: Compute cryptographic hashes
  ✓ Hashes computed
Step 6: Build attestation statement
  ✓ Statement created
Step 7: Sign statement with ephemeral key
  ✓ Statement signed
Step 8: Package attestation bundle
  ✓ Bundle created

Send to Challenger/Auditor: ./attestation_output
```

### Step 3: Transfer Bundle to Verifier
```bash
# From TEE instance
tar -czf attestation.tar.gz attestation_output/
scp attestation.tar.gz verifier@machine:~/

# On verifier machine
tar -xzf attestation.tar.gz
cd attestation_output
```

### Step 4: Verify Attestation (on verifier machine)
```bash
cd attestation_output

WEBGPU_VERIFIER=/path/to/webgpu_verifier \
EXPECTED_NONCE=$NONCE \
./verify_attestation.sh
```

**Expected output:**
```
=== Verifying TEE Attestation Bundle ===
✓ All required files present

Step 1: Verify quote signature and chain
  Using Google's go-tdx-guest verification tool...
  ✓ TDX quote verified successfully

Step 2: Verify ephemeral key binding in REPORTDATA
  ✓ Pubkey hash matches REPORTDATA

Step 3: Verify statement signature
  ✓ Statement signature valid

Step 4: Verify challenge nonce
  ✓ Nonce matches challenge

Step 5: Verify hash consistency
  ✓ Quote hash matches
  ✓ Proof hash matches

Step 6: Re-verify Ligero proof
  ✓ Ligero proof re-verification PASSED

═══════════════════════════════════════════════
✓ ATTESTATION VERIFIED
═══════════════════════════════════════════════

The following has been cryptographically proven:
  1. A TEE (quote verified) with REPORTDATA=<hash>
  2. Holds ephemeral key K with SHA256(K_pub) = REPORTDATA[0:32]
  3. Signed a statement claiming:
     - Challenge nonce: <your_nonce>
     - Verified proof with hash: <proof_hash>
     - Quote hash: <quote_hash>
  4. The claimed proof verification is reproducible

Conclusion: The Ligero proof was verified inside the attested TEE.
```

## 📦 Files in Attestation Bundle

```
attestation_output/
├── quote.dat                    # TDX quote (Intel-signed)
├── reportdata.bin               # SHA256(ephemeral_pubkey) || zeros
├── statement.json               # Human-readable attestation
├── statement.canonical.json     # Canonical JSON (what was signed)
├── statement.sig                # Ed25519 signature (64 bytes)
├── ephemeral_pubkey.pem         # Public key (PEM format)
├── ephemeral_pubkey.hex         # Public key (raw hex)
├── proof_data.gz                # The Ligero proof
├── verify_config.json           # Verification configuration
├── note_spend_guest.wasm        # WASM program for verification
├── shader/                      # WebGPU shaders
├── verification_output.txt      # Full verifier output
├── verify_attestation.sh        # Automated verification script
└── MANIFEST.txt                 # Bundle documentation
```

## 🔐 Security Properties

### What's Proven
1. ✅ **TEE Authenticity**: Quote is signed by Intel's attestation infrastructure
2. ✅ **Key Binding**: Ephemeral key is bound to TEE via REPORTDATA
3. ✅ **Statement Integrity**: Statement is signed by TEE-held private key
4. ✅ **Nonce Freshness**: Statement includes challenger's nonce
5. ✅ **Proof Verification**: Ligero proof was actually verified
6. ✅ **Reproducibility**: Auditor can re-verify the proof

### Attack Resistance
- ❌ **Cannot forge**: Quote signature requires Intel's signing key
- ❌ **Cannot replay**: Each attestation uses a unique nonce
- ❌ **Cannot substitute**: Proof hash is in signed statement
- ❌ **Cannot extract**: Private inputs are redacted from config

## 🛠️ Troubleshooting

### "Quote verification tool not found"
Install Google's go-tdx-guest:
```bash
./install_tdx_verification.sh
# OR manually:
go install github.com/google/go-tdx-guest/tools/check@latest
export PATH="$PATH:$HOME/go/bin"
```

### "Python signing failed"
Install cryptography library:
```bash
pip3 install cryptography
```

### "Ligero proof re-verification FAILED"
Ensure `webgpu_verifier` is in PATH or set explicitly:
```bash
WEBGPU_VERIFIER=/full/path/to/webgpu_verifier ./verify_attestation.sh
```

### "WASM program not found"
The attestation bundle should include all files. If missing:
```bash
# Ensure these exist before running attest_verification.sh:
ls -la note_spend_guest.wasm
ls -la shader/
```

## 📚 Documentation

- `QUOTE_VERIFICATION.md` - Detailed quote verification setup
- `TDX_SETUP.md` - Intel TDX environment configuration
- `BUILD_TDX_TOOL.md` - Building custom TDX tools
- `ATTESTATION_PROTOCOL.md` - Protocol specification (if exists)

## 🔄 Challenge-Response Protocol

```
┌──────────┐                    ┌─────────┐                    ┌──────────┐
│Challenger│                    │   TEE   │                    │ Auditor  │
└────┬─────┘                    └────┬────┘                    └────┬─────┘
     │                               │                              │
     │ 1. Generate nonce             │                              │
     ├──────────────────────────────>│                              │
     │                               │                              │
     │                               │ 2. Generate ephemeral keypair│
     │                               │    (K_pub, K_priv)           │
     │                               │                              │
     │                               │ 3. Generate quote with       │
     │                               │    REPORTDATA=SHA256(K_pub)  │
     │                               │                              │
     │                               │ 4. Verify Ligero proof       │
     │                               │                              │
     │                               │ 5. Sign statement with K_priv│
     │                               │    (includes nonce, hashes)  │
     │                               │                              │
     │ 6. Attestation bundle         │                              │
     │<──────────────────────────────┤                              │
     │                               │                              │
     │ 7. Forward to auditor         │                              │
     ├────────────────────────────────────────────────────────────>│
     │                               │                              │
     │                               │                 8. Verify quote│
     │                               │                 9. Check binding│
     │                               │                 10. Verify sig│
     │                               │                 11. Check nonce│
     │                               │                 12. Re-verify proof│
     │                               │                              │
     │                               │            13. Verdict       │
     │<─────────────────────────────────────────────────────────────┤
     │                               │                              │
```

## 🎓 Next Steps

### Development
1. ✅ Generate and verify attestations locally
2. ✅ Test with different proofs
3. ✅ Experiment with mock quotes (for testing)

### Production
1. ✅ Set up proper PCCS (Intel's certificate service)
2. ✅ Enable quote verification (`check` tool)
3. ✅ Implement policy checks (MRTD, RTMR values)
4. ✅ Add certificate revocation checking
5. ✅ Implement attestation expiry/freshness policies

## 📞 Support

- GitHub Issues: [sovereign-ligero repository]
- Documentation: See `docs/` directory
- Intel TDX Docs: https://www.intel.com/tdx

## ⚖️ License

See LICENSE.md in the repository root.

