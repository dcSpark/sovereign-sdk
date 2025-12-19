/*
 * Rust Guest Program: Value Validator (no_std, WASI Command)
 * 
 * This program validates that:
 *   1. A value is within the range [0, 65535] (u16 range)
 *   2. The proven value matches the value claimed in the transaction
 * 
 * This is a proof-of-concept to verify that Rust->WASM works with Ligero.
 * It implements the same logic and ABI as the original value_validator.cpp WAT.
 * 
 * Using no_std for minimal WASM size!
 * 
 * Entry point: _start (WASI command module)
 * 
 * Arguments (passed via WASI as raw i32 values, not ASCII):
 *     [1]: <i32> The proven value (value being proven in ZK)
 *     [2]: <i32> The claimed value (from transaction input parameter)
 * 
 * The module imports env.assert_one to match the original WAT's behavior.
 * 
 * Copyright (C) 2023-2025 Sovereign Labs
 * Licensed under the Apache License, Version 2.0
 */

#![no_std]

use core::panic::PanicInfo;

// Custom panic handler for no_std
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

// Import env.assert_one from host (matches original WAT)
#[link(wasm_import_module = "env")]
extern "C" {
    fn assert_one(x: i32);
}

// Import WASI functions
#[link(wasm_import_module = "wasi_snapshot_preview1")]
extern "C" {
    fn args_sizes_get(argc: *mut u32, argv_buf_size: *mut u32) -> u32;
    fn args_get(argv_ptrs: *mut *mut u8, argv_buf: *mut u8) -> u32;
    fn proc_exit(code: u32) -> !; // never returns
}

// Core validator logic - matches original WAT's three env.assert_one checks
#[inline(always)]
unsafe fn validate(proven: i32, claimed: i32) {
    // SECURITY: Enforce that the proven value is non-negative
    assert_one((proven >= 0) as i32);
    
    // SECURITY: Enforce that the proven value is at most 65535 (u16 max)
    assert_one((proven <= 65535) as i32);
    
    // CRITICAL SECURITY: Enforce that the proven value matches the claimed value
    // This prevents an attacker from using a proof for value X to claim value Y
    assert_one((proven == claimed) as i32);
}

// WASI command entry point (matches original WAT structure)
#[no_mangle]
pub unsafe extern "C" fn _start() -> ! {
    // 1) Query sizes
    let mut argc: u32 = 0;
    let mut buf_len: u32 = 0;
    if args_sizes_get(&mut argc, &mut buf_len) != 0 {
        proc_exit(71); // error path (matches original WAT error code)
    }

    // 2) Small fixed buffers (no_std). Adjust caps for your use case.
    const MAX_ARGS: usize = 8;
    const MAX_BUF: usize = 128;
    if (argc as usize) > MAX_ARGS || (buf_len as usize) > MAX_BUF {
        proc_exit(71);
    }

    let mut ptrs: [*mut u8; MAX_ARGS] = [core::ptr::null_mut(); MAX_ARGS];
    let mut buf: [u8; MAX_BUF] = [0; MAX_BUF];

    // 3) Fetch argv into our buffers
    if args_get(ptrs.as_mut_ptr(), buf.as_mut_ptr()) != 0 {
        proc_exit(71);
    }

    // 4) Interpret argv[1] and argv[2] as raw i32 little-endian values
    // This matches the original WAT's i32.load operations
    let p1 = ptrs[1] as *const u32;
    let p2 = ptrs[2] as *const u32;
    let proven = core::ptr::read_unaligned(p1) as i32;
    let claimed = core::ptr::read_unaligned(p2) as i32;

    // 5) Run the same three checks using the host assert
    validate(proven, claimed);

    // 6) Normal WASI exit
    proc_exit(0)
}

