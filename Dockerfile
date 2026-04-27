# syntax=docker/dockerfile:1

# ---------- build ----------
FROM rust:latest AS builder
WORKDIR /app

# Pre-compile the dependency graph against a dummy main so subsequent edits
# under src/ only re-link the binary instead of recompiling every crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src target/release/deps/lark_poker* target/release/lark-poker*

# Real build. Release profile is tuned in Cargo.toml (fat LTO, single codegen
# unit, stripped symbols) — no extra cargo flags needed here.
COPY src ./src
RUN cargo build --release

# ---------- runtime ----------
# distroless base = glibc + libssl + ca-certs + tzdata, no shell, no package
# manager. Runs as uid 65532 (`nonroot`). Final image ≈ 30 MB.
FROM gcr.io/distroless/base-debian12:nonroot
COPY --from=builder /app/target/release/lark-poker /app/lark-poker

EXPOSE 8080
ENV RUST_LOG=lark_poker=info,tower_http=info \
    BIND_ADDR=0.0.0.0:8080

ENTRYPOINT ["/app/lark-poker"]
