#!/usr/bin/env python3
"""
Sign a file with Ed25519 private key using Python cryptography library
Works when OpenSSL pkeyutl doesn't support Ed25519
"""

import sys
from pathlib import Path

try:
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric import ed25519
except ImportError:
    print("Error: cryptography library not installed", file=sys.stderr)
    print("Install with: pip3 install cryptography", file=sys.stderr)
    sys.exit(1)

if len(sys.argv) != 4:
    print(f"Usage: {sys.argv[0]} <private_key.pem> <data_file> <signature_output>")
    print("\nSigns data_file with Ed25519 private key and writes signature to output")
    sys.exit(1)

private_key_file = Path(sys.argv[1])
data_file = Path(sys.argv[2])
signature_file = Path(sys.argv[3])

try:
    # Load private key
    private_key_pem = private_key_file.read_bytes()
    private_key = serialization.load_pem_private_key(private_key_pem, password=None)
    
    if not isinstance(private_key, ed25519.Ed25519PrivateKey):
        print(f"Error: Key is not Ed25519 (got {type(private_key).__name__})", file=sys.stderr)
        sys.exit(1)
    
    # Read data to sign
    data = data_file.read_bytes()
    
    # Sign
    signature = private_key.sign(data)
    
    # Write signature
    signature_file.write_bytes(signature)
    
    print(f"✓ Signed {len(data)} bytes")
    print(f"✓ Signature: {signature.hex()[:64]}...")
    print(f"✓ Written to: {signature_file}")
    
except Exception as e:
    print(f"Error: {e}", file=sys.stderr)
    import traceback
    traceback.print_exc()
    sys.exit(1)

