# syntax=docker/dockerfile:1.6

FROM rust:1.88-slim-bookworm AS builder

WORKDIR /app

ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       build-essential \
       pkg-config \
       libsqlite3-dev \
       libssl-dev \
       clang \
       llvm-dev \
       libclang-dev \
       curl \
       ca-certificates \
       git \
    && rm -rf /var/lib/apt/lists/*

COPY . .

RUN cargo build --locked --release -p mcp-external

# ---------------------------------------------------------------------
# Fetch Ligero runtime assets (binaries + shader) from ligero-prover
#
# Sovereign no longer vendors Ligero binaries/shaders, but downstream
# images still need the binaries/shaders at runtime.
# ---------------------------------------------------------------------
ARG LIGERO_PROVER_REV=1b9ba6fd83af9ef2a2d477c50f6faded0fb03876
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) LIGERO_PLATFORM="linux-amd64" ;; \
      arm64) LIGERO_PLATFORM="linux-arm64" ;; \
      *) echo "Unsupported TARGETARCH=${TARGETARCH}"; exit 1 ;; \
    esac; \
    mkdir -p /opt/ligero/bin /opt/ligero/lib /opt/ligero/shader; \
    tmpdir="$(mktemp -d)"; \
    curl -fsSL "https://codeload.github.com/dcSpark/ligero-prover/tar.gz/${LIGERO_PROVER_REV}" -o "${tmpdir}/ligero-prover.tar.gz"; \
    tar -xzf "${tmpdir}/ligero-prover.tar.gz" -C "${tmpdir}"; \
    root="$(find "${tmpdir}" -maxdepth 1 -type d -name 'ligero-prover-*' | head -n 1)"; \
    test -n "${root}"; \
    cp -a "${root}/utils/portable-binaries/${LIGERO_PLATFORM}/bin/." /opt/ligero/bin/; \
    cp -a "${root}/utils/portable-binaries/${LIGERO_PLATFORM}/lib/." /opt/ligero/lib/; \
    cp -a "${root}/shader/." /opt/ligero/shader/; \
    rm -rf "${tmpdir}"


FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates \
       libsqlite3-0 \
       libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

RUN groupadd --system mcp && useradd --system --no-create-home --gid mcp --home /app mcp

COPY --from=builder /app/target/release/mcp-external /usr/local/bin/mcp-external

# Ligero assets
COPY crates/adapters/ligero/guest/bins/programs /opt/ligero/programs
COPY --from=builder /opt/ligero/bin /opt/ligero/bin
COPY --from=builder /opt/ligero/lib /opt/ligero/lib
COPY --from=builder /opt/ligero/shader /opt/ligero/shader

RUN chmod +x /opt/ligero/bin/webgpu_prover \
    && mkdir -p /app/logs /app/proof_outputs \
    && chown -R mcp:mcp /app /opt/ligero

ENV MCP_SERVER_BIND_ADDRESS=0.0.0.0:3000 \
    LIGERO_PROGRAM_PATH=/opt/ligero/programs/note_spend_guest.wasm \
    LIGERO_PROVER_BINARY_PATH=/opt/ligero/bin/webgpu_prover \
    LIGERO_SHADER_PATH=/opt/ligero/shader \
    LD_LIBRARY_PATH=/opt/ligero/lib \
    RUST_LOG=info

EXPOSE 3000

USER mcp

ENTRYPOINT ["mcp-external"]
