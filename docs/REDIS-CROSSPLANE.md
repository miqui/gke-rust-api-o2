# Redis (Memorystore) via Crossplane

The message-service's cache is a **Memorystore for Redis** instance, provisioned by Crossplane
exactly like the Cloud SQL Postgres: the app declares a `RedisInstance` (a platform API) and a
Composition turns it into the managed GCP resource and a connection Secret.

| Piece | File |
|---|---|
| API (namespaced XRD, `platform.miqui.dev/v1alpha1` `RedisInstance`) | `k8s/platform/redis-xrd.yaml` |
| Composition (`function-patch-and-transform` + `function-auto-ready`) | `k8s/platform/redis-composition.yaml` |
| Provider `provider-gcp-redis` (pinned) + runtime config (Workload Identity) | `k8s/platform/crossplane-packages.yaml`, `crossplane-runtime-config.yaml` |
| The instance the app requests | `k8s/redis.yaml` |
| Network policy (app -> `10.16.0.0/20:6379` only) | `k8s/networkpolicies.yaml` |

Argo CD sync waves: providers/functions (wave 0, `platform` app) -> XRD + Composition (wave 1,
`platform` app) -> `RedisInstance` (the `rust-api-o2` app, last).

## Provider

`xpkg.crossplane.io/crossplane-contrib/provider-gcp-redis:v3.0.0` — a sub-provider of
[crossplane-contrib/provider-upjet-gcp](https://github.com/crossplane-contrib/provider-upjet-gcp)
(`cmd/provider/redis`), pinned to the same release as `provider-gcp-sql`. Managed resource:
`redis.gcp.m.upbound.io/v1beta1` `Instance` (namespaced; the `.m.` API group is the Crossplane v2
flavour, same as `sql.gcp.m.upbound.io` for Cloud SQL). Field names in the Composition were checked
against the v3.0.0 CRD (`package/crds/redis.gcp.m.upbound.io_instances.yaml`). Later releases exist
(v3.1.0); bump both providers together.

## What the Composition sets

- `connectMode: PRIVATE_SERVICE_ACCESS` on `projects/k8s-dev-412419/global/networks/dev-vpc`, with
  `reservedIpRange: cloudsql-psa-range` — the same private-service-access range/peering
  `gke-deploy.sh` creates for Cloud SQL (so the pod egress CIDR is the same `10.16.0.0/20`).
- `authEnabled: true` and `transitEncryptionMode: SERVER_AUTHENTICATION` (TLS required).
- `tier`, `memorySizeGb`, `version` come from the `RedisInstance` spec; defaults `BASIC`, `1`,
  `REDIS_7_2`. Use `tier: STANDARD_HA` for a replica with failover.
- `deletionProtection: false`, no persistence — dev-cluster trade-off, a cache.
- The Memorystore instance ID is `<RedisInstance name>-<random suffix>`.

## Connection Secret

Two Secrets in the same namespace (provider-upjet-gcp has no custom connection-details config for
`google_redis_instance`; upjet publishes only *sensitive* Terraform attributes, as `attribute.<name>`):

| Secret | Written by | Keys |
|---|---|---|
| `redis-connection-auth` (`<connectionSecretName>-auth`) | the provider (`writeConnectionSecretToRef`) | `attribute.auth_string` |
| `redis-connection` (`spec.connectionSecretName`) | the Composition's `endpoint` resource (plain `v1/Secret`, rendered from the Instance's observed `status.atProvider.host/port/serverCaCerts[0].cert`) | `host`, `port`, `ca.crt` |

The Deployment maps them to:

| Env / file | Source |
|---|---|
| `REDIS_HOST` | `redis-connection` `host` |
| `REDIS_AUTH_STRING` | `redis-connection-auth` `attribute.auth_string` (required, no `optional`) |
| `REDIS_TLS_VERIFY_HOSTNAME=false` | ConfigMap; Memorystore is reached by IP and its cert has no IP SAN, so only the hostname check is skipped (chain still verified against the instance CA; see docs/RUST-SERVICE.md, "Redis TLS and Memorystore") |
| `REDIS_PORT=6379`, `REDIS_TLS=true`, `REDIS_CA_CERT_PATH=/etc/redis-ca/ca.crt` | ConfigMap; the CA file is `redis-connection` `ca.crt`, mounted read-only |

`k8s/platform/crossplane-rbac-secrets.yaml` aggregates a ClusterRole so Crossplane may create the composed Secret.

The `wait-for-redis` init container holds the pod until `host:6379` accepts connections.

## GCP prerequisites (done by `gke-deploy.sh`)

```sh
gcloud services enable redis.googleapis.com
gcloud projects add-iam-policy-binding k8s-dev-412419 \
  --member=serviceAccount:crossplane-gcp@k8s-dev-412419.iam.gserviceaccount.com \
  --role=roles/redis.admin --condition=None
gcloud iam service-accounts add-iam-policy-binding crossplane-gcp@k8s-dev-412419.iam.gserviceaccount.com \
  --role=roles/iam.workloadIdentityUser \
  --member="serviceAccount:k8s-dev-412419.svc.id.goog[crossplane-system/provider-gcp-redis]"
```

(`roles/compute.networkUser`, already granted for Cloud SQL, covers the VPC attachment.)

## Verify

```sh
kubectl get providers.pkg.crossplane.io                 # provider-gcp-redis INSTALLED + HEALTHY
kubectl get xrd redisinstances.platform.miqui.dev
kubectl get redisinstance                                # SYNCED/READY, INSTANCE, HOST (~3-5 min)
kubectl describe redisinstance messagecache
kubectl get instances.redis.gcp.m.upbound.io            # the managed resource
kubectl get managed -n default
kubectl get secret redis-connection redis-connection-auth -o json | jq '.items[].data | keys'
gcloud redis instances list --region=us-central1
```

## Teardown

`./gke-teardown.sh` deletes all `RedisInstance`s (Crossplane deletes the Memorystore instance) and
waits for the managed resources to disappear *before* deleting the cluster, then removes any
leftover instance labelled `platform=gke-rust-api-o2` and finally the private-service-access
peering (which would fail while Memorystore or Cloud SQL still used it). Manual:
`kubectl delete redisinstance messagecache`.

## Unverified / watch for on first apply

- Not verifiable without a cluster: that the provider's secret key is exactly `attribute.auth_string`
  (upjet convention; the provider repo has no Redis-specific connection-details override) and that the
  composed `v1/Secret` is accepted by Crossplane's RBAC. Check with
  `kubectl get secret redis-connection-auth redis-connection -o json | jq '.items[].data|keys'`
  and adjust the Deployment key if it differs. The single-Secret alternative (host/port/authString/ca.crt)
  needs `function-go-templating` reading the Instance's observed connection details, which Crossplane v2
  does not reliably pass for namespaced XRs - hence two Secrets.
- `provider-gcp-redis` v3.0.0 was verified to exist in the provider repo (tag + `cmd/provider/redis`),
  not pulled from the registry.
