FROM rust:1.89-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release --bin mindreader

FROM debian:bookworm-slim
ARG MINDREADER_VERSION=0.5.0
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/mindreader /usr/local/bin/mindreader
RUN mkdir -p /config/mindreader
COPY packaging/mindreader.docker.toml /config/mindreader/config.toml

ENV XDG_CONFIG_HOME=/config

LABEL org.opencontainers.image.title="Mindreader" \
      org.opencontainers.image.description="Deterministic, privacy-first Neo4j memory MCP server" \
      org.opencontainers.image.source="https://github.com/bnomei/mindreader" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${MINDREADER_VERSION}"

ENTRYPOINT ["/usr/local/bin/mindreader"]
