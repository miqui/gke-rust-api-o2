# GC Pareto Frontier Experiments

Plan for exploring **Pareto frontiers of JVM garbage-collection settings** on this service: which
GC configurations are *non-dominated* when trading tail latency against CPU cost, memory footprint
and startup time. Nothing here is implemented yet; this records the ideas, the chosen first
experiments and how the work is organised.

## Why this service is a good test bed

- **The current GC is implicit.** No GC flag is set anywhere. The only JVM options are
  `-XX:MaxRAMPercentage=75 -XX:+ExitOnOutOfMemoryError` (`Dockerfile`), with a 768Mi memory limit
  and a 1500m CPU limit (`k8s/deployment.yaml`). With under ~1792MB of memory, JDK 21 treats the
  container as a client-class machine and selects **SerialGC**. Confirm with
  `kubectl exec ... -- jcmd 1 VM.flags | tr ' ' '\n' | grep Use.*GC`. Making this visible is
  already a finding.
- **GC cost turns into replicas.** The HPA (`k8s/hpa.yaml`) scales on CPU at 70% of a **250m
  request**, so GC CPU directly changes how many pods the service needs.
- **The tooling already exists:** k6 gRPC scenarios (`k6-*.js`), Prometheus/Grafana with
  Micrometer JVM metrics, short-lived GKE clusters, and Argo CD for rolling out each variant.

## Objectives (pick 2-3 per frontier)

| Axis | Metric | Source |
| --- | --- | --- |
| Tail latency | p99 / p99.9 of gRPC calls | k6 summary (`--summary-export`) |
| Throughput | sustained RPS at < 1% errors | k6 |
| CPU cost | CPU-seconds per 1k requests | `container_cpu_usage_seconds_total` / request count |
| Memory footprint | container working set; smallest limit without OOM | `container_memory_working_set_bytes` |
| GC pauses | max / p99 pause, total pause time | `jvm_gc_pause_milliseconds_*` (divide `_sum` by 1000 for seconds), `-Xlog:gc*` |
| Startup | time to readiness | pod `Ready` condition timestamp minus start time |

A configuration is on the frontier if no other configuration is at least as good on every
chosen axis and strictly better on one.

## Experiment backlog

Ordered cheap to ambitious. **Experiments 1 and 2 come first.**

1. **Collector shootout** at fixed resources (768Mi, 250m/1500m): Serial (implicit and explicit),
   Parallel, G1, generational ZGC, Shenandoah. Frontier: p99 vs CPU-seconds per 1k requests.
2. **Memory-limit sweep** per collector: limit in {512, 768, 1024, 1536}Mi x `MaxRAMPercentage`
   in {50, 65, 75, 85}. Frontier: footprint vs p99, i.e. "the smallest pod that meets the SLO".
3. **CPU limit x concurrent collectors:** sweep the CPU limit 500m-2000m and
   `-XX:ActiveProcessorCount`. Concurrent collectors (G1/ZGC/Shenandoah) need spare cores and can
   lose to Serial when throttled.
4. **Fleet-level frontier:** HPA enabled. Total cluster CPU and memory vs p99, e.g. 3 large G1
   pods vs 6 small Serial pods.
5. **Knobs within one collector:** G1 `MaxGCPauseMillis`, ZGC `SoftMaxHeapSize` / uncommit,
   Parallel `GCTimeRatio`. Each knob traces its own curve.
6. **Workload sensitivity:** the same variants under `k6-retrieve-messages.js` (read and
   cache-heavy) and `k6-create-messages.js` (write-heavy). Shows how the frontier moves with
   the workload.
7. **Automated search:** a driver (e.g. Python + Optuna multi-objective / NSGA-II) proposes flags,
   rolls them out, runs k6, scrapes Prometheus and keeps the non-dominated set.

## How the work is organised

### `main` is the baseline

`main` is not modified for experiments. It defines the baseline GC configuration running today
(implicit Serial, 768Mi, 250m/1500m). The baseline overlay keeps those GC settings but uses one
replica with the HPA removed, matching the capacity of experiments 1-3. Tag `main` before the first
run (`gc-baseline-v1`) so every result can cite the exact commit it was measured against.

### One branch for the harness, not one per variant

GC settings are runtime configuration, not code: every variant runs the **same image**. A branch
per variant would require re-pointing Argo CD's `targetRevision` for each run. The branches would
drift from each other, and any unrelated change on one of them would contaminate the comparison.
Instead, all variants live side by side on a single branch (`exp/gc-pareto`), which merges through
a PR like any other work:

```
k8s/experiments/gc/
  baseline/            # main GC settings; one replica and no HPA to match variants
  serial-explicit/
  parallel/
  g1/
  zgc/
  shenandoah/
  mem-512-p75/ ...     # experiment 2 sweep points
experiments/gc/results/<variant>-<timestamp>.json
scripts/gc-run.sh
scripts/gc-frontier.py
```

Each overlay uses `../../..` (the `k8s/` base) as its resource and patches only:

- **`JDK_JAVA_OPTIONS`** in `message-service-config`, e.g.
  `-XX:+UseZGC -XX:+ZGenerational -Xlog:gc*:stdout:time,uptime,level,tags`. The `java` launcher
  reads this variable in addition to the Dockerfile's `JAVA_TOOL_OPTIONS`, so the heap percentage
  and OOM behaviour stay in place and no image rebuild is needed. (For experiment 2 the overlay
  also sets `-XX:MaxRAMPercentage`. Command-line options win over `JAVA_TOOL_OPTIONS`.)
- **`resources`** of the `message-service` container, for the memory and CPU sweeps.
- For experiments 1-3: **`replicas: 1`** and the HPA removed, so autoscaling doesn't hide the GC
  effect.

### Run script (`scripts/gc-run.sh <variant>`)

1. Point the `springboot-grpc-o2` Application's `spec.source.targetRevision` at the experiment
   branch (`exp/gc-pareto`) and its `spec.source.path` at `k8s/experiments/gc/<variant>`. The
   Application tracks `main`, where the overlays don't exist, so the path alone is not enough.
2. Wait for the rollout and for readiness, and record the startup time.
3. Warm up: 2 minutes of load, discarded (JIT and heap sizing settle).
4. Measure: a fixed k6 scenario at a fixed arrival rate (`constant-arrival-rate`, so slower
   variants can't reduce the offered load), 10 minutes.
5. Scrape Prometheus for the window and write `experiments/gc/results/<variant>-<ts>.json` with the
   variant, git SHA, image digest, k6 summary and metrics.
6. Repeat each variant at least 3 times. Report the median and spread; frontier points are
   medians.

`scripts/gc-frontier.py` reads all result files, computes the non-dominated set for the chosen
axes and plots it, with the baseline highlighted.

### Comparability rules

- **Same image for every run.** Pause Argo CD Image Updater during a sweep so a new build can't
  roll out mid-experiment. Pinning a digest in the overlay is not an equivalent alternative:
  Image Updater writes its override into the child Application's `spec.source.kustomize`, which
  can supersede the overlay's pin.
- **Argo CD self-heal:** the root Application (`k8s/argocd/root-application.yaml`) self-heals its
  child Applications and only ignores `/spec/source/kustomize`. It would therefore revert a change
  to `/spec/source/path`. The root Application tracks `main`, so an `ignoreDifferences` edit on the
  experiment branch has no effect. Either add `/spec/source/path` (and
  `/spec/source/targetRevision`) to the root Application's `ignoreDifferences` on `main`, or turn
  off auto-sync on the root app for the duration of a sweep.
- **Fixed load source:** run k6 from the same machine and network each time, or better, as an
  in-cluster Job, so the public load balancer and home network don't add noise to p99.
- **Same node type** (`e2-standard-2`), with one service pod per node (anti-affinity is already
  set). Note the node in the result.
- Prometheus scrapes every 15s; keep measurement windows much longer than that.

## Porting to the Quarkus fork

The plan is meant to carry over to a Quarkus-based fork of this API almost unchanged: the same
proto contract, k6 scenarios, overlays and run script. Differences to account for:

- **JVM mode:** the same GC flags apply. Quarkus' default container images use `JAVA_OPTS_APPEND`
  (run-java.sh) rather than a plain `java` entrypoint, so the overlays should patch that variable
  instead of `JDK_JAVA_OPTIONS`.
- **Native mode (GraalVM / Mandrel):** a different GC menu: Serial (default) and Epsilon in
  GraalVM CE/Mandrel, G1 only in Oracle GraalVM. Heap sizing uses `-Xmx` / `-Xmn` at run time,
  not `MaxRAMPercentage`. Native should show up as separate points on the frontier, with much
  better startup and footprint and possibly lower peak throughput.
- **Metrics:** Micrometer via `quarkus-micrometer-registry-prometheus` exposes the same
  `jvm_gc_*` names in JVM mode. Native mode reports fewer GC metrics, so rely on container metrics
  and k6 there.
- **Cross-framework frontier:** with both services measured the same way, Spring Boot (JVM) vs
  Quarkus (JVM) vs Quarkus (native) can be plotted on one chart.

## Next steps

1. Tag `main` as `gc-baseline-v1`, verify the implicit SerialGC on a running pod.
2. On `exp/gc-pareto`: the overlays for experiment 1, the root-app `ignoreDifferences` change and
   `gc-run.sh`.
3. Agree on the fixed load profile (scenario, arrival rate, durations) and the SLO used to judge
   experiment 2.
4. Run experiment 1, then experiment 2. Write up the frontiers here or in a results document.
