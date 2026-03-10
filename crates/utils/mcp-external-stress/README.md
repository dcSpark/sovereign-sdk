# mcp-external-stress

Stress-test a deployed `mcp-external` instance by opening **many MCP sessions** (one per wallet),
creating/restoring a wallet per session, and repeatedly sending a privacy transfer **to itself**.

Only the privacy address (`privpool1...`) is logged at `INFO` level.

## Prerequisites (server-side)

This tool assumes the target `mcp-external` deployment:

- Is reachable at `http(s)://HOST:PORT/mcp`
- Has working rollup/indexer/verifier/prover wiring
- Has `POOL_FVK_PK` + `MIDNIGHT_FVK_SERVICE_URL` configured (required by the `send` tool)
- Has **either**:
  - `ADMIN_WALLET_PRIVATE_KEY` + `AUTO_FUND_DEPOSIT_AMOUNT` configured, so each session’s `createWallet`
    auto-funds and deposits, **or**
  - You extend this client to call `restoreWallet` with pre-funded keys.

## Run

Convenience wrapper for the public testnet deployment:

```bash
sh scripts/mcp-external-stress.sh --wallets 2 --txs 1
```

Run the same workload continuously in a loop (until Ctrl-C):

```bash
sh scripts/mcp-external-stress.sh --wallets 2 --txs 1 --continuous
```

Probe the deployment (no wallet creation, no transactions):

```bash
RUST_LOG=info cargo run -p mcp-external-stress -- \
  --probe \
  --mcp-endpoint https://midnight-l2-testnet.shinkai.com/mcp/mcp
```

```bash
cargo run -p mcp-external-stress -- \
  --mcp-endpoint http://127.0.0.1:3000/mcp \
  --wallets 50 \
  --send-amount 1 \
  --duration-secs 300 \
  --per-wallet-delay-ms 1000
```

Tx ids are logged at `INFO` level (use `RUST_LOG=warn` to avoid per-tx logs during large runs).
Each `sent` log line includes `elapsed_ms` (end-to-end time for the MCP `send` tool call).

The periodic progress line is disabled by default; enable it with `--report-interval-secs 1` (or higher).

Optional confirmation polling (extra load):

```bash
cargo run -p mcp-external-stress -- \
  --mcp-endpoint http://127.0.0.1:3000/mcp \
  --wallets 10 \
  --send-amount 1 \
  --duration-secs 120 \
  --confirm
```
