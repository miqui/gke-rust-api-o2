//! Redis read-through (cache-aside) cache for `GetMessage`, replacing the Hazelcast map.
//!
//! Semantics reproduced from `HazelcastMessageCache` / `Messages`:
//! * key = message id, value = the serialized protobuf `Message` (author included; a hit re-reads
//!   the author from the DB so author edits show up immediately);
//! * entries are evicted after a successful update/delete and after a stale-version conflict;
//!   Java has no TTL, here an optional safety-net TTL (`CACHE_TTL_SECONDS`, default 3600, 0 = off)
//!   bounds the damage of an eviction that Redis dropped;
//! * a per-key distributed lock (`SET NX PX`, up to 5 s wait, released by compare-and-delete)
//!   serialises cache fills and mutations through DB commit + eviction.
//!
//! Unlike the Java version, the cache is strictly best effort: any Redis failure (down, slow,
//! garbage entry) is logged, counted and skipped, so requests fall through to the database. A
//! tiny circuit breaker skips Redis for a few seconds after a failure so an outage costs one
//! timeout, not one per call.

use crate::{config::RedisConfig, metrics::Metrics, pb::Message};
use prost::Message as _;
use redis::{
    Client, ConnectionAddr, ConnectionInfo, IntoConnectionInfo, RedisConnectionInfo, Script,
    TlsCertificates,
    aio::{ConnectionManager, ConnectionManagerConfig},
};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use uuid::Uuid;

pub const LOCK_WAIT: Duration = Duration::from_secs(5);
/// Lease of a lock: expires on its own if the holder dies or its request is cancelled.
pub const LOCK_LEASE: Duration = Duration::from_secs(10);
const LOCK_RETRY: Duration = Duration::from_millis(20);
const BREAKER_OPEN: Duration = Duration::from_secs(5);

pub fn entry_key(id: Uuid) -> String {
    format!("messages:{id}")
}

pub fn lock_key(id: Uuid) -> String {
    format!("lock:messages:{id}")
}

/// Held lock; pass back to [`MessageCache::unlock`].
#[derive(Debug)]
pub struct LockGuard {
    key: String,
    token: String,
}

struct Inner {
    client: Client,
    conn: Mutex<Option<ConnectionManager>>,
    ttl: Option<Duration>,
    op_timeout: Duration,
    epoch: Instant,
    /// Millis since `epoch` until which Redis is skipped (0 = closed breaker).
    open_until: AtomicU64,
    metrics: Metrics,
}

#[derive(Clone)]
pub struct MessageCache {
    inner: Option<Arc<Inner>>,
}

impl MessageCache {
    /// A cache that never stores anything: every call is a miss / no-op.
    pub fn disabled() -> Self {
        MessageCache { inner: None }
    }

    /// Builds the client only; no connection is attempted here (connects lazily, retries on use).
    pub fn new(cfg: &RedisConfig, metrics: Metrics) -> anyhow::Result<Self> {
        let addr = if cfg.tls {
            ConnectionAddr::TcpTls {
                host: cfg.host.clone(),
                port: cfg.port,
                insecure: false,
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(cfg.host.clone(), cfg.port)
        };
        let mut redis_settings = RedisConnectionInfo::default();
        if let Some(pw) = &cfg.auth_string {
            redis_settings = redis_settings.set_password(pw);
        }
        let info: ConnectionInfo = addr
            .into_connection_info()?
            .set_redis_settings(redis_settings);
        let client = match (&cfg.ca_cert_path, cfg.tls) {
            (Some(path), true) => {
                let root_cert = std::fs::read(path)
                    .map_err(|e| anyhow::anyhow!("reading REDIS_CA_CERT_PATH {path}: {e}"))?;
                Client::build_with_tls(
                    info,
                    TlsCertificates {
                        client_tls: None,
                        root_cert: Some(root_cert),
                    },
                )?
            }
            _ => Client::open(info)?,
        };
        Ok(MessageCache {
            inner: Some(Arc::new(Inner {
                client,
                conn: Mutex::new(None),
                ttl: cfg.ttl,
                op_timeout: cfg.op_timeout,
                epoch: Instant::now(),
                open_until: AtomicU64::new(0),
                metrics,
            })),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Probe for `/readyz` reporting: PING within the op timeout. `None` when disabled.
    pub async fn ping(&self) -> Option<bool> {
        let inner = self.inner.as_ref()?;
        let pong = inner
            .run("ping", "ok", |mut c| async move {
                redis::cmd("PING").query_async::<String>(&mut c).await
            })
            .await;
        Some(pong.is_some())
    }

    /// Acquires the per-key lock, waiting up to [`LOCK_WAIT`]. `None` means "proceed without it":
    /// Redis is unavailable, or the wait timed out (callers then must not fill the cache).
    pub async fn lock(&self, id: Uuid) -> Option<LockGuard> {
        let inner = self.inner.as_ref()?;
        let key = lock_key(id);
        let token = Uuid::new_v4().to_string();
        let deadline = Instant::now() + LOCK_WAIT;
        let lease_ms = LOCK_LEASE.as_millis() as u64;
        loop {
            let (k, t) = (key.clone(), token.clone());
            let acquired = inner
                .run("lock", "ok", move |mut c| async move {
                    let r: Option<String> = redis::cmd("SET")
                        .arg(&k)
                        .arg(&t)
                        .arg("NX")
                        .arg("PX")
                        .arg(lease_ms)
                        .query_async(&mut c)
                        .await?;
                    Ok(r.is_some())
                })
                .await;
            match acquired {
                None => return None, // Redis unavailable: degrade
                Some(true) => return Some(LockGuard { key, token }),
                Some(false) if Instant::now() + LOCK_RETRY >= deadline => {
                    tracing::warn!(%id, "timed out acquiring message cache lock; continuing without it");
                    return None;
                }
                Some(false) => tokio::time::sleep(LOCK_RETRY).await,
            }
        }
    }

    /// Releases a lock only if it is still ours (compare-and-delete).
    pub async fn unlock(&self, guard: Option<LockGuard>) {
        let (Some(inner), Some(g)) = (self.inner.as_ref(), guard) else {
            return;
        };
        inner
            .run("unlock", "ok", move |mut c| async move {
                Script::new(
                    "if redis.call('get', KEYS[1]) == ARGV[1] then \
                     return redis.call('del', KEYS[1]) else return 0 end",
                )
                .key(&g.key)
                .arg(&g.token)
                .invoke_async::<i64>(&mut c)
                .await
            })
            .await;
    }

    pub async fn get(&self, id: Uuid) -> Option<Message> {
        let inner = self.inner.as_ref()?;
        let key = entry_key(id);
        let raw = inner
            .run_outcome("get", move |mut c| async move {
                let raw: Option<Vec<u8>> = redis::cmd("GET").arg(&key).query_async(&mut c).await?;
                let outcome = if raw.is_some() { "hit" } else { "miss" };
                Ok((raw, outcome))
            })
            .await??;
        match Message::decode(raw.as_slice()) {
            Ok(m) => Some(m),
            Err(e) => {
                // A corrupt entry is a miss; the caller refills it from the DB.
                tracing::warn!(%id, error = %e, "unreadable cache entry; treating as miss");
                None
            }
        }
    }

    pub async fn put(&self, message: &Message) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        let Ok(id) = Uuid::parse_str(&message.id) else {
            return;
        };
        let (key, value, ttl) = (entry_key(id), message.encode_to_vec(), inner.ttl);
        inner
            .run("set", "ok", move |mut c| async move {
                let mut cmd = redis::cmd("SET");
                cmd.arg(&key).arg(value);
                if let Some(ttl) = ttl {
                    cmd.arg("EX").arg(ttl.as_secs().max(1));
                }
                cmd.query_async::<()>(&mut c).await
            })
            .await;
    }

    pub async fn evict(&self, id: Uuid) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        let key = entry_key(id);
        inner
            .run("delete", "ok", move |mut c| async move {
                redis::cmd("DEL").arg(&key).query_async::<i64>(&mut c).await
            })
            .await;
    }
}

impl Inner {
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn breaker_open(&self) -> bool {
        self.open_until.load(Ordering::Relaxed) > self.now_ms()
    }

    fn trip(&self) {
        self.open_until.store(
            self.now_ms() + BREAKER_OPEN.as_millis() as u64,
            Ordering::Relaxed,
        );
    }

    async fn connection(&self) -> Result<ConnectionManager, String> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = slot.as_ref() {
            return Ok(c.clone());
        }
        let cfg = ConnectionManagerConfig::new()
            .set_number_of_retries(1)
            .set_connection_timeout(Some(self.op_timeout))
            .set_response_timeout(Some(self.op_timeout));
        let cm = tokio::time::timeout(
            self.op_timeout * 2,
            ConnectionManager::new_with_config(self.client.clone(), cfg),
        )
        .await
        .map_err(|_| "connect timed out".to_owned())?
        .map_err(|e| e.to_string())?;
        *slot = Some(cm.clone());
        Ok(cm)
    }

    async fn run<T, F, Fut>(&self, op: &'static str, outcome: &'static str, f: F) -> Option<T>
    where
        F: FnOnce(ConnectionManager) -> Fut,
        Fut: Future<Output = redis::RedisResult<T>>,
    {
        self.run_outcome(op, move |c| {
            let fut = f(c);
            async move { fut.await.map(|v| (v, outcome)) }
        })
        .await
    }

    /// Runs one cache operation with timeout, metrics and failure isolation. `None` = skipped or
    /// failed (already logged and counted).
    async fn run_outcome<T, F, Fut>(&self, op: &'static str, f: F) -> Option<T>
    where
        F: FnOnce(ConnectionManager) -> Fut,
        Fut: Future<Output = redis::RedisResult<(T, &'static str)>>,
    {
        if self.breaker_open() {
            return None;
        }
        let start = Instant::now();
        let result: Result<(T, &'static str), String> = async {
            let conn = self.connection().await?;
            match tokio::time::timeout(self.op_timeout, f(conn)).await {
                Err(_) => Err("timed out".to_owned()),
                Ok(Err(e)) => Err(e.to_string()),
                Ok(Ok(v)) => Ok(v),
            }
        }
        .await;
        let secs = start.elapsed().as_secs_f64();
        match result {
            Ok((value, outcome)) => {
                self.metrics.record_cache(op, outcome, secs);
                Some(value)
            }
            Err(e) => {
                self.metrics.record_cache(op, "error", secs);
                self.metrics.record_cache_error(op);
                self.trip();
                tracing::warn!(operation = op, error = %e, "cache operation failed; degrading to the database");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_layout() {
        let id = Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap();
        assert_eq!(
            entry_key(id),
            "messages:123e4567-e89b-12d3-a456-426614174000"
        );
        assert_eq!(
            lock_key(id),
            "lock:messages:123e4567-e89b-12d3-a456-426614174000"
        );
    }

    #[tokio::test]
    async fn disabled_cache_is_a_noop() {
        let c = MessageCache::disabled();
        let id = Uuid::new_v4();
        assert!(!c.is_enabled());
        assert!(c.lock(id).await.is_none());
        assert!(c.get(id).await.is_none());
        c.put(&Message {
            id: id.to_string(),
            ..Default::default()
        })
        .await;
        c.evict(id).await;
        assert!(c.ping().await.is_none());
    }
}
