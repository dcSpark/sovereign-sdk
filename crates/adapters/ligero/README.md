# Ligero zkVM Adapter for Sovereign SDK

This adapter allows [Ligero](https://github.com/ligeroinc/ligero-prover) to be used as a zkVM for the Sovereign SDK, alongside RISC0 and SP1.

## Overview

Ligero is a zkVM that uses WebGPU for high-performance proof generation. Unlike RISC0 and SP1 which compile Rust to custom instruction sets, Ligero compiles C/C++ programs to WebAssembly and proves their execution.

### Key Features

- **WebGPU Acceleration**: Hardware-accelerated proof generation
- **C/C++ Guest Programs**: Write guest programs in C/C++ (compiled to WASM)
- **Fast Verification**: Efficient on-chain verification
- **Flexible Prover Modes**: Support for skip/execute/prove modes via `SOV_PROVER_MODE`

## Prerequisites

### 1. Emscripten SDK

Ligero guest programs are written in C/C++ and compiled to WebAssembly using Emscripten.

```bash
# Install Emscripten
git clone https://github.com/emscripten-core/emsdk.git
cd emsdk
./emsdk install latest
./emsdk activate latest
source emsdk_env.sh
```

### 2. Ligero SDK

The Ligero SDK is required to compile guest programs.

```bash
# Clone Ligero repository (assumed to be a sibling to sovereign-sdk)
cd /path/to
git clone https://github.com/ligeroinc/ligero-prover ligero-vm
cd ligero-vm/ligero-prover/sdk

# Build the SDK
mkdir -p build && cd build
emcmake cmake ..
emmake make -j
```

This will create `libligetron.a` which is required for guest program compilation.

## Usage

### Environment Variables

#### `SKIP_GUEST_BUILD`

Control whether guest programs are compiled:

- `SKIP_GUEST_BUILD=1` or `SKIP_GUEST_BUILD=true`: Skip all guest builds
- `SKIP_GUEST_BUILD=ligero`: Skip only Ligero guest builds
- `SKIP_GUEST_BUILD=0` or unset: Build Ligero guest programs

**Example:**
```bash
export SKIP_GUEST_BUILD=ligero
cargo build --release
```

#### `SOV_PROVER_MODE`

Control proving behavior for the rollup:

- `SOV_PROVER_MODE=skip`: Skip proof generation entirely
- `SOV_PROVER_MODE=execute`: Execute without generating proofs (simulation)
- `SOV_PROVER_MODE=prove`: Generate full proofs using `webgpu_prover`

**Example:**
```bash
export SOV_PROVER_MODE=prove
./target/release/sov-demo-rollup
```

#### `LIGERO_SDK_PATH`

Override the default Ligero SDK path:

```bash
export LIGERO_SDK_PATH=/custom/path/to/ligero-vm/ligero-prover/sdk
cargo build
```

### Writing Guest Programs

Guest programs are written in C++ and must follow the Ligero SDK API:

```cpp
#include <ligetron/api.h>

int main(int argc, char *argv[]) {
    // Read arguments (passed as raw bytes in argv)
    int value = *reinterpret_cast<const int*>(argv[1]);
    
    // Add constraints using assert_one()
    assert_one(value >= 0);
    assert_one(value <= 100);
    
    return 0;
}
```

#### Guest Programs

Sovereign expects guest program artifacts to come from the Ligero-owned repo checkout.

- **Programs dir**: `<ligero-prover>/utils/circuits/bins/`
- **Programs**:
  - `note_spend_guest.wasm`
  - `value_validator_rust.wasm`

You can point directly at a program with `LIGERO_PROGRAM_PATH` (either a circuit name like `note_spend_guest` or a full path to a `.wasm`).

### Using Ligero in a Rollup

To use Ligero as the zkVM for your rollup:

1. **Add Ligero adapter to dependencies:**
```toml
[dependencies]
sov-ligero-adapter = { workspace = true, features = ["native"] }
```

2. **Configure your rollup to use Ligero:**
```rust
use sov_ligero_adapter::Ligero;

type YourZkvm = Ligero;
```

3. **Build with guest programs:**
```bash
# Ensure Emscripten is available
source /path/to/emsdk/emsdk_env.sh

# Build the rollup
cargo build --release
```

4. **Run with desired prover mode:**
```bash
# Development: execute without proofs
export SOV_PROVER_MODE=execute
./target/release/your-rollup

# Production: generate full proofs
export SOV_PROVER_MODE=prove
./target/release/your-rollup
```

## Comparison with RISC0 and SP1

| Feature | Ligero | RISC0 | SP1 |
|---------|--------|-------|-----|
| **Guest Language** | C/C++ (WASM) | Rust (RISC-V) | Rust (RISC-V) |
| **Toolchain** | Emscripten | `rzup` | `sp1up` |
| **Acceleration** | WebGPU | CUDA (optional) | CUDA (optional) |
| **Proof Size** | ~3-4 MB | Varies | Varies |
| **Proving Time** | Fast (GPU) | Medium-Fast | Fast |

## Troubleshooting

### Guest Build Failures

If guest programs fail to build:

1. **Check Emscripten installation:**
```bash
emcc --version
```

2. **Verify Ligero SDK:**
```bash
ls -lh $LIGERO_SDK_PATH/build/libligetron.a
```

3. **Manual build:**

Build the guest programs in the Ligero repo (or set `LIGERO_PROGRAM_PATH` to point at the program you want to use).

### Proof Generation Errors

If proof generation fails:

1. **Check binaries exist:**
```bash
ls -lh crates/adapters/ligero/bins/*/bin/webgpu_{prover,verifier}
```

2. **Verify shader directory:**
```bash
ls crates/adapters/ligero/bins/shader/
```

3. **Enable debug logging:**
```bash
export RUST_LOG=sov_ligero_adapter=debug
```

### Performance Optimization

For optimal proving performance:

- **Use Release Build:** `cargo build --release`
- **Enable GPU:** Ensure WebGPU is available (Chrome/Edge with GPU)
- **Adjust Packing:** Modify `packing` parameter in guest config (default: 8192)

## Examples

### Module-Level Proof (value-setter-zk)

This repo no longer ships a `generate_value_proof` example. Use the `value-setter-zk` module tooling
or your own host wrapper that calls `<Ligero as Zkvm>::Host::from_args(...)` and submits the resulting
`LigeroProofPackage` to the verifier service.

### Rollup-Level Proof

When configured as the rollup's zkVM, Ligero will automatically prove state transitions:

```bash
export SOV_PROVER_MODE=prove
cd examples/demo-rollup
cargo run --release
```

## License

See the main Sovereign SDK LICENSE file.

## Resources

- [Ligero Repository](https://github.com/ligeroinc/ligero-prover)
- [Emscripten Documentation](https://emscripten.org/docs/)
- [WebGPU Specification](https://www.w3.org/TR/webgpu/)
- [Sovereign SDK Documentation](https://github.com/Sovereign-Labs/sovereign-sdk)
