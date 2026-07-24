# syntax=docker/dockerfile:1
# Multi-stage build shared by both binaries; pick one with `--build-arg BIN=`.

FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
ARG BIN
RUN cargo build --release --bin "$BIN" && cp "target/release/$BIN" /app/service

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 10001 app \
    && mkdir -p /app/data && chown -R app:app /app
WORKDIR /app
COPY --from=builder /app/service /usr/local/bin/service
COPY config /app/config
USER app
ENTRYPOINT ["/usr/local/bin/service"]
