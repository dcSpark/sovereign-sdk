# Intel TDX Quote Generation Setup

This guide explains how to generate real Intel TDX attestation quotes instead of mock quotes.

## Quick Start

The attestation script (`attest_verification.sh`) automatically tries these methods in order:

1. **`tdx_attest`** - Intel TDX attestation sample tool
2. **`tdx_quote_gen.py`** - Direct ioctl to `/dev/tdx_guest` (included)
3. **`go-tdx-guest`** - Google's TDX guest tools
4. **Mock quote** - Fallback for testing

## Method 1: Intel TDX Attestation Sample (Recommended)

### Installation

```bash
# Install dependencies
sudo apt-get update
sudo apt-get install -y build-essential git cmake pkg-config libssl-dev

# Clone Intel DCAP repository
git clone https://github.com/intel/SGXDataCenterAttestationPrimitives.git
cd SGXDataCenterAttestationPrimitives

# Build TDX attestation tools
cd QuoteGeneration/qcnl/linux
make
sudo make install

cd ../../pccs
# Follow PCCS setup instructions if needed

# Build tdx_attest sample
cd ../../SampleCode/QuoteGenerationSample
make
sudo cp tdx_attest /usr/local/bin/
```

### Usage

```bash
# Generate quote with REPORTDATA
tdx_attest -r reportdata.bin -q quote.dat

# The attestation script will automatically use this if available
./attest_verification.sh $NONCE
```

### Configuration

Configure Quote Provider Library (QPL) for quote generation:

```bash
# Edit /etc/sgx_default_qcnl.conf
sudo tee /etc/sgx_default_qcnl.conf > /dev/null <<'EOF'
{
  "pccs_url": "https://localhost:8081/sgx/certification/v4/",
  "use_secure_cert": true,
  "collateral_service": "https://api.trustedservices.intel.com/sgx/certification/v4/",
  "retry_times": 3,
  "retry_delay": 3
}
EOF
```

## Method 2: Direct ioctl (Included)

The script includes `tdx_quote_gen.py` which directly calls the kernel ioctl.

### Requirements

- Python 3.6+
- Running inside TDX guest VM
- `/dev/tdx_guest` device accessible
- Root or CAP_SYS_RAWIO capability

### Usage

```bash
# Generate quote directly
sudo python3 tdx_quote_gen.py reportdata.bin quote.dat

# Or let attestation script use it automatically
sudo ./attest_verification.sh $NONCE
```

### Troubleshooting

If you get "Permission denied":
```bash
# Grant permission to /dev/tdx_guest
sudo chmod 666 /dev/tdx_guest

# Or add user to appropriate group
sudo usermod -a -G tdx $USER
```

If you get "QEMU/QVL not configured":
- Ensure QEMU is launched with TDX quote generation service
- Example QEMU command:
  ```bash
  qemu-system-x86_64 \
    -machine q35,confidential-guest-support=tdx \
    -object tdx-guest,id=tdx,quote-generation-socket=/path/to/qgs.sock \
    ...
  ```

## Method 3: Google go-tdx-guest

### Installation

```bash
# Install Go (if not already installed)
wget https://go.dev/dl/go1.21.0.linux-amd64.tar.gz
sudo tar -C /usr/local -xzf go1.21.0.linux-amd64.tar.gz
export PATH=$PATH:/usr/local/go/bin

# Install go-tdx-guest
go install github.com/google/go-tdx-guest/tools/go-tdx-guest@latest
sudo cp ~/go/bin/go-tdx-guest /usr/local/bin/
```

### Usage

```bash
# Generate quote
go-tdx-guest report -report-data reportdata.bin -out quote.dat

# Verify quote
go-tdx-guest verify -in quote.dat
```

## QEMU/KVM TDX Setup

To run TDX guests with quote generation:

### Host Setup

```bash
# Install TDX-enabled kernel
sudo apt install linux-image-tdx-guest

# Enable TDX in BIOS/UEFI
# Check TDX support
dmesg | grep -i tdx
```

### Launch TDX Guest

```bash
# Start QGS (Quote Generation Service)
qemu-system-x86_64 \
  -machine type=q35,kernel_irqchip=split \
  -cpu host,-kvm-steal-time \
  -smp 4 \
  -m 8G \
  -object tdx-guest,id=tdx,quote-generation-socket=/tmp/qgs.sock \
  -machine confidential-guest-support=tdx \
  -device virtio-net-pci,netdev=net0 \
  -netdev user,id=net0,hostfwd=tcp::2222-:22 \
  -drive if=virtio,format=qcow2,file=ubuntu.qcow2 \
  -nographic
```

### Inside Guest

```bash
# Verify TDX is active
ls -l /dev/tdx_guest
# Should show: crw-rw---- 1 root root 10, 125 ...

# Test quote generation
echo "0000000000000000000000000000000000000000000000000000000000000000" | \
  xxd -r -p > test_reportdata.bin

sudo python3 tdx_quote_gen.py test_reportdata.bin test_quote.dat

# Check quote was generated
ls -lh test_quote.dat
# Should show ~4-8KB file
```

## Cloud Platforms

### Google Cloud Confidential VMs

Google Cloud has built-in TDX support:

```bash
# Create TDX VM
gcloud compute instances create tdx-vm \
  --zone=us-central1-a \
  --machine-type=n2d-standard-4 \
  --confidential-compute \
  --maintenance-policy=TERMINATE

# Inside VM, use google-tee-attest
sudo apt install google-guest-attestation
google-tee-attest generate-quote --report-data=reportdata.bin --output=quote.dat
```

### Azure Confidential Computing

```bash
# Create TDX VM (when available)
az vm create \
  --resource-group myResourceGroup \
  --name tdx-vm \
  --size Standard_DC4s_v3 \
  --security-type ConfidentialVM \
  --os-disk-security-encryption-type VMGuestStateOnly

# Use Azure attestation service
# https://learn.microsoft.com/en-us/azure/attestation/
```

### AWS Nitro Enclaves (Alternative)

While AWS doesn't support TDX yet, Nitro Enclaves provide similar functionality:

```bash
# Install Nitro CLI
sudo amazon-linux-extras enable aws-nitro-enclaves-cli
sudo yum install aws-nitro-enclaves-cli -y

# Get attestation document
nitro-cli describe-enclaves
```

## Verifying Quotes

After generating a quote, verify it works:

```bash
# Run attestation
NONCE=$(openssl rand -hex 32)
./attest_verification.sh $NONCE my_attestation

# Check quote type
cd my_attestation
file quote.dat
# Should NOT say "ASCII text" (that's a mock quote)
# Should be binary data

# Verify quote structure
hexdump -C quote.dat | head -20
# Should show binary TDX quote structure

# Full verification
EXPECTED_NONCE=$NONCE ./verify_attestation.sh
```

## Production Checklist

Before using in production:

- [ ] Running on actual Intel TDX hardware (not emulated)
- [ ] `/dev/tdx_guest` device exists and is accessible
- [ ] Quote generation succeeds (not using mock quotes)
- [ ] PCCS/QGS configured for quote verification
- [ ] Quotes are binary (not "MOCK_TDX_QUOTE_v1" text)
- [ ] Verification includes MRENCLAVE/MRTD checks
- [ ] Attestation scripts run with minimal privileges (no unnecessary root)

## Debugging

### Check TDX Status

```bash
# Is TDX supported?
cpuid | grep -i tdx

# Is TDX device available?
ls -l /dev/tdx_guest

# Can we read from device?
sudo cat /dev/tdx_guest

# Check kernel logs
dmesg | grep -i tdx
```

### Quote Generation Failures

```bash
# Enable debug output
./attest_verification.sh $NONCE 2>&1 | tee attest_debug.log

# Check python script directly
sudo python3 -u tdx_quote_gen.py reportdata.bin quote.dat

# Verify REPORTDATA format
hexdump -C reportdata.bin | head -5
# Should be exactly 64 bytes (512 bits)
```

### Common Errors

**"QEMU/QVL not configured"**
- Solution: Launch QEMU with `-device tdx-guest,quote-generation-socket=...`

**"Permission denied"**
- Solution: `sudo chmod 666 /dev/tdx_guest` or run as root

**"Quote generation failed"**
- Check PCCS configuration in `/etc/sgx_default_qcnl.conf`
- Ensure network access to Intel PCS (or local PCCS)

## References

- [Intel TDX Documentation](https://www.intel.com/content/www/us/en/developer/tools/trust-domain-extensions/documentation.html)
- [Intel DCAP GitHub](https://github.com/intel/SGXDataCenterAttestationPrimitives)
- [Google go-tdx-guest](https://github.com/google/go-tdx-guest)
- [TDX Guest Kernel Interface](https://www.kernel.org/doc/html/latest/x86/tdx.html)
- [QEMU TDX Support](https://github.com/qemu/qemu/blob/master/docs/system/i386/tdx.rst)

## Support

For issues with:
- **TDX attestation**: Check Intel TDX forums or GitHub issues
- **Quote generation**: Verify QEMU/QGS setup and PCCS connectivity  
- **This script**: Open an issue in this repository

---

**Note**: This guide focuses on Intel TDX. For AMD SEV-SNP or ARM CCA, similar concepts apply but tools and procedures differ.

