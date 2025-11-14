/*
 * Ligero Guest Program: Value Validator with Input Verification
 * 
 * This program validates that:
 *   1. A value is within the range [0, 65535] (u16 range)
 *   2. The proven value matches the value claimed in the transaction
 * 
 * Arguments:
 *     [1]: <i64> The proven value (value being proven in ZK)
 *     [2]: <i64> The claimed value (from transaction input parameter)
 * 
 * The program uses assert_one() to enforce:
 *     - proven_value >= 0
 *     - proven_value <= 65535
 *     - proven_value == claimed_value (prevent proof substitution attacks)
 * 
 * Copyright (C) 2023-2025 Sovereign Labs
 * Licensed under the Apache License, Version 2.0
 */

#include <ligetron/api.h>

/************************************************************
 * Arguments:
 *     [1]: <i64> Proven value (must be in range [0, 65535])
 *     [2]: <i64> Claimed value (from transaction parameter)
 ************************************************************/

int main(int argc, char *argv[]) {
    // Arguments are passed as char* pointers that need to be cast
    // argv[1] contains the proven value (the value in the ZK proof)
    // argv[2] contains the claimed value (the value from the transaction)
    int proven_value = *reinterpret_cast<const int*>(argv[1]);
    int claimed_value = *reinterpret_cast<const int*>(argv[2]);
    
    // SECURITY: Enforce that the proven value is non-negative
    assert_one(proven_value >= 0);
    
    // SECURITY: Enforce that the proven value is at most 65535 (u16 max)
    assert_one(proven_value <= 65535);
    
    // CRITICAL SECURITY: Enforce that the proven value matches the claimed value
    // This prevents an attacker from using a proof for value X to claim value Y
    // For example, proving value=50 but claiming value=100 in the transaction
    assert_one(proven_value == claimed_value);
    
    // If all assertions pass, the proof is valid and:
    //   - The value is within the valid range [0, 65535]
    //   - The proven value matches what was claimed in the transaction
    
    return 0;
}
