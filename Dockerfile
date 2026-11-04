# syntax=docker/dockerfile:1
# Multi-stage Dockerfile for the Spring Boot gRPC API

# Stage 1: build the jar with the Maven wrapper. The dependency layer (pom + wrapper + policies) is
# separate from the source layer, so editing src/ doesn't re-download dependencies. Tests run in CI
# (`./mvnw clean verify`, which needs Docker for Testcontainers), not here.
FROM eclipse-temurin:21-jdk AS builder
WORKDIR /build
COPY mvnw pom.xml ./
COPY .mvn/ .mvn/
COPY quality/ quality/
RUN --mount=type=cache,target=/root/.m2 ./mvnw -B -q dependency:go-offline
COPY src/ src/
RUN --mount=type=cache,target=/root/.m2 ./mvnw -B -q -DskipTests package \
 && java -Djarmode=tools -jar target/message-service.jar extract --layers --launcher --destination /extracted

# Stage 2: runtime (JRE only). Spring Boot's layers, least to most frequently changing, so a code
# change only rebuilds the last small layer.
FROM eclipse-temurin:21-jre

# Numeric uid/gid 10001: k8s/deployment.yaml sets runAsUser: 10001 + runAsNonRoot, and the kubelet
# can only verify "non-root" for a numeric USER. Above 10000 so it can't collide with a host user.
RUN groupadd -g 10001 app && useradd -u 10001 -g 10001 -M -s /usr/sbin/nologin app

WORKDIR /app
COPY --from=builder /extracted/dependencies/ ./
COPY --from=builder /extracted/spring-boot-loader/ ./
COPY --from=builder /extracted/snapshot-dependencies/ ./
COPY --from=builder /extracted/application/ ./

# Container-aware heap (75% of the memory limit; the rest is metaspace, thread stacks, Netty direct
# buffers). Exit on OOM so the kubelet restarts a clean JVM. The root filesystem is read-only in
# the cluster: /tmp is an emptyDir.
ENV JAVA_TOOL_OPTIONS="-XX:MaxRAMPercentage=75 -XX:+ExitOnOutOfMemoryError -Djava.io.tmpdir=/tmp"

USER 10001:10001
# 9090: gRPC (h2c). 8081: actuator (kubelet probes, load-balancer health check).
EXPOSE 9090 8081

# exec form: java is PID 1 and gets SIGTERM from Kubernetes directly. Readiness goes DOWN, the gRPC
# server drains (spring.grpc.server.shutdown.grace-period), then the JVM exits (code 143 = SIGTERM).
# Flyway migrates at startup, serialised across replicas by its own Postgres advisory lock.
ENTRYPOINT ["java", "org.springframework.boot.loader.launch.JarLauncher"]
