# syntax=docker/dockerfile:1
FROM rust:1.88-bookworm AS builder
WORKDIR /app
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY .sqlx .sqlx
COPY src src
COPY migrations migrations
COPY static static
ENV SQLX_OFFLINE=true
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git git-lfs libssl3 curl \
    && git lfs install --system \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/muggingface /usr/local/bin/muggingface
COPY static /app/static
COPY migrations /app/migrations
ENV HOME=/data
ENV SEEDING_DIR=/data/seeding
ENV PORT=8080
ENV RUST_LOG=info
EXPOSE 8080
HEALTHCHECK --interval=15s --timeout=5s --retries=5 \
    CMD curl -fsS http://127.0.0.1:8080/healthz || exit 1
CMD ["muggingface"]
