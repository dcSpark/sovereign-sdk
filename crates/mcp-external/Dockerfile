# syntax=docker/dockerfile:1.6

FROM rust:1.88-slim-bookworm AS builder

WORKDIR /app

ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
# Use the git CLI for git dependencies (supports standard git credential helpers better than libgit2).
# See: https://doc.rust-lang.org/cargo/reference/config.html#netgit-fetch-with-cli
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true

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

# Build using HTTPS for git dependencies to avoid requiring SSH keys.
RUN cargo build --locked --release -p mcp-external

# ---------------------------------------------------------------------
# Fetch Ligero runtime assets (binaries + shader) from ligero-prover
#
# These assets are kept for optional local proving/debugging. MCP-External
# now uses the Ligero HTTP proof service by default.
# ---------------------------------------------------------------------
# Use the Ligero repo branch that includes `ligero-runner` and the prebuilt circuits in
# `utils/circuits/bins/`.
ARG LIGERO_PROVER_REF=main
ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH:-amd64}" in \
      amd64) LIGERO_PLATFORM="linux-amd64" ;; \
      arm64) LIGERO_PLATFORM="linux-arm64" ;; \
      *) LIGERO_PLATFORM="linux-arm64" ;; \
    esac; \
    mkdir -p /opt/ligero/bin /opt/ligero/lib /opt/ligero/shader /opt/ligero/programs; \
    tmpdir="$(mktemp -d)"; \
    git clone --depth 1 --branch "${LIGERO_PROVER_REF}" https://github.com/dcSpark/ligero-prover.git "${tmpdir}/ligero-prover"; \
    root="${tmpdir}/ligero-prover"; \
    cp -a "${root}/utils/portable-binaries/${LIGERO_PLATFORM}/bin/." /opt/ligero/bin/; \
    cp -a "${root}/utils/portable-binaries/${LIGERO_PLATFORM}/lib/." /opt/ligero/lib/; \
    if [ -d "${root}/utils/portable-binaries/shader" ]; then \
      cp -a "${root}/utils/portable-binaries/shader/." /opt/ligero/shader/; \
    else \
      cp -a "${root}/shader/." /opt/ligero/shader/; \
    fi; \
    cp -a "${root}/utils/circuits/bins/." /opt/ligero/programs/; \
    rm -rf "${tmpdir}"


FROM ubuntu:24.04 AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates \
       libsqlite3-0 \
       libssl3 \
       libvulkan1 \
       vulkan-tools \
       mesa-vulkan-drivers \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

RUN groupadd --system mcp && useradd --system --no-create-home --gid mcp --home /app mcp

COPY --from=builder /app/target/release/mcp-external /usr/local/bin/mcp-external

# Ligero assets (binaries/shaders/programs are copied from the Ligero branch during build).
COPY --from=builder /opt/ligero/bin /opt/ligero/bin
COPY --from=builder /opt/ligero/lib /opt/ligero/lib
COPY --from=builder /opt/ligero/shader /opt/ligero/shader
COPY --from=builder /opt/ligero/programs /opt/ligero/programs

RUN chmod +x /opt/ligero/bin/webgpu_prover \
    && mkdir -p /app/logs /app/proof_outputs \
    && chown -R mcp:mcp /app /opt/ligero

ENV MCP_SERVER_BIND_ADDRESS=0.0.0.0:3000 \
    LIGERO_PROOF_SERVICE_URL=http://127.0.0.1:1313 \
    LIGERO_PROGRAM_PATH=note_spend_guest \
    RUST_LOG=info

EXPOSE 3000

USER mcp

ENTRYPOINT ["mcp-external"]
