# TDX Quote Verification Setup

This guide explains how to set up **TDX quote verification** for the attestation system.

## Overview

The attestation process generates a **TDX quote** signed by Intel's attestation infrastructure. To trust this quote, you need to:

1. **Verify the quote signature chain** - Proves the quote is from genuine Intel hardware
2. **Extract REPORTDATA** - Contains the hash of the ephemeral public key
3. **Verify the statement signature** - Proves the TEE signed the attestation statement

## Why Quote Verification Matters

Without quote verification:
- ⚠️ Anyone could generate a fake quote claiming to be a TEE
- ⚠️ No cryptographic proof that the computation ran in a real TEE

With quote verification:
- ✓ Cryptographically proves the quote is from Intel-signed hardware
- ✓ Proves the specific measurements (MRTD, RTMR values)
- ✓ Establishes the complete attestation chain

## Installation Options

### Option 1: Google's go-tdx-guest (Recommended)

**Easiest and most reliable option.**

#### Prerequisites
- Go 1.19 or later

#### Install Go (if needed)
```bash
# Linux AMD64
wget https://go.dev/dl/go1.21.5.linux-amd64.tar.gz
sudo rm -rf /usr/local/go
sudo tar -C /usr/local -xzf go1.21.5.linux-amd64.tar.gz
export PATH=$PATH:/usr/local/go/bin
echo 'export PATH=$PATH:/usr/local/go/bin' >> ~/.bashrc
```

#### Install go-tdx-guest
```bash
# Install the 'check' tool
go install github.com/google/go-tdx-guest/tools/check@latest

# Add Go bin to PATH
export PATH="$PATH:$HOME/go/bin"
echo 'export PATH="$PATH:$HOME/go/bin"' >> ~/.bashrc

# Verify installation
check --help
```

#### Or use the installation script
```bash
./install_tdx_verification.sh
```

#### Verify a quote
```bash
check -in quote.dat
```

**Output on success:**
```
Verification succeeded
```

**Output on failure:**
```
Error: Quote verification failed: <reason>
```

### Option 2: Intel DCAP Libraries

**More complex, provides additional features.**

#### Install Intel SGX DCAP
```bash
# Add Intel SGX repository
echo 'deb [arch=amd64] https://download.01.org/intel-sgx/sgx_repo/ubuntu jammy main' | \
    sudo tee /etc/apt/sources.list.d/intel-sgx.list
wget -qO - https://download.01.org/intel-sgx/sgx_repo/ubuntu/intel-sgx-deb.key | \
    sudo apt-key add -

# Install DCAP packages
sudo apt update
sudo apt install -y \
    libsgx-dcap-ql-dev \
    libsgx-dcap-quote-verify-dev \
    libsgx-urts \
    libsgx-qe3-logic \
    libsgx-pce-logic \
    libsgx-dcap-default-qpl
```

#### Configure Quote Provider Library (QPL)
```bash
# Edit /etc/sgx_default_qcnl.conf
sudo nano /etc/sgx_default_qcnl.conf
```

Set the PCCS (Provisioning Certificate Caching Service) URL:
```json
{
  "pccs_url": "https://api.trustedservices.intel.com/sgx/certification/v4/",
  "use_secure_cert": true,
  "collateral_service": "https://api.trustedservices.intel.com/sgx/certification/v4/",
  "retry_times": 6,
  "retry_delay": 10
}
```

#### Compile custom verification tool
See `BUILD_TDX_TOOL.md` for details.

## Verification in Production

### Automatic Verification
The `verify_attestation.sh` script automatically uses quote verification if tools are available:

```bash
cd attestation_output
WEBGPU_VERIFIER=/path/to/webgpu_verifier \
EXPECTED_NONCE=<challenge_nonce> \
./verify_attestation.sh
```

**With quote verification:**
```
Step 1: Verify quote signature and chain
  Using Google's go-tdx-guest verification tool...
  ✓ TDX quote verified successfully
```

**Without quote verification:**
```
Step 1: Verify quote signature and chain
  ⚠ Quote verification tool not found (skipping platform check)
  
  To enable quote verification, install Google's go-tdx-guest:
    go install github.com/google/go-tdx-guest/tools/check@latest
```

### Manual Verification
```bash
# 1. Verify quote
check -in quote.dat

# 2. Extract REPORTDATA (first 64 bytes of quote user data)
xxd -p -l 64 -s 632 quote.dat | tr -d '\n'

# 3. Verify it matches SHA256(ephemeral_pubkey.hex)
sha256sum ephemeral_pubkey.hex

# 4. Verify statement signature
python3 <<EOF
from pathlib import Path
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import ed25519

pub_key = serialization.load_pem_public_key(Path('ephemeral_pubkey.pem').read_bytes())
data = Path('statement.canonical.json').read_bytes()
signature = Path('statement.sig').read_bytes()
pub_key.verify(signature, data)
print("✓ Signature valid")
EOF
```

## What the Quote Contains

A TDX quote includes:
- **REPORTDATA** - 64 bytes of user data (we put SHA256(K_pub) in first 32 bytes)
- **MRTD** - Measurement of initial TD image
- **RTMR** - Runtime measurements
- **ATTRIBUTES** - TD configuration flags
- **XFAM** - Extended features
- **Signature** - Quote signed by Intel's Quoting Enclave

## Troubleshooting

### "Quote verification failed"
- Ensure you're using a real TDX quote (not a mock/test quote)
- Check that PCCS URL is accessible
- Verify Intel attestation services are reachable
- Check system time is correct (affects certificate validation)

### "check: command not found"
```bash
# Verify Go is installed
go version

# Reinstall go-tdx-guest
go install github.com/google/go-tdx-guest/tools/check@latest

# Check PATH
echo $PATH | grep -o "$HOME/go/bin"
```

### Network issues
If behind a firewall/proxy, configure for Intel's attestation service:
```bash
export HTTPS_PROXY=your-proxy:port
check -in quote.dat
```

## References

- [Intel TDX Attestation](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-trust-domain-extensions.html)
- [Google go-tdx-guest](https://github.com/google/go-tdx-guest)
- [Intel SGX DCAP](https://github.com/intel/SGXDataCenterAttestationPrimitives)
- [IETF RATS Architecture](https://datatracker.ietf.org/doc/html/rfc9334)

## Security Considerations

### Production Requirements
For production attestation:
1. ✅ **MUST** verify quote signature chain
2. ✅ **MUST** verify REPORTDATA binding
3. ✅ **MUST** verify statement signature
4. ✅ **MUST** check quote freshness (timestamp/nonce)
5. ✅ **MUST** verify proof hash matches
6. ✅ **MUST** re-run proof verification

### Optional Checks
- Check MRTD against known good values
- Verify RTMR measurements
- Check TD attributes/configuration
- Validate TCB (Trusted Computing Base) level
- Verify certificate revocation status

### Development vs Production
- **Development**: Quote verification can be skipped for testing
- **Production**: Quote verification is **mandatory** for security

