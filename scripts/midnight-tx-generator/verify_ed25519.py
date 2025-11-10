#!/usr/bin/env python3
"""
Verify Ed25519 signature using Python cryptography library
Works when OpenSSL pkeyutl doesn't support Ed25519
"""

import sys
from pathlib import Path

try:
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric import ed25519
    from cryptography.exceptions import InvalidSignature
except ImportError:
    print("Error: cryptography library not installed", file=sys.stderr)
    print("Install with: pip3 install cryptography", file=sys.stderr)
    sys.exit(1)

if len(sys.argv) != 4:
    print(f"Usage: {sys.argv[0]} <public_key.pem> <data_file> <signature_file>")
    print("\nVerifies Ed25519 signature")
    sys.exit(1)

public_key_file = Path(sys.argv[1])
data_file = Path(sys.argv[2])
signature_file = Path(sys.argv[3])

try:
    # Load public key
    public_key_pem = public_key_file.read_bytes()
    public_key = serialization.load_pem_public_key(public_key_pem)
    
    if not isinstance(public_key, ed25519.Ed25519PublicKey):
        print(f"Error: Key is not Ed25519 (got {type(public_key).__name__})", file=sys.stderr)
        sys.exit(1)
    
    # Read data and signature
    data = data_file.read_bytes()
    signature = signature_file.read_bytes()
    
    # Verify
    try:
        public_key.verify(signature, data)
        print("✓ Signature valid")
        sys.exit(0)
    except InvalidSignature:
        print("✗ Invalid signature", file=sys.stderr)
        sys.exit(1)
    
except Exception as e:
    print(f"Error: {e}", file=sys.stderr)
    import traceback
    traceback.print_exc()
    sys.exit(1)

