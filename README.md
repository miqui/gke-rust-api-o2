# gke-rust-api-o2

Rust (tonic) gRPC Message/Author service on GKE — a **work-in-progress port** of the Spring Boot gRPC service
[miqui/gke-springboot-grpc-o2](https://github.com/miqui/gke-springboot-grpc-o2).

- Infrastructure (k8s manifests, GKE scripts, k6 tests, CI, docs) is carried over unchanged.
- The Java application code is not included; the Rust implementation is in progress.
- Proto contract: `proto/message/v1/message_service.proto`; DB migrations: `migrations/`.
- Original README: [docs/SPRINGBOOT-README.md](docs/SPRINGBOOT-README.md).
