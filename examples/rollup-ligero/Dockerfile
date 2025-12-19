# syntax=docker/dockerfile:1.5

FROM rust:1.88.0-slim-bullseye AS builder

RUN apt-get update \
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
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

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

RUN cargo build --release -p sov-rollup-ligero

FROM debian:bullseye-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl1.1 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /workspace/target/release/sov-rollup-ligero /usr/local/bin/sov-rollup-ligero

COPY examples/rollup-ligero/rollup_config.toml rollup_config.toml
RUN sed -i 's/bind_host = "127.0.0.1"/bind_host = "0.0.0.0"/' rollup_config.toml

COPY examples/test-data test-data
RUN mkdir -p demo_data

ENV RUST_LOG=info \
    SOV_PROVER_MODE=execute

EXPOSE 12346 9845

VOLUME ["/app/demo_data"]

ENTRYPOINT ["/usr/local/bin/sov-rollup-ligero"]
CMD ["--rollup-config-path", "/app/rollup_config.toml", "--genesis-config-dir", "/app/test-data/genesis/demo/mock", "--da-layer", "mock", "--prometheus-exporter-bind", "0.0.0.0:9845"]
