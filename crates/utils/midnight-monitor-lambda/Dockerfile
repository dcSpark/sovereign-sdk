# syntax=docker/dockerfile:1.6

FROM rust:1.88.0-slim-bullseye AS builder

WORKDIR /workspace

ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        curl \
        git \
        libclang-dev \
        libssl-dev \
        llvm-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY . .

RUN cargo build --locked --release -p midnight-monitor-lambda -p mcp-external-stress

FROM public.ecr.aws/lambda/provided:al2023

COPY --from=builder /workspace/target/release/midnight-monitor-lambda /var/task/bootstrap
COPY --from=builder /workspace/target/release/mcp-external-stress /usr/local/bin/mcp-external-stress
COPY scripts/mcp-external-stress.sh /var/task/scripts/mcp-external-stress.sh

RUN chmod 755 /var/task/bootstrap /usr/local/bin/mcp-external-stress /var/task/scripts/mcp-external-stress.sh

ENTRYPOINT ["/var/task/bootstrap"]
