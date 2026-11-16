FROM rust:1.88-bookworm AS builder
WORKDIR /workspace
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY apps ./apps
COPY tests ./tests
RUN cargo build --release --bin riffty --bin riffty_mcp

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 riffty
COPY --from=builder /workspace/target/release/riffty /usr/local/bin/riffty
COPY --from=builder /workspace/target/release/riffty_mcp /usr/local/bin/riffty_mcp
COPY rules /opt/riffty/rules
USER riffty
WORKDIR /repo
ENTRYPOINT ["riffty"]
CMD ["check", "."]

