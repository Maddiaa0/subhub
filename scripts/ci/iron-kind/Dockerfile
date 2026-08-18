FROM rust:1.90-bookworm AS builder

WORKDIR /src
COPY . .
RUN cargo build --locked --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl openssl python3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/subhub /usr/local/bin/subhub
COPY scripts/ci/iron-kind/setup.sh /usr/local/libexec/subhub-iron-smoke/setup.sh
COPY scripts/ci/iron-kind/upstream.py /usr/local/libexec/subhub-iron-smoke/upstream.py

RUN chmod 0755 \
    /usr/local/bin/subhub \
    /usr/local/libexec/subhub-iron-smoke/setup.sh \
    /usr/local/libexec/subhub-iron-smoke/upstream.py
