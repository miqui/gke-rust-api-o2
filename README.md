# gke-rust-api-o2

Rust (tonic) gRPC Message/Author service on GKE — a **work-in-progress port** of the Spring Boot gRPC service
[miqui/gke-springboot-grpc-o2](https://github.com/miqui/gke-springboot-grpc-o2).

- Infrastructure (k8s manifests, GKE scripts, k6 tests, CI, docs) is carried over unchanged.
- The Java application code is not included; the Rust implementation is in progress.
- Proto contract: `proto/message/v1/message_service.proto`; DB migrations: `migrations/`.
- Original README: [docs/SPRINGBOOT-README.md](docs/SPRINGBOOT-README.md).

## Deployment (GKE)

`gke-deploy.sh` creates the GCP side (VPC, GKE, private service access, Artifact Registry, IAM);
`gke-bootstrap.sh` installs Argo CD, which syncs everything under `k8s/` (app of apps in
`k8s/argocd/apps/`). Cloud resources are provisioned by Crossplane from platform APIs, never by
scripts: **Postgres** (`PostgresInstance` -> Cloud SQL) and **Redis** (`RedisInstance` ->
Memorystore, see [docs/REDIS-CROSSPLANE.md](docs/REDIS-CROSSPLANE.md)). `gke-teardown.sh` removes
both before deleting the cluster.

### Architecture

```
client --gRPC/TLS--> Gateway (grpc.miqui.dev) --h2c--> message-service (Rust, 3-6 pods, HPA)
                                                          |-- :9090 gRPC (grpc.health.v1)
                                                          |-- :8081 /livez /readyz /metrics
                                                          |-- 10.16.0.0/20:5432 -> Cloud SQL (private IP, TLS)
                                                          `-- 10.16.0.0/20:6379 -> Memorystore Redis (AUTH + TLS)
Prometheus --scrape :8081/metrics--> message-service      OTLP --> OTel Collector --> OpenObserve
```

- Image: `us-central1-docker.pkg.dev/k8s-dev-412419/gke-rust-api-o2/gke-rust-api-o2:<yyyymmddHHMMSS>-<sha7>`
  (Argo CD Image Updater, `k8s/argocd/image-updater/`); container runs as UID 65532, read-only root FS.
- Env contract: `DB_HOST` (Cloud SQL secret), `DB_PORT`, `DB_NAME`, `DB_SSLMODE`, `DB_USER`,
  `DB_PASSWORD`; `REDIS_HOST`, `REDIS_PORT`, `REDIS_AUTH_STRING`, `REDIS_TLS`, `REDIS_CA_CERT_PATH`.
- The Hazelcast cache and its JMX exporter/dashboard were removed; there is no in-cluster cache.
- Not applied from this repo by hand: Argo CD reconciles `k8s/`.

