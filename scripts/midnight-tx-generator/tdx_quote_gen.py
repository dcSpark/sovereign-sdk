#!/usr/bin/env python3
"""
Intel TDX Quote Generation via Direct ioctl

This script generates a TDX attestation quote by directly calling the
TDX_CMD_GET_QUOTE ioctl on /dev/tdx_guest.

Usage:
    python3 tdx_quote_gen.py <reportdata.bin> <quote_output.dat>

Requirements:
    - Running inside an Intel TDX guest VM
    - /dev/tdx_guest device accessible
    - Root or appropriate permissions

Based on Intel TDX Guest-Host Communication Interface (GHCI) specification.
"""

import sys
import os
import struct
import fcntl
from pathlib import Path

# TDX ioctl command codes (from tdx-guest.h)
TDX_CMD_GET_REPORT = 0xc0104401
TDX_CMD_GET_QUOTE = 0xc0104402

# Quote request structure (must match kernel ABI)
# struct tdx_quote_req {
#     __u64 buf;           // Quote buffer address
#     __u64 len;           // Buffer length
#     __u64 reportdata;    // REPORTDATA address (64 bytes)
# };

def get_tdx_report(reportdata: bytes) -> bytes:
    """
    Get TDX report via ioctl.
    
    Args:
        reportdata: 64-byte REPORTDATA
        
    Returns:
        1024-byte TDX report
    """
    if len(reportdata) != 64:
        raise ValueError(f"REPORTDATA must be exactly 64 bytes, got {len(reportdata)}")
    
    try:
        with open("/dev/tdx_guest", "rb") as f:
            # Allocate buffer for report (1024 bytes)
            report_buf = bytearray(1024)
            
            # Create request structure
            # Format: QQQ (3x uint64)
            #   - report buffer address
            #   - report buffer length
            #   - reportdata buffer address
            import ctypes
            
            report_addr = ctypes.addressof(ctypes.c_char.from_buffer(report_buf))
            reportdata_addr = ctypes.addressof(ctypes.c_char.from_buffer(bytearray(reportdata)))
            
            req = struct.pack("QQQ", report_addr, 1024, reportdata_addr)
            
            # Call ioctl
            result = fcntl.ioctl(f, TDX_CMD_GET_REPORT, req)
            
            return bytes(report_buf)
    
    except FileNotFoundError:
        raise RuntimeError("/dev/tdx_guest not found - are you running in a TDX guest?")
    except PermissionError:
        raise RuntimeError("Permission denied accessing /dev/tdx_guest - try running as root")
    except Exception as e:
        raise RuntimeError(f"Failed to get TDX report: {e}")


def get_tdx_quote(reportdata: bytes, max_quote_size: int = 8192) -> bytes:
    """
    Get TDX attestation quote via ioctl.
    
    Args:
        reportdata: 64-byte REPORTDATA
        max_quote_size: Maximum quote size (default 8KB)
        
    Returns:
        TDX quote (size varies)
    """
    if len(reportdata) != 64:
        raise ValueError(f"REPORTDATA must be exactly 64 bytes, got {len(reportdata)}")
    
    try:
        with open("/dev/tdx_guest", "rb") as f:
            # Allocate buffer for quote
            quote_buf = bytearray(max_quote_size)
            
            # Create request structure
            import ctypes
            
            quote_addr = ctypes.addressof(ctypes.c_char.from_buffer(quote_buf))
            reportdata_addr = ctypes.addressof(ctypes.c_char.from_buffer(bytearray(reportdata)))
            
            req = struct.pack("QQQ", quote_addr, max_quote_size, reportdata_addr)
            
            # Call ioctl
            try:
                result = fcntl.ioctl(f, TDX_CMD_GET_QUOTE, req)
            except OSError as e:
                if e.errno == 22:  # EINVAL
                    raise RuntimeError(
                        "TDX quote generation failed - QEMU/QVL may not be configured. "
                        "Ensure Quote Generation Service is running."
                    )
                raise
            
            # Find actual quote size (TDX quotes are typically 4-8KB)
            # The quote structure has a size field at offset 4
            if len(quote_buf) >= 8:
                # Parse quote header to get actual size
                # Quote format: version(2) | attestation_key_type(2) | reserved(4) | qe_svn(2) | pce_svn(2) | ...
                # Actual size is typically indicated in the header
                actual_size = len(quote_buf)
                # Trim trailing zeros
                for i in range(len(quote_buf) - 1, -1, -1):
                    if quote_buf[i] != 0:
                        actual_size = i + 1
                        break
                
                return bytes(quote_buf[:actual_size])
            
            return bytes(quote_buf)
    
    except FileNotFoundError:
        raise RuntimeError("/dev/tdx_guest not found - are you running in a TDX guest?")
    except PermissionError:
        raise RuntimeError("Permission denied accessing /dev/tdx_guest - try running as root")
    except Exception as e:
        raise RuntimeError(f"Failed to get TDX quote: {e}")


def main():
    if len(sys.argv) != 3:
        print("Usage: tdx_quote_gen.py <reportdata.bin> <quote_output.dat>")
        print("")
        print("Generates an Intel TDX attestation quote via /dev/tdx_guest ioctl")
        print("")
        print("Arguments:")
        print("  reportdata.bin    - Input file with 64-byte REPORTDATA")
        print("  quote_output.dat  - Output file for TDX quote")
        sys.exit(1)
    
    reportdata_file = Path(sys.argv[1])
    quote_file = Path(sys.argv[2])
    
    # Read REPORTDATA
    try:
        reportdata = reportdata_file.read_bytes()
    except FileNotFoundError:
        print(f"Error: REPORTDATA file not found: {reportdata_file}")
        sys.exit(1)
    
    if len(reportdata) != 64:
        print(f"Error: REPORTDATA must be exactly 64 bytes, got {len(reportdata)}")
        print(f"Hint: Ensure the file contains binary data, not hex text")
        sys.exit(1)
    
    print(f"REPORTDATA: {reportdata.hex()[:64]}...")
    
    # Generate quote
    try:
        print("Requesting TDX quote via /dev/tdx_guest ioctl...")
        quote = get_tdx_quote(reportdata)
        print(f"✓ Quote generated: {len(quote)} bytes")
        
        # Write quote to file
        quote_file.write_bytes(quote)
        print(f"✓ Quote written to: {quote_file}")
        
        # Display quote info
        if len(quote) >= 16:
            print(f"\nQuote header (first 16 bytes): {quote[:16].hex()}")
        
        return 0
    
    except RuntimeError as e:
        print(f"Error: {e}")
        print("\nTroubleshooting:")
        print("  1. Ensure you're running inside an Intel TDX guest VM")
        print("  2. Check /dev/tdx_guest exists and is accessible")
        print("  3. Try running with sudo: sudo python3 tdx_quote_gen.py ...")
        print("  4. Verify TDX Quote Generation Service is configured")
        print("     (QEMU: -device tdx-guest,quote-generation-socket=...)")
        return 1
    
    except Exception as e:
        print(f"Unexpected error: {e}")
        import traceback
        traceback.print_exc()
        return 1


if __name__ == "__main__":
    sys.exit(main())

