# syntax=docker/dockerfile:1.5

################################################################################
# SIMPLIFIED DOCKERFILE USING PRE-COMPILED LIGERO BINARIES
# 
# This Dockerfile uses pre-compiled Ligero binaries (ligero-linux-amd64.tar.gz)
# to drastically simplify the build process and eliminate the need to compile
# Dawn, WABT, and Ligero from source.
#
# This reduces build time from ~20+ minutes to just a few minutes!
################################################################################

# Stage 1: Build Rust application
FROM ubuntu:24.04 AS rust-builder

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        clang \
        cmake \
        curl \
        git \
        libclang-dev \
        libssl-dev \
        pkg-config \
        protobuf-compiler \
        ca-certificates

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.88.0
ENV PATH="/root/.cargo/bin:${PATH}"

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

RUN cargo build --release -p sov-proof-verifier-service

# ---------------------------------------------------------------------
# Fetch Ligero runtime assets (binaries + shader) from ligero-prover
#
# Sovereign no longer vendors Ligero binaries/shaders, but this image
# still needs the verifier/prover binaries and shaders at runtime.
# ---------------------------------------------------------------------
ARG LIGERO_PROVER_REV=1b9ba6fd83af9ef2a2d477c50f6faded0fb03876
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) LIGERO_PLATFORM="linux-amd64" ;; \
      arm64) LIGERO_PLATFORM="linux-arm64" ;; \
      *) echo "Unsupported TARGETARCH=${TARGETARCH}"; exit 1 ;; \
    esac; \
    mkdir -p /workspace/ligero/bin /workspace/ligero/lib /workspace/ligero/shader; \
    tmpdir="$(mktemp -d)"; \
    curl -fsSL "https://codeload.github.com/dcSpark/ligero-prover/tar.gz/${LIGERO_PROVER_REV}" -o "${tmpdir}/ligero-prover.tar.gz"; \
    tar -xzf "${tmpdir}/ligero-prover.tar.gz" -C "${tmpdir}"; \
    root="$(find "${tmpdir}" -maxdepth 1 -type d -name 'ligero-prover-*' | head -n 1)"; \
    test -n "${root}"; \
    cp -a "${root}/utils/portable-binaries/${LIGERO_PLATFORM}/bin/." /workspace/ligero/bin/; \
    cp -a "${root}/utils/portable-binaries/${LIGERO_PLATFORM}/lib/." /workspace/ligero/lib/; \
    cp -a "${root}/shader/." /workspace/ligero/shader/; \
    rm -rf "${tmpdir}"

# Stage 2: Runtime
FROM ubuntu:24.04 AS runtime

# Install only minimal runtime dependencies for the pre-compiled binaries
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl3 \
        libx11-6 \
        libxrandr2 \
        libxinerama1 \
        libxcursor1 \
        libxi6 \
        libvulkan1 \
        vulkan-tools \
        mesa-vulkan-drivers

WORKDIR /app

# Copy Rust binary
COPY --from=rust-builder /workspace/target/release/proof-verifier /usr/local/bin/proof-verifier

# Copy the pre-compiled Ligero binaries + shader (downloaded from ligero-prover)
COPY --from=rust-builder /workspace/ligero/ /app/ligero/
RUN echo "=== Ligero directory structure ===" && \
    ls -la /app/ligero/ && \
    echo "=== Bin directory ===" && \
    ls -la /app/ligero/bin/ && \
    echo "=== Making binaries executable ===" && \
    chmod +x /app/ligero/bin/*

# Copy shared libraries to system location for runtime access
RUN cp -r /app/ligero/lib/* /usr/local/lib/ && \
    ldconfig

# Copy value_validator.wasm from the repository
COPY crates/adapters/ligero/guest/bins/programs/value_validator.wasm /app/ligero/programs/value_validator.wasm

# Copy keys and setup directories
COPY examples/test-data/keys/token_deployer_private_key.json keys/token_deployer_private_key.json
RUN mkdir -p /app/demo_data
COPY crates/utils/sov-proof-verifier-service/docker-entrypoint.sh /app/docker-entrypoint.sh
RUN chmod +x /app/docker-entrypoint.sh

# Update library path for pre-compiled libraries
ENV LD_LIBRARY_PATH="/usr/local/lib:${LD_LIBRARY_PATH}"

# Environment variables pointing to the extracted Ligero binaries
ENV \
    BIND_ADDRESS=0.0.0.0:8080 \
    NODE_RPC_URL=http://localhost:12346 \
    SIGNING_KEY_PATH=/app/keys/token_deployer_private_key.json \
    METHOD_ID=0x698c44527e4fa3f934471015da3caa61da1f4e167107dbba9df71a6545396fb3 \
    MAX_CONCURRENT_VERIFICATIONS=5 \
    LOG_LEVEL=debug \
    LIGERO_PROGRAM_PATH=/app/ligero/programs/value_validator.wasm \
    LIGERO_SHADER_PATH=/app/ligero/shader \
    LIGERO_VERIFIER_BIN=/app/ligero/bin/webgpu_verifier \
    LIGERO_PROVER_PATH=/app/ligero/bin/webgpu_prover \
    LIGERO_PACKING=8192 \
    RUST_LOG=debug

EXPOSE 8080

ENTRYPOINT ["/app/docker-entrypoint.sh"]
