FROM rust:1.84-slim-bookworm AS builder


WORKDIR /usr/src/app

COPY . .

RUN --mount=type=cache,target=/var/cache/apt apt-get update && apt-get install -y \
    build-essential \
    cmake \
    git \
    libssl-dev \
    pkg-config \
    && cargo build --release

FROM debian:bookworm-slim AS production

LABEL org.opencontainers.image.source="https://github.com/scylladb/latte"
LABEL org.opencontainers.image.title="ScyllaDB latte"

COPY --from=builder /usr/src/app/target/release/latte /usr/local/bin/latte

RUN --mount=type=cache,target=/var/cache/apt apt-get update \
    && apt-get upgrade -y \
    && apt-get install -y libssl3 \
    && apt-get autoremove -y \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

ENTRYPOINT [ "latte" ]
