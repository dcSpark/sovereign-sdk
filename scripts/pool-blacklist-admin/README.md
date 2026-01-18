# Pool Blacklist Admin Scripts

Small helper script for managing the `midnight-privacy` pool deny-map and pool-admin set:

- add a pool admin (`AddPoolAdmin`, module-admin only)
- remove a pool admin (`RemovePoolAdmin`, module-admin only)
- freeze a privacy address (`FreezeAddress`, pool-admin only)
- unfreeze a privacy address (`UnfreezeAddress`, pool-admin only)

This uses `sov-cli` under the hood to sign and submit transactions to the rollup REST API.

## Setup

```bash
cd scripts/pool-blacklist-admin
cp .env.example .env
# edit .env as needed
```

## Examples

```bash
# List current pool admins
./pool_blacklist_admin.sh list-admins

# List frozen (blacklisted) privacy addresses
./pool_blacklist_admin.sh list-frozen

# Add a pool admin (must be signed by midnight-privacy module admin)
./pool_blacklist_admin.sh add-admin sov1...

# Remove a pool admin (module admin only)
./pool_blacklist_admin.sh remove-admin sov1...

# Freeze a privacy address (pool admin only)
./pool_blacklist_admin.sh freeze privpool1...

# Unfreeze a privacy address (pool admin only)
./pool_blacklist_admin.sh unfreeze privpool1...
```

## Notes

- Default demo genesis uses module admin `sov1lzkj...`; the matching private key is `75fbf8...` (see `examples/test-data/keys/token_deployer_private_key.json`).
- If `CHAIN_ID` is not set, the script tries to fetch it from `${ROLLUP_RPC_URL}/rollup/schema` and falls back to `4321`.
- By default the script creates a temporary `SOV_WALLET_DIR` and deletes it on exit, so your private key isn’t persisted on disk.
