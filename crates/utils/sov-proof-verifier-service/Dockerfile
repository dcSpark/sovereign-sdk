# syntax=docker/dockerfile:1.5

################################################################################
# DOCKERFILE FOR SOV-PROOF-VERIFIER-SERVICE (Nightstream)
#
# Pure-Rust build — no external binaries or GPU dependencies required.
# Supports both a normal container runtime image and a Lambda-compatible image
# target via the AWS Lambda Web Adapter.
################################################################################

# Stage 1: Build Rust application
FROM ubuntu:24.04 AS builder

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        clang \
        cmake \
        curl \
        git \
        openssh-client \
        libclang-dev \
        libssl-dev \
        pkg-config \
        protobuf-compiler \
        ca-certificates

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.88.0
ENV PATH="/root/.cargo/bin:${PATH}"
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true

WORKDIR /workspace

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo .cargo
COPY crates crates
COPY examples examples
COPY scripts scripts
COPY constants.toml constants.toml
COPY constants.testing.toml constants.testing.toml

ARG GIT_SHA=unknown
ENV GIT_COMMIT_HASH=${GIT_SHA}
ENV SKIP_GUEST_BUILD=ligero \
    SOV_PROVER_MODE=execute

# Clone the Nightstream repo — sovereign-ligero has path dependencies that
# resolve to /Nightstream/crates/{neo-fold,neo-math,neo-ajtai,neo-memory,neo-ccs,nightstream-sdk}.
ARG NIGHTSTREAM_REF=5b98cb6471fccea81b86a335c54d6f3a73f5499d
RUN git clone https://github.com/LFDT-Nightstream/Nightstream.git /Nightstream \
    && git -C /Nightstream checkout "${NIGHTSTREAM_REF}"

RUN cargo build --release -p sov-proof-verifier-service

# Stage 2: Minimal runtime image
FROM ubuntu:24.04 AS runtime

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the compiled binary
COPY --from=builder /workspace/target/release/proof-verifier /usr/local/bin/proof-verifier

# Copy keys, config, and entrypoint
COPY examples/test-data/keys/token_deployer_private_key.json keys/token_deployer_private_key.json
COPY examples/rollup-nightstream/rollup_config.toml /app/rollup_config.toml
RUN mkdir -p /app/demo_data
COPY crates/utils/sov-proof-verifier-service/docker-entrypoint.sh /app/docker-entrypoint.sh
RUN chmod +x /app/docker-entrypoint.sh

# ── Required configuration ────────────────────────────────────────────────────
# Address the HTTP server listens on.
ENV BIND_ADDRESS=0.0.0.0:8080
# URL of the rollup node RPC endpoint.
ENV NODE_RPC_URL=http://127.0.0.1:12346
# Path to the Ed25519 signing key (JSON) used for non-ZK transactions.
ENV SIGNING_KEY_PATH=/app/keys/token_deployer_private_key.json
# Path to the rollup configuration TOML. Used to derive the DA DB connection
# string and incoming-worker-tx persistence settings.
ENV ROLLUP_CONFIG_PATH=/app/rollup_config.toml
# Chain ID for transaction authentication.
ENV CHAIN_ID=4321

# ── Optional configuration (override at runtime) ─────────────────────────────
# Method ID (hex, 32 bytes) for value-setter proof verification.
# If unset, auto-computed from the value_validator ROM at startup.
#ENV METHOD_ID=
# Method ID (hex, 32 bytes) for midnight/Nightstream proof verification.
# If unset, auto-computed from the Nightstream ROM at startup.
#ENV MIDNIGHT_METHOD_ID=
# Maximum number of concurrent proof verifications.
# If unset, defaults to the number of available CPUs.
#ENV MAX_CONCURRENT=
# Connection string for the worker_txs database.
# If empty, derived from ROLLUP_CONFIG_PATH's [da].connection_string.
# Example: "postgres://user:pass@host:5432/worker_txs"
ENV DA_DB=
# URL of the remote prover service. If unset, proving is done locally.
#ENV PROVER_SERVICE_URL=
# When true, queue worker-verified txs instead of immediately submitting
# them to the sequencer. Flush via POST /midnight-privacy/flush.
ENV DEFER_SUBMISSION=false

# ── Logging ───────────────────────────────────────────────────────────────────
ENV LOG_LEVEL=info
ENV RUST_LOG=info

EXPOSE 8080

ENTRYPOINT ["/app/docker-entrypoint.sh"]

# Stage 3: Lambda-compatible runtime image
FROM runtime AS lambda-runtime

COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:1.0.0-rc1 /lambda-adapter /opt/extensions/lambda-adapter

ENV AWS_LWA_PORT=8080
ENV AWS_LWA_READINESS_CHECK_PATH=/health
