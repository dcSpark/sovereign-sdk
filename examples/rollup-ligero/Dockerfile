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
        libtss2-dev \
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

RUN cargo build --release \
    -p sov-rollup-ligero \
    -p oracle \
    --features sov-modules-rollup-blueprint/tee

FROM debian:bullseye-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libssl1.1 \
        libtss2-esys-3.0.2-0 \
        openssl \
        python3 \
        tpm2-tools \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /mcs

COPY --from=builder /workspace/target/release/sov-rollup-ligero /usr/local/bin/sov-rollup-ligero
COPY --from=builder /workspace/target/release/oracle /usr/local/bin/oracle
COPY examples/rollup-ligero/docker-entrypoint.sh /usr/local/bin/rollup-ligero-entrypoint

COPY examples/rollup-ligero/rollup_config_replica.toml /etc/mcs/rollup_config_replica.toml
COPY docker/rollup_ligero_oracle_pubkey.env /etc/mcs/oracle_pubkey.env
COPY crates/oracle/policies /etc/mcs/oracle-policies
RUN chmod +x /usr/local/bin/rollup-ligero-entrypoint \
    && command -v tpm2_checkquote >/dev/null

COPY examples/test-data /usr/share/mcs/test-data
RUN mkdir -p /mcs/demo_data_replica /run/mcs

ENV RUST_LOG=info \
    SOV_PROVER_MODE=execute \
    SOV_TEE_MOCK_ATTESTATION=0 \
    SOV_TEE_ATTESTATION_URL=http://attestation:8000/ \
    MCS_HOME=/mcs \
    MCS_CONFIG_DIR=/etc/mcs \
    MCS_SHARE_DIR=/usr/share/mcs \
    ORACLE_PUBKEY_ENV=/etc/mcs/oracle_pubkey.env \
    ENABLE_ORACLE=0 \
    ORACLE_DEV_ACCEPT_ALL=false \
    ORACLE_SERVER_BIND_ADDRESS=127.0.0.1:8090 \
    ORACLE_POLICIES_DIR=/etc/mcs/oracle-policies

EXPOSE 8090 12347 13201

VOLUME ["/mcs/demo_data_replica"]

ENTRYPOINT ["/usr/local/bin/rollup-ligero-entrypoint"]
CMD ["replica"]
