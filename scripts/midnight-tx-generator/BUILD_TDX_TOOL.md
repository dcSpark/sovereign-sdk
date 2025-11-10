# Building TDX Quote Generator

Quick guide to compile and use the TDX quote generation tool on your TDX instance.

## Prerequisites

Your system has `/dev/tdx-attest` and the Intel TDX attestation library installed.

## Build Steps

```bash
# 1. Navigate to the scripts directory
cd ~/sovereign-ligero/scripts/midnight-tx-generator

# 2. Compile the tool
make -f Makefile.tdx

# You should see:
# ✓ Built: tdx_quote_gen

# 3. Test it
echo "0000000000000000000000000000000000000000000000000000000000000000" | \
  xxd -r -p > test_reportdata.bin

sudo ./tdx_quote_gen test_reportdata.bin test_quote.dat

# You should see:
# ✓ Success! TDX quote generation complete.
```

## Usage

### Standalone
```bash
# Create REPORTDATA (64 bytes hex)
echo "YOUR_64_BYTE_HEX_STRING" | xxd -r -p > reportdata.bin

# Generate quote
sudo ./tdx_quote_gen reportdata.bin quote.dat

# Check output
ls -lh quote.dat
hexdump -C quote.dat | head -10
```

### With Attestation Script

Once compiled, the attestation script will automatically detect and use it:

```bash
# Generate attestation with real TDX quote
NONCE=$(openssl rand -hex 32)
./attest_verification.sh $NONCE my_attestation

# You should see:
#   Detected Intel TDX platform (/dev/tdx-attest)
#   Using compiled tdx_quote_gen (libtdx-attest)
#   ✓ TDX quote generated via libtdx-attest
```

## Install System-Wide (Optional)

```bash
# Install to /usr/local/bin
sudo make -f Makefile.tdx install

# Now you can use it from anywhere
cd ~/anywhere
tdx_quote_gen reportdata.bin quote.dat
```

## Troubleshooting

### Build Errors

**Error: "tdx_attest.h: No such file or directory"**

The libtdx-attest development files are not installed. Install them:

```bash
# If using Intel's TDX guest library
cd ~/SGXDataCenterAttestationPrimitives/QuoteGeneration
# Follow Intel's build instructions for libtdx-attest

# Or check if pre-built packages are available
sudo apt-cache search tdx-attest
sudo apt-cache search intel-tdx
```

**Error: "cannot find -ltdx-attest"**

The library is not in the linker path. Check where it's installed:

```bash
# Find the library
sudo find / -name "libtdx-attest.so*" 2>/dev/null

# If found in /usr/local/lib, add to library path
export LD_LIBRARY_PATH=/usr/local/lib:$LD_LIBRARY_PATH
sudo ldconfig

# Try building again
make -f Makefile.tdx
```

**Error: Compiler not found**

```bash
# Install build tools
sudo apt-get update
sudo apt-get install build-essential gcc make
```

### Runtime Errors

**Error: "Cannot open REPORTDATA file"**

Ensure the reportdata file exists and is readable:

```bash
ls -l reportdata.bin
hexdump -C reportdata.bin | head -5
```

**Error: "Failed to get TD report"**

Check device permissions:

```bash
ls -l /dev/tdx-attest
# Should show: crw-rw---- or similar

# Add yourself to the appropriate group or use sudo
sudo ./tdx_quote_gen reportdata.bin quote.dat
```

**Error: "Failed to get TD quote"**

The TDX Quote Generation Service (QGS) may not be configured:

```bash
# Check if QGS is running
ps aux | grep qgs

# Check dmesg for TDX messages
dmesg | grep -i tdx | tail -20

# Verify TDX is enabled
cat /sys/firmware/tdx/status 2>/dev/null || echo "Status file not found"
```

## Clean Up

```bash
# Remove build artifacts
make -f Makefile.tdx clean

# Uninstall from system
sudo rm /usr/local/bin/tdx_quote_gen
```

## Alternative: Using Your Sample Code

If you prefer to use your existing sample code that generates random reportdata:

```bash
# Copy your C code to this directory
cp /path/to/your/sample.c ./tdx_sample.c

# Compile it
gcc -o tdx_sample tdx_sample.c -ltdx-attest

# Run it (generates quote.dat and report.dat)
sudo ./tdx_sample

# Use the generated quote
cp quote.dat my_attestation/
```

## Integration Notes

The `attest_verification.sh` script tries these methods in order:

1. **Compiled `./tdx_quote_gen`** (this tool) ← Best option
2. **System `tdx_quote_gen`** (if installed globally)
3. **Intel `tdx_attest`** sample tool
4. **Python `tdx_quote_gen.py`** (for /dev/tdx_guest)
5. **Google `go-tdx-guest`**
6. **Mock quote** (fallback for testing)

Once you compile `tdx_quote_gen`, it will be automatically preferred.

## File Descriptions

- **`tdx_quote_gen.c`** - Source code (uses libtdx-attest)
- **`Makefile.tdx`** - Build instructions
- **`tdx_quote_gen`** - Compiled binary (after build)

## Example: Full Workflow

```bash
# 1. Build tool
cd ~/sovereign-ligero/scripts/midnight-tx-generator
make -f Makefile.tdx

# 2. Verify proof (if not already done)
./generate_verify_command.sh --local

# 3. Generate attestation with real TDX quote
NONCE=$(openssl rand -hex 32)
./attest_verification.sh $NONCE production_attestation

# 4. Verify quote is real (not mock)
if grep -q "MOCK" production_attestation/quote.dat; then
    echo "❌ Using mock quote!"
else
    echo "✅ Real TDX quote generated!"
    hexdump -C production_attestation/quote.dat | head -10
fi

# 5. Verify attestation
cd production_attestation
EXPECTED_NONCE=$NONCE ./verify_attestation.sh

# 6. Package for auditor
cd ..
tar czf production_attestation.tar.gz production_attestation/
```

## Production Checklist

Before using in production:

- [ ] `tdx_quote_gen` compiles and runs successfully
- [ ] Test quote generation produces binary output (not mock text)
- [ ] Quote size is reasonable (~4-8KB for TDX)
- [ ] Verification script confirms quote is not a mock
- [ ] Add MRENCLAVE/MRTD checks to verification
- [ ] Document your specific TDX platform configuration

## Support

- **Build issues**: Check Intel TDX documentation and DCAP build guides
- **Runtime issues**: Verify `/dev/tdx-attest` permissions and QGS configuration
- **Integration issues**: Check `attest_verification.sh` logs

---

**Quick Command Reference:**

```bash
# Build
make -f Makefile.tdx

# Test
sudo ./tdx_quote_gen test_reportdata.bin test_quote.dat

# Use with attestation
NONCE=$(openssl rand -hex 32) && ./attest_verification.sh $NONCE
```

