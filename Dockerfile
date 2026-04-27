# syntax=docker/dockerfile:1

# ---------- planner: hash the dependency graph (cargo-chef recipe) ----------
FROM lukemathwalker/cargo-chef:latest-rust-latest AS chef
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

# ---------- cooked: compile every dep once, in both debug+release profiles.
# This layer's cache key is recipe.json (= Cargo.lock + manifests). Source
# edits in src/ do NOT bust this layer.
FROM chef AS cooked
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --recipe-path recipe.json \
    && cargo chef cook --release --recipe-path recipe.json

# ---------- source: shared base for test + builder ----------
FROM cooked AS source
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# ---------- test: cargo test runs here. Failure aborts the whole build. ----
FROM source AS test
RUN cargo test --locked

# ---------- builder: depends on test, so tests must pass before binary build.
# Reuses the cooked release deps; only the larkriver crate links here.
FROM test AS builder
RUN cargo build --release --locked

# ---------- runtime: distroless cc, ≈ 32 MB final image, runs as nonroot.
# Note: must be `cc` (= base + libgcc + libstdc++), NOT `base`. Rust's default
# `panic = unwind` dlopens libgcc_s.so.1 at runtime; without it the binary
# fails to start with "error while loading shared libraries: libgcc_s.so.1".
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/larkriver /app/larkriver

EXPOSE 8080
ENV RUST_LOG=larkriver=info,tower_http=info \
    BIND_ADDR=0.0.0.0:8080

ENTRYPOINT ["/app/larkriver"]
