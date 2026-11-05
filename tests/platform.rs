//! Platform behaviour: migrations (incl. adopting a Flyway-created schema), seeding, health,
//! reflection and the management HTTP endpoints.

mod common;

use common::{CacheMode, TestEnv, infra};
use message_service::{
    cache::MessageCache,
    management::{self, MgmtState},
    metrics::Metrics,
    migrate, seed,
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::sync::{Arc, atomic::AtomicBool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tonic::transport::Channel;
use uuid::Uuid;

macro_rules! env {
    ($mode:expr) => {
        match TestEnv::start($mode).await {
            Some(t) => t,
            None => return,
        }
    };
}

async fn blank_db() -> Option<PgPool> {
    let infra = infra()?;
    let name = format!("t_{}", Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{}/postgres", infra.pg_base))
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    Some(
        PgPoolOptions::new()
            .max_connections(8)
            .connect(&format!("{}/{name}", infra.pg_base))
            .await
            .unwrap(),
    )
}

const V1: &str = include_str!("../migrations/V1__initial_schema.sql");
const V2: &str = include_str!("../migrations/V2__author_message_pagination_index.sql");

async fn flyway_history(pool: &PgPool, versions: &[&str]) {
    sqlx::raw_sql(
        "CREATE TABLE flyway_schema_history (installed_rank INT NOT NULL PRIMARY KEY, \
         version VARCHAR(50), description VARCHAR(200) NOT NULL, type VARCHAR(20) NOT NULL, \
         script VARCHAR(1000) NOT NULL, checksum INT, installed_by VARCHAR(100) NOT NULL, \
         installed_on TIMESTAMP NOT NULL DEFAULT now(), execution_time INT NOT NULL, \
         success BOOLEAN NOT NULL)",
    )
    .execute(pool)
    .await
    .unwrap();
    for (i, v) in versions.iter().enumerate() {
        sqlx::query(
            "INSERT INTO flyway_schema_history VALUES ($1, $2, 'x', 'SQL', 'V__x.sql', 1, 'flyway', now(), 1, true)",
        )
        .bind(i as i32 + 1)
        .bind(*v)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn table_exists(pool: &PgPool, name: &str) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn migrations_apply_once_and_are_idempotent() {
    let Some(pool) = blank_db().await else { return };
    assert_eq!(migrate::run(&pool).await.unwrap(), vec![1, 2]);
    assert!(table_exists(&pool, "authors").await && table_exists(&pool, "messages").await);
    assert!(migrate::run(&pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_migration_runs_do_not_collide() {
    let Some(pool) = blank_db().await else { return };
    // Not tokio::spawn: sqlx's `&mut PgConnection` executor trips a rustc HRTB limitation when
    // the migration future must be `Send`; polling them concurrently in one task is enough.
    let runs = (0..6).map(|_| migrate::run(&pool));
    let mut applied = vec![];
    for r in futures::future::join_all(runs).await {
        applied.extend(r.unwrap());
    }
    applied.sort();
    assert_eq!(
        applied,
        vec![1, 2],
        "each migration applied by exactly one replica"
    );
}

#[tokio::test]
async fn flyway_created_schema_is_adopted_without_touching_it() {
    let Some(pool) = blank_db().await else { return };
    // Exactly what the Java service's Flyway leaves behind.
    sqlx::raw_sql(V1).execute(&pool).await.unwrap();
    sqlx::raw_sql(V2).execute(&pool).await.unwrap();
    flyway_history(&pool, &["1", "2"]).await;
    sqlx::query("INSERT INTO authors (name, email) VALUES ('keep', 'keep@x.org')")
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        migrate::run(&pool).await.unwrap().is_empty(),
        "nothing left to apply"
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM authors")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "existing data untouched");
    let hist: i64 = sqlx::query_scalar("SELECT count(*) FROM flyway_schema_history")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hist, 2, "flyway_schema_history is never written to");
}

#[tokio::test]
async fn flyway_partially_applied_schema_only_gets_the_missing_migration() {
    let Some(pool) = blank_db().await else { return };
    sqlx::raw_sql(V1).execute(&pool).await.unwrap();
    flyway_history(&pool, &["1"]).await;
    assert_eq!(migrate::run(&pool).await.unwrap(), vec![2]);
}

#[tokio::test]
async fn failed_flyway_rows_are_not_treated_as_applied() {
    let Some(pool) = blank_db().await else { return };
    flyway_history(&pool, &[]).await;
    sqlx::query("INSERT INTO flyway_schema_history VALUES (1, '1', 'x', 'SQL', 'V1__x.sql', 1, 'f', now(), 1, false)")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(migrate::run(&pool).await.unwrap(), vec![1, 2]);
}

#[tokio::test]
async fn seed_creates_one_welcome_message_even_when_racing() {
    let Some(pool) = blank_db().await else { return };
    migrate::run(&pool).await.unwrap();
    let runs: Vec<_> = (0..6)
        .map(|_| {
            let p = pool.clone();
            tokio::spawn(async move { seed::seed(&p).await.unwrap() })
        })
        .collect();
    let mut created = 0;
    for r in runs {
        created += usize::from(r.await.unwrap());
    }
    assert_eq!(created, 1);
    let (title, email): (String, String) = sqlx::query_as(
        "SELECT m.title, a.email FROM messages m JOIN authors a ON a.id = m.author_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (title.as_str(), email.as_str()),
        (seed::WELCOME_TITLE, seed::SYSTEM_EMAIL)
    );
    assert!(!seed::seed(&pool).await.unwrap(), "second run is a no-op");
}

#[tokio::test]
async fn seed_skips_a_database_that_already_has_messages() {
    let Some(pool) = blank_db().await else { return };
    migrate::run(&pool).await.unwrap();
    sqlx::query("INSERT INTO authors (name, email) VALUES ('a', 'a@x.org')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO messages (title, content, author_id) SELECT 't', 'c', id FROM authors",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(!seed::seed(&pool).await.unwrap());
}

#[tokio::test]
async fn grpc_health_and_reflection_are_served() {
    let t = env!(CacheMode::Disabled);
    let channel = Channel::from_shared(format!("http://{}", t.grpc_addr))
        .unwrap()
        .connect()
        .await
        .unwrap();

    let mut health = tonic_health::pb::health_client::HealthClient::new(channel.clone());
    for service in ["", "message.v1.MessageService", "message.v1.AuthorService"] {
        let r = health
            .check(tonic_health::pb::HealthCheckRequest {
                service: service.into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            r.status,
            tonic_health::pb::health_check_response::ServingStatus::Serving as i32
        );
    }

    use tonic_reflection::pb::v1::{
        ServerReflectionRequest, server_reflection_client::ServerReflectionClient,
        server_reflection_request::MessageRequest, server_reflection_response::MessageResponse,
    };
    let mut refl = ServerReflectionClient::new(channel);
    let req = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    let mut stream = refl
        .server_reflection_info(tokio_stream::iter(vec![req]))
        .await
        .unwrap()
        .into_inner();
    let resp = stream.message().await.unwrap().unwrap();
    let Some(MessageResponse::ListServicesResponse(list)) = resp.message_response else {
        panic!("unexpected reflection response")
    };
    let names: Vec<_> = list.service.into_iter().map(|s| s.name).collect();
    for want in [
        "message.v1.MessageService",
        "message.v1.AuthorService",
        "grpc.health.v1.Health",
    ] {
        assert!(
            names.iter().any(|n| n == want),
            "{want} missing from {names:?}"
        );
    }
}

/// Minimal HTTP/1.1 GET against the management server (no HTTP client dependency).
async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    let status = buf.split(' ').nth(1).unwrap().parse().unwrap();
    let body = buf.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    (status, body)
}

async fn serve_mgmt(
    pool: PgPool,
    cache: MessageCache,
    metrics: Metrics,
    down: Arc<AtomicBool>,
) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = management::router(MgmtState {
        pool,
        cache,
        metrics,
        shutting_down: down,
    });
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

#[tokio::test]
async fn management_endpoints() {
    let mut t = env!(CacheMode::Redis);
    let down = Arc::new(AtomicBool::new(false));
    let addr = serve_mgmt(
        t.pool.clone(),
        t.cache.clone(),
        t.metrics.clone(),
        down.clone(),
    )
    .await;
    t.author("m").await;

    assert_eq!(http_get(addr, "/livez").await.0, 200);
    let (code, body) = http_get(addr, "/readyz").await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("\"cache\":\"UP\""), "{body}");
    let (code, body) = http_get(addr, "/metrics").await;
    assert_eq!(code, 200);
    assert!(body.contains("grpc_server_seconds_bucket"), "{body}");
    assert_eq!(http_get(addr, "/nope").await.0, 404);

    // SIGTERM path: readiness flips, liveness stays up.
    down.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(http_get(addr, "/readyz").await.0, 503);
    assert_eq!(http_get(addr, "/livez").await.0, 200);
}

#[tokio::test]
async fn readiness_ignores_a_dead_redis_but_not_a_dead_database() {
    let t = env!(CacheMode::Broken);
    let down = Arc::new(AtomicBool::new(false));
    let addr = serve_mgmt(
        t.pool.clone(),
        t.cache.clone(),
        t.metrics.clone(),
        down.clone(),
    )
    .await;
    let (code, body) = http_get(addr, "/readyz").await;
    assert_eq!(code, 200, "Redis is non-fatal: {body}");
    assert!(body.contains("\"cache\":\"DOWN\""), "{body}");

    t.pool.close().await;
    let (code, body) = http_get(addr, "/readyz").await;
    assert_eq!(code, 503, "{body}");
}
