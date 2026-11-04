# Rust service (tonic) — architecture, configuration, running

The `message-service` is a Rust port of the Spring Boot service: same gRPC contract
(`proto/message/v1/message_service.proto`, unchanged, wire-compatible), same PostgreSQL schema
(Flyway migrations `migrations/V1`, `V2`), same error model, validation, optimistic locking and
keyset pagination. Hazelcast is replaced by Redis (Memorystore on GKE).

## Architecture

```
            :9090 cleartext h2 (TLS ends at the load balancer)         :8081 HTTP/1.1
  client ─▶ tonic Server                                              ─▶ axum
             ├─ grpc.health.v1.Health   (SERVING; NOT_SERVING on SIGTERM)    /livez   process up
             ├─ grpc.reflection.v1[alpha]                                    /readyz  DB (Redis reported, non-fatal)
             ├─ message.v1.MessageService ─┐                                 /metrics Prometheus text
             └─ message.v1.AuthorService ──┤
                       grpc.rs  (proto plumbing + hand validation + per-RPC span/metrics/log)
                       service.rs (Messages / Authors: transactions, locking, cache orchestration)
                          ├─ repo.rs   sqlx queries (PostgreSQL)
                          └─ cache.rs  Redis cache-aside + per-key distributed lock
```

| Module | Java counterpart |
|---|---|
| `grpc.rs` | `*GrpcService`, `RpcMetricsInterceptor` |
| `validation.rs` | `Violations` (hand validation; Java whitespace + UTF-16 length semantics) |
| `error.rs` | `GrpcErrorHandler`, `ApiException` (`google.rpc.Status` + `ErrorInfo` + `BadRequest`) |
| `service.rs` | `service/Messages`, `service/Authors` |
| `repo.rs` | the JDBC repositories |
| `cache.rs` | `cache/HazelcastMessageCache` |
| `seed.rs` | `WelcomeSeeder` |
| `migrate.rs` | Flyway (see below) |
| `management.rs`, `metrics.rs`, `telemetry.rs` | actuator probes, Micrometer, OTel tracing |

### Behaviour reproduced from the Java service

* **Error model**: every error is a `google.rpc.Status` carrying `ErrorInfo{domain="message-service.miqui.dev", reason}`
  (`BAD_USER_INPUT`, `NOT_FOUND`, `CONFLICT`, `INTERNAL_SERVER_ERROR`), and `BadRequest.field_violations` for validation.
  Unexpected errors never leak details (logged server-side only). Codes: `INVALID_ARGUMENT`, `NOT_FOUND`,
  `ABORTED` (stale version), `ALREADY_EXISTS` (duplicate email), `FAILED_PRECONDITION` (author still has messages).
* **Validation** collects *all* violations (same messages and order as Java). Strings are stripped with Java
  `Character.isWhitespace` semantics (so NBSP is *not* whitespace) and length limits count UTF-16 code units.
  Limits: title 100, content 1000, name 50, email 100, `limit` 1..=200 (default 50), `offset` >= 0; request bodies
  > 16 KiB are rejected by the transport.
* **Optimistic locking**: `UPDATE … WHERE id=$1 AND version=$2`; zero rows → `ABORTED/CONFLICT` (or `NOT_FOUND` if the row
  is gone). Explicit transactions at PostgreSQL's default `READ COMMITTED` (as Spring's default isolation).
* **Pagination**: stable order `(created_at, id)` tiebreak; `total_count`; `GetAuthor` returns `next_messages_offset` only when
  more rows exist.
* **Seeder**: on a database with no messages, inserts `system@message-service.local` and the welcome message once, even
  when all replicas start together (`pg_advisory_xact_lock`). Disable with `APP_SEED_ENABLED=false`.
* **Metrics** per RPC: `grpc_server_seconds{rpc_service,rpc_method,grpc_status_code}` (histogram) and
  `grpc_errors_total{…,error_code}`.

### Cache (Redis replaces Hazelcast)

Read-through / cache-aside for `GetMessage` only, same as Java.

* Key `messages:<uuid>` → protobuf-encoded `Message` (author is *not* trusted from the cache: a hit re-reads the live author,
  so author edits are visible immediately, as in Java).
* Per-key distributed lock `lock:messages:<uuid>` (`SET NX PX`, 10 s lease, unique token, release via compare-and-delete Lua)
  serialises "read DB + fill" against "write DB + evict". Lock wait is 5 s.
* Invalidate-after-write: `UpdateMessage` and `DeleteMessage` evict the key (also on a stale-version failure).
* Entries carry a safety-net TTL (`CACHE_TTL_SECONDS`, default 3600; `0` = none, which is the Java behaviour).
* **Never fails a request.** Connection failure, timeout (`CACHE_TIMEOUT_MS`, default 500), unreadable entry, or lock timeout all
  degrade to the database; a lock timeout skips the cache fill (a writer may be racing). A circuit breaker (5 s) stops hammering a dead Redis.
* Metrics: `message_cache_seconds{operation,outcome}`, `message_cache_errors_total{operation}`.

### Migrations

`migrate.rs` embeds `migrations/V*.sql` and applies pending ones at startup under a PostgreSQL advisory lock
(safe with many replicas). A database created by the Java service's Flyway is **adopted, not modified**: versions
recorded as successful in `flyway_schema_history` count as applied, and Flyway's table is never written to. Rust-applied versions
are recorded in `schema_migrations_rust`.

## Configuration (environment variables only)

Names and secret keys are the same as the Java service so the existing manifests keep working.

| Variable | Default | Notes |
|---|---|---|
| `DB_HOST` / `DB_PORT` / `DB_NAME` | `localhost` / `5432` / `messagedb` | |
| `DB_USER` / `DB_PASSWORD` | `message_app` / `message_app` | from the `postgres-credentials` secret |
| `DB_SSLMODE` | `prefer` | `disable`, `prefer`, `require` (encrypt, no verification; `verify-*` would need a root cert, which is not configurable) |
| `DB_POOL_MAX` | `10` | |
| `REDIS_HOST` | *(unset = no cache)* | |
| `REDIS_PORT` | `6379` | |
| `REDIS_AUTH_STRING` | – | optional |
| `REDIS_TLS` | `false` | `rediss://`; roots = webpki bundle, plus `REDIS_CA_CERT_PATH` |
| `REDIS_CA_CERT_PATH` | – | PEM CA for Memorystore |
| `CACHE_TTL_SECONDS` | `3600` | `0` disables expiry |
| `CACHE_TIMEOUT_MS` | `500` | per Redis operation |
| `GRPC_PORT` / `MANAGEMENT_PORT` | `9090` / `8081` | |
| `GRPC_SHUTDOWN_GRACE_SECONDS` | `20` | max drain after SIGTERM |
| `APP_SEED_ENABLED` | `true` | welcome message seeder |
| `OTEL_EXPORT_ENABLED` | `true` | |
| `OTEL_TRACES_URL` | `http://localhost:4318/v1/traces` | OTLP/HTTP |
| `TRACES_SAMPLING_PROBABILITY` | `0.1` | parent-based ratio |
| `POD_NAME` | – | `k8s.pod.name` resource attribute |
| `LOG_FORMAT` | `json` | `text` for humans |
| `RUST_LOG` | `info,sqlx=warn,…` | |

## Shutdown

SIGTERM/SIGINT: `/readyz` → 503 and grpc.health → `NOT_SERVING`, in-flight RPCs drain (bounded by `GRPC_SHUTDOWN_GRACE_SECONDS`),
the pool closes, the process exits 0.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test            # Docker needed: Postgres 16 + Redis 7 via testcontainers
```

Without Docker, point the tests at existing servers: `TEST_DATABASE_URL=postgres://user:pw@host:5432`
(a throw-away database is created per test) and `TEST_REDIS_URL=redis://host:6379`. With neither, the DB-backed tests
print `SKIPPED` and pass; set `REQUIRE_INTEGRATION=1` (CI) to make that an error. `protoc` is not needed
(`protoc-bin-vendored`).

## Run manually

These are the commands that were executed to verify the service (Docker Desktop, macOS).

```sh
docker run -d --rm --name rs-pg -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=messages -p 55432:5432 postgres:16-alpine
docker run -d --rm --name rs-redis -p 56379:6379 redis:7-alpine

DB_HOST=127.0.0.1 DB_PORT=55432 DB_NAME=messages DB_USER=postgres DB_PASSWORD=pw DB_SSLMODE=disable \
REDIS_HOST=127.0.0.1 REDIS_PORT=56379 OTEL_EXPORT_ENABLED=false cargo run --release
```

Startup log (JSON lines; abridged to the message):

```
starting message-service          {"grpc_port":9090,"management_port":8081}
applying migration                {"version":1,"description":"initial schema"}
applying migration                {"version":2,"description":"author message pagination index"}
migrations up to date             {"applied":"[1, 2]"}
welcome message created
listening                         {"grpc_addr":"0.0.0.0:9090","mgmt":"0.0.0.0:8081"}
```

Probes:

```sh
$ curl -s -i localhost:8081/readyz
HTTP/1.1 200 OK
content-type: text/plain; charset=utf-8
…
{"status":"UP","components":{"db":"UP","cache":"UP"}}
$ curl -s -i localhost:8081/livez | head -1
HTTP/1.1 200 OK
```

gRPC (grpcurl via Docker — Docker Desktop reaches the host as `host.docker.internal`):

```sh
$ docker run --rm fullstorydev/grpcurl -plaintext host.docker.internal:9090 list
grpc.health.v1.Health
grpc.reflection.v1.ServerReflection
message.v1.AuthorService
message.v1.MessageService

$ docker run --rm fullstorydev/grpcurl -plaintext -d '{}' host.docker.internal:9090 message.v1.MessageService/ListMessages
{
  "items": [
    {
      "id": "a15a2b33-3c78-4373-baf2-afab5d1cc698",
      "title": "Welcome to the Kubernetes gRPC API",
      "content": "A sample message backed by Rust (tonic) gRPC and Cloud SQL for PostgreSQL on GKE.",
      "createdAt": "2026-11-04T19:07:02.350241Z",
      "author": { "id": "26a4aa80-…", "name": "system", "email": "system@message-service.local", "createdAt": "2026-11-04T19:07:02.350241Z" }
    }
  ],
  "totalCount": "1"
}

$ docker run --rm fullstorydev/grpcurl -plaintext -d '{"id":"nope"}' host.docker.internal:9090 message.v1.MessageService/GetMessage
ERROR:
  Code: InvalidArgument
  Message: The request content was invalid or failed validation constraints.
  Details:
  1)	{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", "domain": "message-service.miqui.dev", "reason": "BAD_USER_INPUT" }
  2)	{ "@type": "type.googleapis.com/google.rpc.BadRequest",
  	  "fieldViolations": [ { "field": "id", "description": "id must be a valid UUID" } ] }
```

(The error output above is reflowed for width; grpcurl prints each detail pretty-printed over several lines.)

Metrics (excerpt):

```sh
$ curl -s localhost:8081/metrics | grep -E '^grpc_errors_total'
grpc_errors_total{error_code="BAD_USER_INPUT",grpc_status_code="INVALID_ARGUMENT",rpc_method="GetMessage",rpc_service="message.v1.MessageService"} 1
```

Graceful shutdown (`pkill -TERM -f target/release/message-service`) logged
`shutdown signal received; draining` then `stopped`, and the process exited.

Cleanup: `docker rm -f rs-pg rs-redis`.
