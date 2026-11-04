//! Shared integration-test harness: one Postgres + one Redis container per test binary
//! (testcontainers; override with `TEST_DATABASE_URL` / `TEST_REDIS_URL` when Docker is not
//! available), a fresh database per test, and the real gRPC server on an ephemeral port.
//!
//! Without Docker and without those variables the DB-backed tests print a notice and pass
//! vacuously; set `REQUIRE_INTEGRATION=1` (CI) to make that a failure instead.

#![allow(dead_code)]

use message_service::{
    cache::MessageCache,
    config::RedisConfig,
    metrics::Metrics,
    migrate,
    pb::{
        Author, CreateAuthorRequest, CreateMessageRequest, Message,
        author_service_client::AuthorServiceClient, message_service_client::MessageServiceClient,
    },
    server::{self, GrpcDeps},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{sync::OnceLock, time::Duration};
use testcontainers::{ImageExt, runners::AsyncRunner};
use tokio::sync::oneshot;
use tonic::{Code, Status, transport::Channel};
use tonic_types::StatusExt;
use uuid::Uuid;

pub struct Infra {
    /// `postgres://user:pass@host:port` (no database name).
    pub pg_base: String,
    pub redis_url: String,
}

static INFRA: OnceLock<Option<Infra>> = OnceLock::new();

pub fn infra() -> Option<&'static Infra> {
    INFRA
        .get_or_init(|| {
            if let Ok(pg) = std::env::var("TEST_DATABASE_URL") {
                let redis_url = std::env::var("TEST_REDIS_URL")
                    .unwrap_or_else(|_| "redis://127.0.0.1:1".into());
                return Some(Infra {
                    pg_base: pg.trim_end_matches('/').to_owned(),
                    redis_url,
                });
            }
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    let started = async {
                        let pg = testcontainers_modules::postgres::Postgres::default()
                            .with_tag("16-alpine")
                            .start()
                            .await?;
                        let redis = testcontainers_modules::redis::Redis::default()
                            .with_tag("7-alpine")
                            .start()
                            .await?;
                        let pg_port = pg.get_host_port_ipv4(5432).await?;
                        let redis_port = redis.get_host_port_ipv4(6379).await?;
                        Ok::<_, testcontainers::TestcontainersError>((
                            pg, redis, pg_port, redis_port,
                        ))
                    }
                    .await;
                    match started {
                        Ok((pg, redis, pg_port, redis_port)) => {
                            let _ = tx.send(Some(Infra {
                                pg_base: format!(
                                    "postgres://postgres:postgres@127.0.0.1:{pg_port}"
                                ),
                                redis_url: format!("redis://127.0.0.1:{redis_port}"),
                            }));
                            // Keep the containers alive for the life of the test process
                            // (testcontainers' reaper removes them afterwards).
                            let _keep = (pg, redis);
                            std::future::pending::<()>().await;
                        }
                        Err(e) => {
                            eprintln!(
                                "Docker unavailable ({e}); integration tests will be skipped"
                            );
                            let _ = tx.send(None);
                        }
                    }
                });
            });
            rx.recv().ok().flatten()
        })
        .as_ref()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    Redis,
    Disabled,
    /// Redis configured but nothing listens: every cache call fails.
    Broken,
}

pub struct TestEnv {
    pub pool: PgPool,
    pub db_name: String,
    pub messages: MessageServiceClient<Channel>,
    pub authors: AuthorServiceClient<Channel>,
    pub redis: Option<redis::aio::MultiplexedConnection>,
    pub metrics: Metrics,
    pub cache: MessageCache,
    pub db_url: String,
    pub grpc_addr: std::net::SocketAddr,
    _shutdown: oneshot::Sender<()>,
}

impl TestEnv {
    /// `None` = integration infrastructure unavailable (test is skipped).
    pub async fn start(mode: CacheMode) -> Option<TestEnv> {
        let Some(infra) = infra() else {
            assert!(
                std::env::var("REQUIRE_INTEGRATION").is_err(),
                "REQUIRE_INTEGRATION is set but neither Docker nor TEST_DATABASE_URL is available"
            );
            eprintln!("SKIPPED: no Docker / TEST_DATABASE_URL");
            return None;
        };
        let db_name = format!("t_{}", Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&format!("{}/postgres", infra.pg_base))
            .await
            .expect("admin connection");
        sqlx::query(&format!("CREATE DATABASE {db_name}"))
            .execute(&admin)
            .await
            .unwrap();
        let db_url = format!("{}/{db_name}", infra.pg_base);
        let pool = PgPoolOptions::new()
            .max_connections(20)
            .connect(&db_url)
            .await
            .unwrap();
        migrate::run(&pool).await.expect("migrations");

        let metrics = Metrics::new();
        let cfg = |host: &str, port: u16| RedisConfig {
            host: host.into(),
            port,
            auth_string: None,
            tls: false,
            ca_cert_path: None,
            tls_verify_hostname: true,
            ttl: Some(Duration::from_secs(3600)),
            op_timeout: Duration::from_millis(300),
        };
        let (cache, redis) = match mode {
            CacheMode::Disabled => (MessageCache::disabled(), None),
            CacheMode::Broken => (
                MessageCache::new(&cfg("127.0.0.1", 1), metrics.clone()).unwrap(),
                None,
            ),
            CacheMode::Redis => {
                let url = url::Url::parse(&infra.redis_url).unwrap();
                let c = cfg(url.host_str().unwrap(), url.port().unwrap());
                let raw = redis::Client::open(infra.redis_url.as_str())
                    .unwrap()
                    .get_multiplexed_async_connection()
                    .await
                    .unwrap();
                (MessageCache::new(&c, metrics.clone()).unwrap(), Some(raw))
            }
        };

        let (listener, addr) = server::bind(0).await.unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let deps = GrpcDeps {
            pool: pool.clone(),
            cache: cache.clone(),
            metrics: metrics.clone(),
        };
        tokio::spawn(async move {
            server::serve_grpc(listener, deps, async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });
        let channel = Channel::from_shared(format!("http://127.0.0.1:{}", addr.port()))
            .unwrap()
            .connect_timeout(Duration::from_secs(5))
            .connect()
            .await
            .expect("connect to in-process gRPC server");
        Some(TestEnv {
            pool,
            db_name,
            messages: MessageServiceClient::new(channel.clone()),
            authors: AuthorServiceClient::new(channel),
            redis,
            metrics,
            cache,
            db_url,
            grpc_addr: addr,
            _shutdown: tx,
        })
    }

    pub async fn author(&mut self, name: &str) -> Author {
        self.authors
            .create_author(CreateAuthorRequest {
                name: name.into(),
                email: format!("{}@example.com", Uuid::new_v4().simple()),
            })
            .await
            .expect("create author")
            .into_inner()
    }

    pub async fn message(&mut self, author: &Author, title: &str, content: &str) -> Message {
        self.messages
            .create_message(CreateMessageRequest {
                title: title.into(),
                content: content.into(),
                author_id: author.id.clone(),
            })
            .await
            .expect("create message")
            .into_inner()
    }

    pub async fn redis_get(&self, key: &str) -> Option<Vec<u8>> {
        let mut c = self.redis.clone().expect("redis mode");
        redis::cmd("GET")
            .arg(key)
            .query_async(&mut c)
            .await
            .unwrap()
    }

    pub async fn redis_ttl(&self, key: &str) -> i64 {
        let mut c = self.redis.clone().expect("redis mode");
        redis::cmd("TTL")
            .arg(key)
            .query_async(&mut c)
            .await
            .unwrap()
    }

    pub async fn redis_set(&self, key: &str, value: &[u8], px: Option<u64>) {
        let mut c = self.redis.clone().expect("redis mode");
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value);
        if let Some(px) = px {
            cmd.arg("PX").arg(px);
        }
        cmd.query_async::<()>(&mut c).await.unwrap();
    }

    pub async fn redis_del(&self, key: &str) {
        let mut c = self.redis.clone().expect("redis mode");
        redis::cmd("DEL")
            .arg(key)
            .query_async::<i64>(&mut c)
            .await
            .unwrap();
    }
}

/// `ErrorInfo.reason` of an error status.
pub fn reason(st: &Status) -> String {
    st.get_details_error_info()
        .expect("ErrorInfo detail")
        .reason
}

pub fn domain(st: &Status) -> String {
    st.get_details_error_info()
        .expect("ErrorInfo detail")
        .domain
}

/// (field, description) pairs of the `BadRequest` detail.
pub fn violations(st: &Status) -> Vec<(String, String)> {
    st.get_details_bad_request()
        .expect("BadRequest detail")
        .field_violations
        .into_iter()
        .map(|v| (v.field, v.description))
        .collect()
}

/// Asserts code + ErrorInfo reason (+ domain) of an error result.
#[track_caller]
pub fn assert_status<T: std::fmt::Debug>(r: Result<T, Status>, code: Code, why: &str) -> Status {
    let st = r.expect_err("expected an error status");
    assert_eq!(st.code(), code, "status: {st:?}");
    assert_eq!(reason(&st), why, "status: {st:?}");
    assert_eq!(domain(&st), "message-service.miqui.dev");
    st
}
