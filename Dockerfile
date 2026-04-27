# syntax=docker/dockerfile:1

# ---------- build stage ----------
FROM rust:latest AS builder
WORKDIR /app

# 1) Pre-compile the dependency graph against a dummy main so that subsequent
#    edits to src/ only re-link the binary instead of recompiling every crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src target/release/deps/lark_poker* target/release/lark-poker*

# 2) Real build.
COPY src ./src
RUN cargo build --release \
    && strip target/release/lark-poker

# ---------- runtime stage ----------
FROM debian:latest
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/lark-poker /usr/local/bin/lark-poker

EXPOSE 8080
ENV RUST_LOG=lark_poker=info,tower_http=info \
    BIND_ADDR=0.0.0.0:8080

ENTRYPOINT ["/usr/local/bin/lark-poker"]
