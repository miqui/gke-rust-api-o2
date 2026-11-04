# Pod logs → OpenObserve

Container logs from the `default` namespace (message-service, postgres, hazelcast)
are tailed off the nodes by an OTel Collector DaemonSet and shipped to OpenObserve. Metrics
(`PROMETHEUS.md`) and traces (`TRACING.md`) already go there; this adds the third signal.

```
/var/log/pods/default_*/*/*.log ─► log-collector (DaemonSet, filelog) ─► OpenObserve
   (CRI files on each node)          OTLP/HTTP + basic auth               stream "pod_logs"
```

## Manifests (`k8s/observability/`)

| File | What |
| :--- | :--- |
| `config/log-collector.yaml` | `filelog` receiver + `container` parser → batch → `otlphttp/openobserve` |
| `log-collector-daemonset.yaml` | `otel/opentelemetry-collector-contrib:0.114.0`, one pod per api/db/cache node |
| `resourcequota.yaml` | `pods` raised from 20 to 30 (was 14/20 in use; the DaemonSet adds 5) |
| `../policies/exceptions/log-collector.yaml` | Kyverno `PolicyException` (see "Kyverno" below) |

It is a separate collector from `otel-collector` (the OTLP receiver Deployment): that one is a
central Deployment, this one has to run on every node to read the node's files.

## How it works

- **Scope: `default` namespace only** (`include: /var/log/pods/default_*/*/*.log`). Collecting every
  pod (argocd, kube-system, crossplane-system, …) would risk OpenObserve's 5Gi PVC and single-node
  MemTable — the same failure described in `PROMETHEUS.md` step 1. Widen it deliberately, not by
  accident. This also means the collector never reads its own logs (it is in `observability`).
- **`container` operator** parses the CRI line format (`<time> stdout F <message>`), re-joins lines
  the runtime split, and derives the pod/namespace/container from the file path. So no
  `k8sattributes` processor and no ClusterRole are needed for those labels.
- **`include_file_path: true` is required** by that operator. Without it every line fails with
  `failed to handle attribute mappings: type '<nil>' cannot be parsed as log path field` and
  nothing reaches OpenObserve (this was the first attempt's failure).
- **`start_at: end`**: a starting or restarted collector skips whatever was logged while it was
  down instead of replaying every file. There is no `file_storage` checkpointing, so restarts lose
  a small gap. Fine for a dev cluster; add `file_storage` if that matters.
- **Placement**: `nodeAffinity` on `workload In (api, db, cache)` — the nodes running `default`
  pods. The control-plane, observability and openobserve nodes get none.
- **Auth**: same `basicauth` extension and `openobserve-remote-write-credentials` Secret as the
  traces pipeline (`OPENOBSERVE_PASSWORD` env, key `password`).
- **Single stream**: the `stream-name: pod_logs` header puts everything in one stream (default
  would be `default`).

## Fields in OpenObserve

Each row of `pod_logs` has `body` (the log line), `k8s_namespace_name`, `k8s_pod_name`,
`k8s_pod_uid`, `k8s_container_name`, `k8s_container_restart_count`, `log_iostream`
(`stdout`/`stderr`), `logtag` (`F` full / `P` partial), `log_file_path`, and `_timestamp`.

```sql
select _timestamp, k8s_pod_name, body from "pod_logs"
where k8s_container_name = 'message-service' and body like '%error%'
order by _timestamp desc
```

## Kyverno

`observability` is audit-only, so nothing here is blocked, but the DaemonSet would show up in the
PolicyReports twice: a `hostPath` volume (`disallow-host-access`) and running as root
(`require-secure-container-context`). Root is required — on the GKE nodes `/var/log/pods` is
`root:root 0750` and the files are `0640`. `k8s/policies/exceptions/log-collector.yaml` exempts it
from those two audit rules, like `node-exporter.yaml` does; the rest of its `securityContext`
(no privilege escalation, drop ALL, RuntimeDefault seccomp, read-only root filesystem, read-only
mount) is still set. The exception is synced by ArgoCD from `main`.

## Application log format

message-service writes **one JSON object per line to stdout** (`app/logging.py`; uvicorn's and
Alembic's own logging is routed through the same formatter), so the CRI line the collector ships is
`<time> stdout F {"message": ..., "timestamp": ..., "level": ..., "logger": ..., ...}`. Fields:

| Field | Notes |
| :--- | :--- |
| `timestamp` | ISO-8601 UTC, millisecond precision |
| `level` | `INFO`, `WARNING`, `ERROR`, ... |
| `message` | The log message |
| `logger` | The Python logger name (`app.main`, `app.access`, `uvicorn.error`, `alembic.runtime.migration`, ...) |
| `trace_id`, `span_id` | Present when a span is active - copy `trace_id` into OpenObserve's Traces view to jump from a log line to its trace |
| extras | e.g. `method`, `route`, `status_code`, `duration_ms` on the per-request `app.access` line |

`app.access` logs one line per request, with the route *template*; `/health/*` probes are not
logged (uvicorn's own access log is off). An unhandled exception is logged with its stack trace
(and the active `trace_id`), while the client only ever gets a generic 500. Request bodies and
credentials are never logged.

## Verification (live, 2026-09-23)

On the earlier local cluster running the real image, after running the k6 scripts: `pod_logs` held
32,471 rows from message-service pods, of which 32,053 carried a `trace_id` in their JSON `body`,
and the startup line (`{"message": "message-service started", "api_docs_enabled": true, ...,
"logger": "app.main"}`) was searchable. Earlier (2026-09-21), marker lines written with
`kubectl exec … > /proc/1/fd/1` had been verified to arrive with the right pod/namespace labels, with
0 rows returned for `k8s_namespace_name <> 'default'`.

To repeat it:

```bash
kubectl exec -n default deploy/message-service -c message-service -- \
  sh -c 'echo "log-shipping-smoke-test from $HOSTNAME" > /proc/1/fd/1'

U=$(kubectl get secret openobserve-root-credentials -n observability -o jsonpath='{.data.ZO_ROOT_USER_EMAIL}' | base64 -d)
P=$(kubectl get secret openobserve-root-credentials -n observability -o jsonpath='{.data.ZO_ROOT_USER_PASSWORD}' | base64 -d)
kubectl port-forward -n observability svc/openobserve 15080:5080 &
END=$(python3 -c "import time;print(int(time.time()*1e6))"); START=$((END-900000000))
curl -s -u "$U:$P" -X POST "http://localhost:15080/api/default/_search?type=logs" \
  -H 'Content-Type: application/json' \
  -d "{\"query\":{\"sql\":\"select k8s_pod_name, body from \\\"pod_logs\\\" where body like '%smoke-test%'\",\"start_time\":$START,\"end_time\":$END,\"from\":0,\"size\":20}}"
```

## Gotchas

- **Config changes roll the DaemonSet** — the collector doesn't hot-reload, but its ConfigMap is
  generated with a content hash, so a merged config change rolls the pods by itself. Lines logged
  while a pod restarts are skipped (`start_at: end`).
- **Nothing arriving?** `kubectl logs -n observability ds/log-collector` — parse errors
  (`failed to process token`) and export errors (401/400 from OpenObserve) show there. An empty
  `pod_logs` stream with a healthy collector usually just means the pods haven't logged since it
  started.
- **Stream stats lag** — `GET /api/default/streams?type=logs` can show `doc_num: 0` while rows are
  already searchable; search instead.
- **ArgoCD-managed**: `k8s/observability/` is the `observability` Application - change it in git
  and merge; hand edits are reverted by self-heal.
- **Adding a namespace**: change the `include` glob. The DaemonSet runs on every node, so no
  scheduling change is needed.
