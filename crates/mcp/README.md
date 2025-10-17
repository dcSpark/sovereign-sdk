# MCP Server

Model Context Protocol (MCP) server for interacting with the Sovereign SDK L2 rollup.

## Overview

This server exposes L2 wallet operations through the MCP protocol, enabling AI assistants and other clients to interact with the rollup network. It handles wallet management, transaction submission, and ZK proof generation using the Ligero adapter.

## Running the Server

### Prerequisites

- Rust toolchain
- A running Sovereign SDK L2 rollup node
- Ligero prover binaries and shader files

### Configuration

Create a `.env` file based on `.env.example`:

```bash
cp .env.example .env
```

Configure the following variables:

- `MCP_SERVER_BIND_ADDRESS` - Server bind address (default: `127.0.0.1:3000`)
- `WALLET_PRIVATE_KEY` - Hex-encoded private key for wallet operations
- `ROLLUP_RPC_URL` - L2 rollup RPC endpoint
- `LIGERO_PROGRAM_PATH` - Path to Ligero program WASM
- `LIGERO_PROVER_BINARY_PATH` - Path to Ligero prover binary
- `LIGERO_SHADER_PATH` - Path to shader directory

### Start the Server

```bash
cargo run -p mcp
```

The server will start on the configured address. The MCP endpoint will be available at:

```
http://<bind_address>/mcp
```

### Logging

Set the `RUST_LOG` environment variable to control log verbosity:

```bash
RUST_LOG=debug cargo run -p mcp
```

## Testing

Run the test suite:

```bash
cargo test -p mcp
```
