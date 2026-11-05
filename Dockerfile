# syntax=docker/dockerfile:1
# Multi-stage Dockerfile for the Rust gRPC API (binary: message-service).
#
# Runtime is distroless cc-debian12 (glibc), not static musl: musl's allocator is markedly slower
# under multi-threaded tokio load and C-dependency builds get fiddly. The builder is Debian
# bookworm (same glibc as the runtime), so the binary links against what it runs on. TLS is rustls
# (no OpenSSL), so cc-debian12 has everything the binary needs.

ARG RUST_IMAGE=rust:1-bookworm

# --- chef: cargo-chef so dependency compilation caches as its own layer ---
FROM ${RUST_IMAGE} AS chef
RUN cargo install cargo-chef --locked
WORKDIR /build

# --- planner: dependency-only recipe (changes only when manifests change) ---
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# --- builder ---
FROM chef AS builder
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
# rust-toolchain.toml pins the toolchain; rustup installs it on first cargo call.
COPY rust-toolchain.toml ./
COPY --from=planner /build/recipe.json recipe.json
# build.rs needs the protos (and vendored third_party protos) even when only cooking dependencies.
COPY proto/ proto/
COPY third_party/ third_party/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --locked --recipe-path recipe.json
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked --bin message-service \
 && cp target/release/message-service /message-service

# --- runtime: no shell, no package manager; ca-certificates + tzdata included ---
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /message-service /app/message-service

# Numeric uid/gid 65532 (distroless "nonroot"): the kubelet can only verify runAsNonRoot for a
# numeric USER. Nothing writes to the image filesystem, so a read-only rootfs works as-is.
# k8s/deployment.yaml should set runAsUser/runAsGroup 65532 (the Java image used 10001).
USER 65532:65532
WORKDIR /app
# 9090: gRPC (h2c). 8081: health/metrics (kubelet probes, load-balancer health check).
EXPOSE 9090 8081

# exec form: the binary is PID 1 and receives SIGTERM from Kubernetes directly.
ENTRYPOINT ["/app/message-service"]
