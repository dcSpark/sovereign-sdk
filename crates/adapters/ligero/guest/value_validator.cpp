/*
 * Ligero Guest Program: Value Validator
 * 
 * This program validates that a u32 value is within the range [0, 100].
 * The value is read from command-line arguments using WASI.
 * 
 * Arguments:
 *     [1]: <i64> The value to validate (as i64, will be cast to int)
 * 
 * The program uses assert_one() to enforce:
 *     - value <= 100
 * 
 * Copyright (C) 2023-2025 Sovereign Labs
 * Licensed under the Apache License, Version 2.0
 */

#include <ligetron/api.h>

/************************************************************
 * Arguments:
 *     [1]: <i64> Value to validate (must be in range [0, 100])
 ************************************************************/

int main(int argc, char *argv[]) {
    // Arguments are passed as char* pointers that need to be cast
    // argv[1] contains the i64 value as raw bytes
    int value = *reinterpret_cast<const int*>(argv[1]);
    
    // Enforce that the value is non-negative
    assert_one(value >= 0);
    
    // Enforce that the value is at most 100
    assert_one(value <= 100);
    
    // If both assertions pass, the proof is valid
    // The value itself becomes part of the proof's public output
    // through the prover's configuration
    
    return 0;
}
