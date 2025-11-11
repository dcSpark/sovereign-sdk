//! Benchmark for discriminant checking performance
//! Run with: cargo test --test discriminant_bench --release -- --nocapture

use borsh::{BorshDeserialize, BorshSerialize};
use sov_modules_api::FullyBakedTx;
use std::time::Instant;

/// Mock RuntimeCall enum to simulate the real one
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug)]
enum MockRuntimeCall {
    Bank(u64),
    SequencerRegistry(u64),
    OperatorIncentives(u64),
    AttesterIncentives(u64),
    ProverIncentives(u64),
    Accounts(u64),
    Uniqueness(u64),
    ChainState(u64),
    BlobStorage(u64),
    Paymaster(u64),
    Evm(u64),
    AccessPattern(u64),
    SyntheticLoad(u64),
    ValueSetter(u64),
    ValueSetterZk(u64),
    MidnightPrivacy(u64), // Discriminant = 15 (0-indexed)
}

const MIDNIGHT_PRIVACY_DISCRIMINANT: u8 = 15;

/// Option 1: Raw byte check
#[inline(never)]
fn option1_raw_byte_check(baked_tx: &FullyBakedTx) -> bool {
    baked_tx.data.first() == Some(&MIDNIGHT_PRIVACY_DISCRIMINANT)
}

/// Option 2: Borsh deserialize discriminant only
#[inline(never)]
fn option2_deserialize_discriminant(baked_tx: &FullyBakedTx) -> bool {
    let mut data = &baked_tx.data[..];
    if let Ok(discriminant) = u8::deserialize(&mut data) {
        discriminant == MIDNIGHT_PRIVACY_DISCRIMINANT
    } else {
        false
    }
}

/// Option 3: Full deserialization (for comparison)
#[inline(never)]
fn option3_full_deserialize(baked_tx: &FullyBakedTx) -> bool {
    let mut data = &baked_tx.data[..];
    if let Ok(call) = MockRuntimeCall::deserialize(&mut data) {
        matches!(call, MockRuntimeCall::MidnightPrivacy(_))
    } else {
        false
    }
}

fn create_mock_tx(discriminant: u8, payload_size: usize) -> FullyBakedTx {
    let call = match discriminant {
        0 => MockRuntimeCall::Bank(42),
        15 => MockRuntimeCall::MidnightPrivacy(42),
        _ => MockRuntimeCall::Bank(42),
    };
    
    let mut data = borsh::to_vec(&call).unwrap();
    // Pad with extra data to simulate real transaction size
    data.extend(vec![0u8; payload_size]);
    
    FullyBakedTx::new(data)
}

fn format_duration(ns: u128) -> String {
    if ns < 1000 {
        format!("{} ns", ns)
    } else if ns < 1_000_000 {
        format!("{:.2} µs", ns as f64 / 1000.0)
    } else {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    }
}

#[test]
fn bench_discriminant_checks() {
    println!("\n════════════════════════════════════════════════════════════");
    println!("   DISCRIMINANT CHECK PERFORMANCE BENCHMARK");
    println!("════════════════════════════════════════════════════════════\n");

    const WARMUP_ITERATIONS: usize = 10_000;
    const ITERATIONS: usize = 1_000_000;
    
    // Test with different transaction sizes
    let test_cases = vec![
        ("Tiny (100 bytes)", 100),
        ("Small (1 KB)", 1024),
        ("Medium (10 KB)", 10_240),
        ("Large (100 KB)", 102_400),
    ];

    for (name, size) in test_cases {
        println!("📦 Transaction Size: {}", name);
        println!("─────────────────────────────────────────────────────────");
        
        // Create test transactions
        let midnight_tx = create_mock_tx(MIDNIGHT_PRIVACY_DISCRIMINANT, size);
        let other_tx = create_mock_tx(0, size);
        
        // Verify correctness first
        assert!(option1_raw_byte_check(&midnight_tx));
        assert!(option2_deserialize_discriminant(&midnight_tx));
        assert!(option3_full_deserialize(&midnight_tx));
        
        assert!(!option1_raw_byte_check(&other_tx));
        assert!(!option2_deserialize_discriminant(&other_tx));
        assert!(!option3_full_deserialize(&other_tx));
        
        // Warmup
        for _ in 0..WARMUP_ITERATIONS {
            std::hint::black_box(option1_raw_byte_check(&midnight_tx));
            std::hint::black_box(option2_deserialize_discriminant(&midnight_tx));
            std::hint::black_box(option3_full_deserialize(&midnight_tx));
        }
        
        // Benchmark Option 1
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(option1_raw_byte_check(&midnight_tx));
        }
        let option1_time = start.elapsed();
        let option1_per_call = option1_time.as_nanos() / ITERATIONS as u128;
        
        // Benchmark Option 2
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(option2_deserialize_discriminant(&midnight_tx));
        }
        let option2_time = start.elapsed();
        let option2_per_call = option2_time.as_nanos() / ITERATIONS as u128;
        
        // Benchmark Option 3
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(option3_full_deserialize(&midnight_tx));
        }
        let option3_time = start.elapsed();
        let option3_per_call = option3_time.as_nanos() / ITERATIONS as u128;
        
        // Calculate throughput
        let option1_throughput = 1_000_000_000 / option1_per_call.max(1);
        let option2_throughput = 1_000_000_000 / option2_per_call.max(1);
        let option3_throughput = 1_000_000_000 / option3_per_call.max(1);
        
        // Calculate overhead
        let overhead_2_vs_1 = if option1_per_call > 0 {
            ((option2_per_call as f64 / option1_per_call as f64) - 1.0) * 100.0
        } else {
            0.0
        };
        
        let overhead_3_vs_1 = if option1_per_call > 0 {
            ((option3_per_call as f64 / option1_per_call as f64) - 1.0) * 100.0
        } else {
            0.0
        };
        
        println!("  Option 1 (Raw byte check):");
        println!("    ⏱  Time per check:  {}", format_duration(option1_per_call));
        println!("    🚀 Throughput:      {} million checks/sec", option1_throughput / 1_000_000);
        println!();
        
        println!("  Option 2 (Deserialize discriminant):");
        println!("    ⏱  Time per check:  {}", format_duration(option2_per_call));
        println!("    🚀 Throughput:      {} million checks/sec", option2_throughput / 1_000_000);
        println!("    📊 Overhead:        {:.1}% slower than Option 1", overhead_2_vs_1);
        println!();
        
        println!("  Option 3 (Full deserialize):");
        println!("    ⏱  Time per check:  {}", format_duration(option3_per_call));
        println!("    🚀 Throughput:      {} million checks/sec", option3_throughput / 1_000_000);
        println!("    📊 Overhead:        {:.1}% slower than Option 1", overhead_3_vs_1);
        println!();
    }
    
    println!("════════════════════════════════════════════════════════════");
    println!("📝 NOTE: Run with --release for realistic performance!");
    println!("   cargo test --test discriminant_bench --release -- --nocapture");
    println!("════════════════════════════════════════════════════════════\n");
}

#[test]
fn bench_cache_effects() {
    println!("\n════════════════════════════════════════════════════════════");
    println!("   CACHE LOCALITY TEST");
    println!("════════════════════════════════════════════════════════════\n");
    
    const ITERATIONS: usize = 100_000;
    
    // Test with varying numbers of transactions to show cache effects
    let batch_sizes = vec![1, 10, 100, 1000];
    
    for batch_size in batch_sizes {
        println!("📦 Batch Size: {} transactions", batch_size);
        
        // Create a batch of transactions
        let txs: Vec<_> = (0..batch_size)
            .map(|i| {
                let discriminant = if i % 2 == 0 { MIDNIGHT_PRIVACY_DISCRIMINANT } else { 0 };
                create_mock_tx(discriminant, 1024)
            })
            .collect();
        
        // Option 2: Sequential checking
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            for tx in &txs {
                std::hint::black_box(option2_deserialize_discriminant(tx));
            }
        }
        let time = start.elapsed();
        let per_check = time.as_nanos() / (ITERATIONS as u128 * batch_size as u128);
        
        println!("  ⏱  Time per check: {}", format_duration(per_check));
        println!();
    }
    
    println!("════════════════════════════════════════════════════════════\n");
}

#[test]
fn bench_real_world_context() {
    println!("\n════════════════════════════════════════════════════════════");
    println!("   REAL-WORLD CONTEXT COMPARISON");
    println!("════════════════════════════════════════════════════════════\n");
    
    const ITERATIONS: usize = 10_000;
    
    let tx = create_mock_tx(MIDNIGHT_PRIVACY_DISCRIMINANT, 1024);
    
    // Discriminant check (Option 2)
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        std::hint::black_box(option2_deserialize_discriminant(&tx));
    }
    let discriminant_time = start.elapsed().as_nanos() / ITERATIONS as u128;
    
    // Simulate full deserialization cost
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let mut data = &tx.data[..];
        std::hint::black_box(MockRuntimeCall::deserialize(&mut data).ok());
    }
    let full_deserialize_time = start.elapsed().as_nanos() / ITERATIONS as u128;
    
    // Simulate hash computation (as proxy for signature verification)
    use sha2::{Sha256, Digest};
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let mut hasher = Sha256::new();
        hasher.update(&tx.data);
        std::hint::black_box(hasher.finalize());
    }
    let hash_time = start.elapsed().as_nanos() / ITERATIONS as u128;
    
    println!("  Discriminant check (Option 2):  {}", format_duration(discriminant_time));
    println!("  Full deserialization:           {}", format_duration(full_deserialize_time));
    println!("  SHA-256 hash:                   {}", format_duration(hash_time));
    println!();
    println!("  Discriminant check is {:.1}x faster than full deserialization", 
             full_deserialize_time as f64 / discriminant_time as f64);
    println!("  Discriminant check is {:.1}x faster than SHA-256", 
             hash_time as f64 / discriminant_time as f64);
    println!();
    println!("════════════════════════════════════════════════════════════\n");
}

