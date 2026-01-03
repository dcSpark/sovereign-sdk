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
        openssh-client \
        libclang-dev \
        libssl-dev \
        pkg-config \
        protobuf-compiler \
        ca-certificates

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.88.0
ENV PATH="/root/.cargo/bin:${PATH}"
# Use the git CLI for git dependencies (supports standard git credential helpers better than libgit2).
# See: https://doc.rust-lang.org/cargo/reference/config.html#netgit-fetch-with-cli
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

# If you build with private git dependencies, use SSH forwarding:
#   DOCKER_BUILDKIT=1 docker build --ssh default -f crates/utils/sov-proof-verifier-service/Dockerfile .
#
# Cargo dependencies use https URLs, so we rewrite them to SSH inside the build.
RUN --mount=type=ssh \
    mkdir -p -m 0700 /root/.ssh \
    && ssh-keyscan github.com >> /root/.ssh/known_hosts \
    && git config --global url."git@github.com:".insteadOf "https://github.com/" \
    && cargo build --release -p sov-proof-verifier-service

# ---------------------------------------------------------------------
# Fetch Ligero runtime assets (binaries + shader) from ligero-prover
#
# Sovereign no longer vendors Ligero binaries/shaders, but this image
# still needs the verifier/prover binaries and shaders at runtime.
# ---------------------------------------------------------------------
# Use the Ligero repo branch that includes `ligero-runner` and the prebuilt circuits in
# `utils/circuits/bins/`.
ARG LIGERO_PROVER_REF=feature/midnight-l2
ARG TARGETARCH
RUN --mount=type=ssh set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) LIGERO_PLATFORM="linux-amd64" ;; \
      arm64) LIGERO_PLATFORM="linux-arm64" ;; \
      *) echo "Unsupported TARGETARCH=${TARGETARCH}"; exit 1 ;; \
    esac; \
    mkdir -p /workspace/ligero/bin /workspace/ligero/lib /workspace/ligero/shader /workspace/ligero/programs; \
    tmpdir="$(mktemp -d)"; \
    git clone --depth 1 --branch "${LIGERO_PROVER_REF}" git@github.com:dcSpark/ligero-prover.git "${tmpdir}/ligero-prover"; \
    root="${tmpdir}/ligero-prover"; \
    cp -a "${root}/utils/portable-binaries/${LIGERO_PLATFORM}/bin/." /workspace/ligero/bin/; \
    cp -a "${root}/utils/portable-binaries/${LIGERO_PLATFORM}/lib/." /workspace/ligero/lib/; \
    if [ -d "${root}/utils/portable-binaries/shader" ]; then \
      cp -a "${root}/utils/portable-binaries/shader/." /workspace/ligero/shader/; \
    else \
      cp -a "${root}/shader/." /workspace/ligero/shader/; \
    fi; \
    cp -a "${root}/utils/circuits/bins/." /workspace/ligero/programs/; \
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

# Guest program artifacts (WASM) are owned by the Ligero repo and are copied from the
# pinned Ligero branch during image build. You can still override them via volume-mount.

# Copy keys, config, and setup directories
COPY examples/test-data/keys/token_deployer_private_key.json keys/token_deployer_private_key.json
COPY examples/rollup-ligero/rollup_config.toml /app/rollup_config.toml
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
    ROLLUP_CONFIG_PATH=/app/rollup_config.toml \
    METHOD_ID=0x698c44527e4fa3f934471015da3caa61da1f4e167107dbba9df71a6545396fb3 \
    MAX_CONCURRENT_VERIFICATIONS=5 \
    LOG_LEVEL=debug \
    LIGERO_PROGRAM_PATH=/app/ligero/programs/note_spend_guest.wasm \
    LIGERO_SHADER_PATH=/app/ligero/shader \
    LIGERO_VERIFIER_BIN=/app/ligero/bin/webgpu_verifier \
    LIGERO_PROVER_PATH=/app/ligero/bin/webgpu_prover \
    LIGERO_PACKING=8192 \
    RUST_LOG=debug

EXPOSE 8080

ENTRYPOINT ["/app/docker-entrypoint.sh"]
