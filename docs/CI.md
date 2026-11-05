# Build and CI

- **Image**: `Dockerfile` (cargo-chef dependency cache, `cargo build --release --locked --bin message-service`,
  `gcr.io/distroless/cc-debian12:nonroot`, uid/gid 65532, no shell, ports 9090 gRPC / 8081 health).
- **`.github/workflows/rust-ci.yml`** runs on every pull request. Path filtering skips jobs
  (skipped = passing) for unrelated changes. Check names: `Rust fmt`, `Rust clippy`, `Rust test` (Postgres 16 + Redis 7 via testcontainers on the runner's Docker, `REQUIRE_INTEGRATION=1` so a skip fails);
  non-blocking `Rust audit (non-blocking)`; PR-only `Docker build (no push)`; main-only
  `Build and push image` (keyless WIF push to
  `us-central1-docker.pkg.dev/k8s-dev-412419/gke-rust-api-o2/gke-rust-api-o2`).
- **`.github/workflows/policy-check.yml`**: Kyverno + Polaris against `k8s/`.
- Toolchain is pinned in `rust-toolchain.toml`; `Cargo.lock` must be committed (`--locked`).
- No API keys or secrets are used by CI.
