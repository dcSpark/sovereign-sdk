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

# Ligero assets (Linux/amd64 build)
COPY crates/adapters/ligero/guest/bins/programs /opt/ligero/programs
COPY crates/adapters/ligero/bins/linux-amd64/bin /opt/ligero/bin
COPY crates/adapters/ligero/bins/linux-amd64/lib /opt/ligero/lib
COPY crates/adapters/ligero/bins/shader /opt/ligero/shader

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
