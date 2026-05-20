FROM rust:1.94-slim-trixie AS builder

WORKDIR /usr/src/app

ENV RUSTFLAGS="--cfg fetch_extended_version_info --cfg tokio_unstable"

COPY . .

RUN apt-get update && apt-get install -y \
    build-essential \
    cmake \
    git \
    libssl-dev \
    pkg-config \
    && cargo build --release \
    && cargo build --release --no-default-features --features alternator

FROM debian:trixie-slim AS production

LABEL org.opencontainers.image.source="https://github.com/scylladb/latte"
LABEL org.opencontainers.image.title="ScyllaDB latte benchmarking tool"
LABEL com.scylladb.loader-type="latte"

COPY --from=builder /usr/src/app/target/release/latte /usr/local/bin/latte
COPY --from=builder /usr/src/app/target/release/latte-alternator /usr/local/bin/latte-alternator

RUN --mount=type=cache,target=/var/cache/apt apt-get update \
    && apt-get upgrade -y \
    && apt-get install -y libssl3 \
    && apt-get autoremove -y \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

ENTRYPOINT [ "latte" ]
