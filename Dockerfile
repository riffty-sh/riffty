FROM rust:1.88-bookworm AS rust-builder
WORKDIR /workspace
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY apps ./apps
COPY tests ./tests
RUN cargo build --release --bin riffty --bin riffty_mcp --bin riffty-ab

FROM eclipse-temurin:25-jdk AS java-builder
WORKDIR /workspace
RUN apt-get update \
    && apt-get install -y --no-install-recommends maven \
    && rm -rf /var/lib/apt/lists/*
COPY plugins/riffty-java ./plugins/riffty-java
RUN bash plugins/riffty-java/package_jlink_aot.sh

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 riffty
COPY --from=rust-builder /workspace/target/release/riffty /usr/local/bin/riffty
COPY --from=rust-builder /workspace/target/release/riffty_mcp /usr/local/bin/riffty_mcp
COPY --from=rust-builder /workspace/target/release/riffty-ab /usr/local/bin/riffty-ab
COPY --from=java-builder /workspace/plugins/riffty-java/dist /usr/local/bin/riffty-java
COPY rules /opt/riffty/rules
USER riffty
WORKDIR /repo
ENTRYPOINT ["riffty"]
CMD ["check", "."]
