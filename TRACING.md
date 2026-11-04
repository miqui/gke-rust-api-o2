# Tracing — message-service → OpenObserve

Distributed tracing for the REST API, exported over OTLP to the OTel Collector and from there to
OpenObserve. Traces sit alongside the metrics that already flow through the same collector (see
`PROMETHEUS.md`).

```
message-service ──► otel-collector:4318 ─────────────────────► OpenObserve (org "default")
  OTLP/HTTP          /v1/traces           OTLP/HTTP + basic auth
                                          /api/default/v1/traces
```

## What is traced

| Layer | Instrumentation | Spans |
| :--- | :--- | :--- |
| HTTP server | `opentelemetry-instrumentation-fastapi` | One server span per request, named `<METHOD> <route template>` (e.g. `PATCH /messages/{id}`) with `http.status_code`. `/health/liveness` and `/health/readiness` (kubelet probes) are excluded. W3C `traceparent` is propagated. |
| Database | `opentelemetry-instrumentation-sqlalchemy` (on the async engine's sync engine) | One span per SQL statement (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, with `db.statement`), plus a `connect` span per connection checkout from the pool. |
| Cache | manual spans in `app/cache.py` | `hazelcast.get` / `hazelcast.set` / `hazelcast.delete`, each with `db.system=hazelcast`, `db.operation` and `cache.map`; `get` also sets `cache.hit` (true/false). |

The Hazelcast client has no OpenTelemetry instrumentation, hence the hand-written `_traced()` wrapper
in `app/cache.py`. It matters for `GET /messages/{id}`, a cache-aside read: on a hit the trace has no
SQL spans at all, and without a `hazelcast.get` span the hop to the (separate) Hazelcast pod is an
unexplained gap inside the request span.

A `connect` span is the instrumentor wrapping `Engine.connect()`, i.e. checking a connection out of
the pool - it is not a new TCP connection to Postgres, and lasts a fraction of a millisecond.

The service calls nothing else over HTTP, so there is no cross-service trace.

## Code

`app/tracing.py`, called from `create_app()` in `app/main.py`. Decisions worth knowing before
changing it:

- **It runs at app creation, not in the lifespan.** `FastAPIInstrumentor.instrument_app()` adds
  middleware, which Starlette refuses once the app has started.
- **`exclude_spans=["receive", "send"]`.** Without it every request gets `http send` / `http receive`
  child spans, which multiply the span count without adding information.
- **The instrumentors get a no-op `MeterProvider`.** Left alone they publish their own
  `http_server_*` and `db_client_connections_*` metrics through the global provider, duplicating the
  ones in `app/telemetry.py` - labelled with the pod IP and the client-controlled `Host` header
  (`http_host`, `http_server_name`), which would let a caller mint Prometheus series at will. Spans
  are unaffected.
- **Resource attributes** mirror the metrics: `service.name` and `k8s.pod.name` (from the
  `POD_NAME` Downward API env var).
- **The exporter URL is used verbatim.** `OTEL_TRACES_URL` must include `/v1/traces`; the exporter
  does not append it when an endpoint is passed explicitly.
- **Bounded shutdown.** The exporter timeout is 5s, and `shutdown_tracing()` runs in the lifespan
  shutdown so the last batch of spans is flushed - without letting an unreachable collector hold the
  pod past `terminationGracePeriodSeconds`.

## Configuration

`k8s/configmap.yaml`, `message-service-config`:

| Variable | Value | Purpose |
| :--- | :--- | :--- |
| `OTEL_TRACES_URL` | `http://otel-collector.observability.svc.cluster.local:4318/v1/traces` | Where the SDK posts spans |
| `OTEL_TRACES_SAMPLER` | `parentbased_traceidratio` | Sample by ratio; children follow the parent's decision |
| `OTEL_TRACES_SAMPLER_ARG` | `"0.1"` | 10% of new traces |

The sampler uses the SDK's standard env vars, which the Python SDK reads natively, so no code parses
them - the ratio can be changed by editing the ConfigMap and restarting the pods. 10% is deliberate:
a k6 run against a 5Gi PVC with 3-day retention (see `k8s/observability/openobserve-values.yaml`)
would otherwise fill it quickly. Set `"1.0"` temporarily when debugging, and expect to need a burst
of ~50+ requests to see anything at 0.1.

## Collector

`k8s/observability/config/otel-collector.yaml` gained a `traces` pipeline next to the existing
`metrics` one:

```yaml
extensions:
  basicauth/openobserve:
    client_auth:
      username: root@example.com
      password: ${env:OPENOBSERVE_PASSWORD}

exporters:
  otlphttp/openobserve:
    endpoint: http://openobserve.observability.svc.cluster.local:5080/api/default
    auth:
      authenticator: basicauth/openobserve

service:
  extensions: [basicauth/openobserve]
  pipelines:
    traces:
      receivers: [otlp]
      processors: [batch]
      exporters: [otlphttp/openobserve]
```

- The `otlphttp` exporter appends `/v1/traces` itself, unlike the services' exporter (above).
- `OPENOBSERVE_USER` / `OPENOBSERVE_PASSWORD` come from `secretKeyRef` in
  `otel-collector-deployment.yaml`, pointing at the `openobserve-remote-write-credentials` Secret
  (keys `username`, `password`) that Prometheus' `remote_write` also uses — built by External
  Secrets from GCP Secret Manager, so the credentials stay out of the ConfigMap and there is one
  place (1Password -> `gke-secrets-seed.sh`) to rotate them.
- The collector ConfigMap is generated with a content hash (`configMapGenerator` in
  `k8s/observability/kustomization.yaml`), so a merged config change rolls the collector by itself.

## Verification

**Collector → OpenObserve (verified live).** A synthetic OTLP span posted to the collector appeared
in OpenObserve as `tracing-smoke-test / smoke-span`, with no export errors in the collector log:

```bash
kubectl port-forward -n observability svc/otel-collector 14318:4318 &
curl -X POST localhost:14318/v1/traces -H 'Content-Type: application/json' -d '{
  "resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"tracing-smoke-test"}}]},
  "scopeSpans":[{"scope":{"name":"smoke"},"spans":[{"traceId":"5b8efff798038103d269b633813fc60c",
  "spanId":"eee19b7ec3c1b174","name":"smoke-span","kind":2,
  "startTimeUnixNano":"1700000000000000000","endTimeUnixNano":"1700000000050000000"}]}]}]}'
```

**Service → OpenObserve (verified live, 2026-09-23, on the earlier local cluster).** Running the real
image (3 to 6 replicas, `OTEL_TRACES_SAMPLER_ARG: "0.1"`), after running all five k6 scripts:

- **The service arrived:** 14,365 spans for `service_name = message-service`.
- **Span names are bounded:** `GET /messages/{id}`, `PATCH /messages/{id}`, `POST /messages`,
  `DELETE /messages/{id}`, `GET /messages` (route templates, never raw paths), plus `SELECT` /
  `INSERT` / `UPDATE` / `DELETE` / `connect` and `hazelcast.get` / `hazelcast.set` /
  `hazelcast.delete`.
- **Health probes are excluded:** 0 spans with `health` in the operation name, despite kubelet
  probing every few seconds.
- **Spans nest as intended.** A `PATCH /messages/{id}` trace:

  ```
  - PATCH /messages/{id}          2757us  status 200
    - connect                      317us
    - UPDATE                       229us  UPDATE messages SET title=$1::VARCHAR, content=$2::VARCHAR, version=(messages.version + $3) ...
    - SELECT                       132us  SELECT messages.id, messages.title, ...
    - hazelcast.delete             351us
  ```

- **`cache.hit` is recorded on `hazelcast.get`:** `false` on 828 spans and `true` on 314 in that run,
  so cache-aside behaviour is visible in the trace.
- **No duplicate metrics:** the new pods exported none of the `http_server_*` /
  `db_client_connections_usage` series (see the no-op `MeterProvider` above).

To repeat the check:

```bash
kubectl get pods -n default -l app=message-service

# generate traffic (10% sampling), e.g. one of the k6 scripts, then:
U=$(kubectl get secret openobserve-root-credentials -n observability -o jsonpath='{.data.ZO_ROOT_USER_EMAIL}' | base64 -d)
P=$(kubectl get secret openobserve-root-credentials -n observability -o jsonpath='{.data.ZO_ROOT_USER_PASSWORD}' | base64 -d)
kubectl port-forward -n observability svc/openobserve 15080:5080 &
END=$(python3 -c "import time;print(int(time.time()*1e6))"); START=$((END-3600000000))
curl -s -u "$U:$P" -X POST "http://localhost:15080/api/default/_search?type=traces" \
  -H 'Content-Type: application/json' \
  -d "{\"query\":{\"sql\":\"select operation_name, count(*) as n from \\\"default\\\" where service_name = 'message-service' group by operation_name order by n desc\",\"start_time\":$START,\"end_time\":$END,\"from\":0,\"size\":30}}"
```

Expect rows for the route-templated request spans and the SQL / `hazelcast.*` spans. Or open
OpenObserve → Traces (`http://localhost:5080` via `./gke-port-forward.sh openobserve`).

## Gotchas

- **Stream stats lag.** `GET /api/default/streams?type=traces` reports `doc_num: 0` for the
  `default` stream even when spans are searchable (they are still in the in-memory table). Search
  instead of trusting the stat.
- **ArgoCD `selfHeal` reverts hand edits** to `k8s/configmap.yaml` and the deployment, and the
  service image comes from Artifact Registry via Image Updater. To test a tracing change on the cluster it
  has to be merged, or auto-sync paused.
- **Nothing shows up?** In order: pods still on the old image; sampler ratio too low for the
  amount of traffic; `OTEL_TRACES_URL` missing `/v1/traces`; collector log for export errors
  (`kubectl logs -n observability deploy/otel-collector`).
- **Logs** are not covered here - see `LOGS.md`. The app writes one JSON object per line to stdout,
  with `trace_id` / `span_id` when a span is active, so a log line can be matched to its trace.
