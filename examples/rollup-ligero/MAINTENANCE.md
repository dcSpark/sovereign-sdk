# Rollup Ligero Maintenance Guide

Last updated: February 24, 2026

## Purpose

This document explains how `examples/rollup-ligero` is deployed and maintained in the current testnet setup, from an SRE perspective.

## Scope

This guide covers:
- Runtime topology and service ownership.
- Health checks, restart procedures, and incident response.
- Worker fleet operations.
- Replica node role.
- Database and secret-management operational notes.

This guide does not cover:
- Local developer setup.
- Kubernetes operations (planned future state).

## Runtime Topology

### Control Plane

- Host: `midnight-l2-testnet` (AWS EC2, Linux).
- `systemd` manages only one unit: `rollup-ligero-service-controller.service`.
- The controller is the process supervisor for the rollup stack and spawns/stops child services via scripts under `examples/rollup-ligero`.

### Data Plane

- Main stack runs on `midnight-l2-testnet`.
- Workers are remote and run on five macOS VMs (`macos-ligero-prover-*`).
- Replica node runs on `rollup-replica-indexer` in read-only mode for read traffic/indexer support.

### Host Inventory

| Role | Host / Instance | Notes |
|---|---|---|
| Main orchestrator + app stack | `midnight-l2-testnet` (`i-06b67fc05fb38650d`) | Runs controller and local services except worker (remote mode). |
| Replica node | `rollup-replica-indexer` (`i-0676ab5298115ab9c`) | Read-only rollup for indexer/block-explorer traffic. |
| Worker 1 | `macos-ligero-prover-m4` (`i-0a2540891b2645878`, `13.216.246.65`) | Remote verifier worker. |
| Worker 2 | `macos-ligero-prover-*` (`i-0c1d809e5285ad26a`, `98.83.159.116`) | Remote verifier worker. |
| Worker 3 | `macos-ligero-prover-*` (`i-010d600d6e4a2ba0b`, `107.23.225.27`) | Remote verifier worker. |
| Worker 4 | `macos-ligero-prover-*` (`i-0e3825c91e8444580`, `54.211.232.66`) | Remote verifier worker. |
| Worker 5 | `macos-ligero-prover-*` (`i-07ca1b7175ded0841`, `34.228.245.250`) | Remote verifier worker. |

### Access and Permissions

- Each server in the inventory is accessible over SSH (subject to security group/network policy).
- For server onboarding, share your public SSH key with the contacts listed in `Contacts and Escalation`.
- For AWS console access, request IAM access from the same contacts.

## Service Model

### systemd Layer

- Unit: `rollup-ligero-service-controller.service`
- Managed by: `systemctl`
- Everything else is managed by controller endpoints (`/start`, `/stop`, `/restart`, per-service variants).

### Controller-Managed Services

Default managed service IDs:
- `oracle` (conditional by env)
- `rollup`
- `worker` (configured as remote in production)
- `fvk` (conditional by env)
- `indexer`
- `proof-pool`
- `mcp`
- `metrics`

Current production behavior:
- `worker` is configured as remote.
- `SERVICE_WORKER_REMOTE=1`
- `SERVICE_WORKER_URL=http://localhost/worker`

This means controller health checks the worker via nginx route and does not attempt local start/stop for worker.

## Worker Fleet Operations

Each macOS worker runs a tmux session named `worker` with:

```bash
NODE_RPC_URL=http://172.33.91.192:12346 \
POOL_FVK_PK=<must-match-controller-pool-fvk-pk> \
BIND_ADDR=0.0.0.0:8080 \
./run_verifier_service.sh
```

Traffic routing:
- Main nginx routes `/worker` and `/ligero` load-balance to the macOS worker backend.
- Nginx upstream backends currently configured as:
  - `172.33.109.145:8080` (weight 10)
  - `172.33.65.65:8080` (weight 20)
  - `172.33.71.114:8080` (weight 20)
  - `172.33.79.77:8080` (weight 20)
  - `172.33.116.4:8080` (weight 20)

Operational expectation:
- Restart workers one-by-one to keep capacity available.
- Keep `POOL_FVK_PK` consistent across all worker nodes and controller unit env.
- If rollup RPC private IP changes, update `NODE_RPC_URL` on all workers.

## Production systemd Unit (Source of Truth)

Canonical unit path:
- `/etc/systemd/system/rollup-ligero-service-controller.service`

Important runtime settings observed in production:
- `User=ubuntu`
- `WorkingDirectory=/home/ubuntu/sovereign-sdk/examples/rollup-ligero`
- `SERVICE_CONTROLLER_AUTO_START=1`
- `SERVICE_WORKER_REMOTE=1`
- `SERVICE_WORKER_URL=http://localhost/worker`
- `VERIFIER_URL=http://localhost/worker`
- `LIGERO_PROOF_SERVICE_URL=http://localhost/ligero`
- `MAX_CONCURRENT=100`
- `MAX_PROOFS=100`
- `MAX_CONCURRENT_PROOFS=20`
- `PROOF_GENERATION_INTERVAL_MS=1000`
- `DEFER_SEQUENCER_SUBMISSION=1`
- `ROLLUP_GENESIS_CONFIG_DIR=/home/ubuntu/sovereign-sdk/examples/rollup-ligero/genesis-production`

Database URLs configured in the unit:
- `DA_CONNECTION_STRING` -> RDS `da` database
- `INDEX_DB` -> RDS `indexer` database
- `MIDNIGHT_FVK_SERVICE_DB` -> RDS `fvk` database
- `MCP_SESSION_DB_URL` -> RDS `mcp_sessions` database

Security note:
- Secret values are intentionally omitted from this document.
- Store and rotate secrets through secure channels only (currently 1Password, planned AWS Secrets Manager).

## Endpoints and Health Checks

### Local Controller Health (main host)

```bash
curl -fsS http://127.0.0.1:9090/health
curl -fsS http://127.0.0.1:9090/services
curl -fsS http://127.0.0.1:9090/stats
```

### Public Routed Endpoints (nginx)

- `/rollup` -> rollup RPC on the replica node (`rollup-replica-indexer`, currently proxied to `172.33.111.65:12346`)
- `/oracle` -> oracle (`8090`)
- `/mcp` -> MCP (`3000`)
- `/authority` -> authority API (`3000/authority`)
- `/metrics` -> metrics API (`13200`)
- `/controller` -> service controller (`9090`), protected by basic auth via `.htpasswd`
- `/proof-pool` -> proof-pool (`11235`), requires `auth_token` query parameter (from systemd service `AUTH_TOKEN`)
- `/explorer` -> redirect to block explorer

### Local/Private Routed Endpoints (nginx on `localhost`)

- `/worker` -> load-balanced remote worker fleet
- `/ligero` -> load-balanced remote worker fleet
- `/rollup` -> local rollup (`12346`)
- `/oracle` -> local oracle (`8090`)
- `/indexer` -> local indexer (`13100`)
- `/fvk` -> local FVK (`8088`)
- `/metrics` -> local metrics (`13200`)
- `/mcp` -> local MCP (`3000`)
- `/authority` -> local authority API
- `/proof-pool` -> local proof-pool (`11235`)

## Daily Operations

### Controller Service Management

```bash
sudo systemctl status rollup-ligero-service-controller
sudo systemctl restart rollup-ligero-service-controller
journalctl -u rollup-ligero-service-controller -f
```

### Child Service Management via Controller

```bash
curl -X POST http://127.0.0.1:9090/start
curl -X POST http://127.0.0.1:9090/stop
curl -X POST http://127.0.0.1:9090/restart
curl -X POST http://127.0.0.1:9090/restart/indexer
curl -X POST http://127.0.0.1:9090/restart/proof-pool
```

### Destructive Maintenance Actions

Run only when all managed services are stopped.

```bash
curl -X POST http://127.0.0.1:9090/clean
curl -X POST http://127.0.0.1:9090/clean-database
curl -X POST http://127.0.0.1:9090/reset-tee
```

All controller actions above are also available from the dashboard at `midnight-l2-testnet.shinkai.com` (basic-auth protected).

### Worker Spot Checks

From each macOS host:

```bash
tmux ls
tmux attach -t worker
curl -fsS http://127.0.0.1:8080/health
```

From main host (through remote URL used by controller):

```bash
curl -fsS http://localhost/worker/health
```

## Incident Runbooks

### Rollup Unhealthy

1. Check controller health summary: `GET /health`.
2. Check rollup logs from controller stream (`/controller/logs`) or system journal.
3. Restart only rollup first: `POST /restart/rollup`.
4. If still failing, restart dependent services in order: `worker`, `indexer`, `proof-pool`, `mcp`, `metrics`.

### Worker Degradation

1. Validate `http://localhost/worker/health`.
2. Validate each worker tmux session and local `:8080/health`.
3. Restart only failed worker hosts one-by-one.
4. Confirm nginx backend returns healthy status before moving to next host.

### Database Connectivity Errors

1. Validate RDS availability in AWS console.
2. Validate DB connectivity from main host:
   - `psql "$DA_CONNECTION_STRING" -c "select now();"`
3. Check pool exhaustion risk from configured `*_MAX_CONNECTIONS` values.
4. Restart impacted services after DB recovery.

### Replica Node Issues (`rollup-replica-indexer`)

1. Confirm replica process health and API availability.
2. Confirm replica is still in read-only mode (`rollup_config_replica.toml`).
3. If lagging or stalled, restart replica node and verify it re-syncs from DA DB.
4. Keep primary node reserved for write/sequencing traffic.

## Database Notes (Current State)

RDS instance:
- Identifier: `testnet-db`
- Engine: PostgreSQL `17.6`
- Class: `db.m8g.large` (2 vCPU, 8 GiB RAM)
- Deployment: single AZ (`us-east-1d`), not Multi-AZ
- Storage: `gp3`, 100 GiB, autoscaling to 1000 GiB
- Provisioned IOPS: 3000
- Throughput: 125 MiB/s
- Encryption: enabled

Operational risk:
- Single-AZ DB is a current availability risk and should be considered in incident response and maintenance windows.

## Monitoring and Alerting

Current:
- Status page at `midnight-l2-testnet.shinkai.com` pings health endpoints.

Planned:
- Migrate to Kubernetes and adopt Prometheus/Grafana for metrics and alerting.
- Dockerize all services.
- Formalize infrastructure as code with Terraform.
- Apply production SRE/DevOps best practices for reliability and operability (HA design, SLO-driven alerting, runbooks, backup/restore drills, security hardening, controlled rollouts, and disaster recovery procedures).
- Replace macOS workers with an autoscaling worker solution alongside the Ligero -> Nightstream migration.

Pending definition:
- SLOs and paging thresholds are not finalized yet.

## Secrets and Configuration

Current:
- Runtime secrets are configured in systemd service environment.
- Backed up in 1Password vault.

Planned:
- Migrate secret storage to AWS Secrets Manager.

## Contacts and Escalation

Primary contacts for production issues:
- Alfredo Gallardo (`alfredo@dcspark.io`)
- Guillermo Valin (`guillermo@dcspark.io`)
- Nicolás Arqueros (`nicolas@dcspark.io`)

Access requests:
- Contact the same team to add your SSH public key to servers or grant IAM access to the AWS console.

## Open Items

- Define SLOs and alert thresholds for availability, latency, error rates, and saturation.
- Confirm canonical hostnames for the four `macos-ligero-prover-m1*` instances (names are currently tracked by instance ID/IP).
- Define migration runbook from current VM-based stack to Kubernetes.
