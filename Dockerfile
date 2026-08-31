# Build with the toolchain pinned by rust-toolchain.toml; the slim image
# carries gcc/libc6-dev, which ring's build script needs.
FROM rust:slim-bookworm AS builder
WORKDIR /build

COPY rust-toolchain.toml ./
RUN rustup toolchain install

# Cache dependency compilation: build a stub crate against the real lockfile,
# then discard the stub's own artifacts so the real sources rebuild.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/subhub target/release/deps/subhub-* \
        target/release/deps/libsubhub-* target/release/.fingerprint/subhub-*

COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 subhub \
    && useradd --uid 10001 --gid subhub --create-home subhub \
    && mkdir -p /data && chown subhub:subhub /data && chmod 700 /data

COPY --from=builder /build/target/release/subhub /usr/local/bin/subhub

USER subhub
# The Linux credential store lives at $XDG_CONFIG_HOME/subhub/; mount a
# writable persistent volume at /data — the gateway rotates refresh tokens
# in place and a rotation lost to an ephemeral filesystem strands the account.
ENV HOME=/home/subhub \
    XDG_CONFIG_HOME=/data
VOLUME /data
EXPOSE 7842

# SUBHUB_CLIENT_TOKEN must be provided; --allow-remote refuses to start without it.
ENTRYPOINT ["/usr/local/bin/subhub"]
CMD ["gateway", "serve", "--listen", "0.0.0.0:7842", "--allow-remote"]
