//! Configuration strictly from environment variables. Names mirror the Spring Boot service
//! (`application.yaml`, k8s/configmap.yaml, the `postgres-credentials` Secret) so the manifests
//! stay unchanged; the cache variables replace `HAZELCAST_*`.

use std::{collections::HashMap, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub user: String,
    pub password: String,
    /// libpq sslmode: disable, allow, prefer, require, verify-ca, verify-full.
    pub sslmode: String,
    pub pool_max: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisConfig {
    pub host: String,
    pub port: u16,
    pub auth_string: Option<String>,
    pub tls: bool,
    pub ca_cert_path: Option<String>,
    /// Safety-net TTL for cache entries; `None` = no expiry (the Java behaviour).
    pub ttl: Option<Duration>,
    /// Upper bound for any single cache round trip.
    pub op_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OtelConfig {
    pub export_enabled: bool,
    pub traces_url: String,
    pub sampling_probability: f64,
    pub pod_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub grpc_port: u16,
    pub management_port: u16,
    pub shutdown_grace: Duration,
    pub db: DbConfig,
    /// `None` when `REDIS_HOST` is unset: the service then runs cache-less (always DB).
    pub redis: Option<RedisConfig>,
    pub otel: OtelConfig,
    pub seed_enabled: bool,
    /// `json` (default) or `text`.
    pub log_format: String,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid configuration: {0}")]
pub struct ConfigError(String);

fn num<T: std::str::FromStr>(v: Option<&str>, key: &str, default: T) -> Result<T, ConfigError> {
    match v {
        None => Ok(default),
        Some(s) => s
            .parse()
            .map_err(|_| ConfigError(format!("{key}={s:?} is not a valid number"))),
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let vars: HashMap<String, String> = std::env::vars().collect();
        Self::from_map(&vars)
    }

    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self, ConfigError> {
        let get = |k: &str| vars.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
        let string = |k: &str, d: &str| get(k).unwrap_or(d).to_owned();
        let boolean = |k: &str, d: bool| -> Result<bool, ConfigError> {
            match get(k).map(str::to_ascii_lowercase).as_deref() {
                None => Ok(d),
                Some("true" | "1" | "yes" | "on") => Ok(true),
                Some("false" | "0" | "no" | "off") => Ok(false),
                Some(o) => Err(ConfigError(format!("{k}={o:?} is not a boolean"))),
            }
        };

        let db = DbConfig {
            host: string("DB_HOST", "localhost"),
            port: num(get("DB_PORT"), "DB_PORT", 5432)?,
            name: string("DB_NAME", "messagedb"),
            user: string("DB_USER", "message_app"),
            // Taken as-is (not trimmed): a password may legitimately start/end with spaces.
            password: vars
                .get("DB_PASSWORD")
                .cloned()
                .unwrap_or_else(|| "message_app".into()),
            sslmode: string("DB_SSLMODE", "prefer"),
            pool_max: num(get("DB_POOL_MAX"), "DB_POOL_MAX", 10)?,
        };

        let redis = match get("REDIS_HOST") {
            None => None,
            Some(host) => {
                let ttl_secs: u64 = num(get("CACHE_TTL_SECONDS"), "CACHE_TTL_SECONDS", 3600)?;
                Some(RedisConfig {
                    host: host.to_owned(),
                    port: num(get("REDIS_PORT"), "REDIS_PORT", 6379)?,
                    auth_string: vars
                        .get("REDIS_AUTH_STRING")
                        .cloned()
                        .filter(|v| !v.is_empty()),
                    tls: boolean("REDIS_TLS", false)?,
                    ca_cert_path: get("REDIS_CA_CERT_PATH").map(str::to_owned),
                    ttl: (ttl_secs > 0).then(|| Duration::from_secs(ttl_secs)),
                    op_timeout: Duration::from_millis(num(
                        get("CACHE_TIMEOUT_MS"),
                        "CACHE_TIMEOUT_MS",
                        500,
                    )?),
                })
            }
        };

        let sampling: f64 = num(
            get("TRACES_SAMPLING_PROBABILITY"),
            "TRACES_SAMPLING_PROBABILITY",
            0.1,
        )?;
        if !(0.0..=1.0).contains(&sampling) {
            return Err(ConfigError(
                "TRACES_SAMPLING_PROBABILITY must be within 0..=1".into(),
            ));
        }

        Ok(Config {
            grpc_port: num(get("GRPC_PORT"), "GRPC_PORT", 9090)?,
            management_port: num(get("MANAGEMENT_PORT"), "MANAGEMENT_PORT", 8081)?,
            shutdown_grace: Duration::from_secs(num(
                get("GRPC_SHUTDOWN_GRACE_SECONDS"),
                "GRPC_SHUTDOWN_GRACE_SECONDS",
                20,
            )?),
            db,
            redis,
            otel: OtelConfig {
                export_enabled: boolean("OTEL_EXPORT_ENABLED", true)?,
                traces_url: string("OTEL_TRACES_URL", "http://localhost:4318/v1/traces"),
                sampling_probability: sampling,
                pod_name: string("POD_NAME", "local"),
            },
            seed_enabled: boolean("APP_SEED_ENABLED", true)?,
            log_format: string("LOG_FORMAT", "json"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_map(
            &pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        )
    }

    #[test]
    fn defaults_match_the_java_service() {
        let c = cfg(&[]).unwrap();
        assert_eq!((c.grpc_port, c.management_port), (9090, 8081));
        assert_eq!(c.db.host, "localhost");
        assert_eq!(c.db.port, 5432);
        assert_eq!(c.db.name, "messagedb");
        assert_eq!(c.db.user, "message_app");
        assert_eq!(c.db.sslmode, "prefer");
        assert_eq!(c.db.pool_max, 10);
        assert!(c.redis.is_none());
        assert!(c.seed_enabled);
        assert_eq!(c.otel.sampling_probability, 0.1);
    }

    #[test]
    fn redis_settings() {
        let c = cfg(&[
            ("REDIS_HOST", "redis"),
            ("REDIS_AUTH_STRING", "s3cret"),
            ("REDIS_TLS", "true"),
            ("REDIS_CA_CERT_PATH", "/ca.pem"),
        ])
        .unwrap();
        let r = c.redis.unwrap();
        assert_eq!((r.host.as_str(), r.port, r.tls), ("redis", 6379, true));
        assert_eq!(r.auth_string.as_deref(), Some("s3cret"));
        assert_eq!(r.ca_cert_path.as_deref(), Some("/ca.pem"));
        assert_eq!(r.ttl, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn rejects_garbage() {
        assert!(cfg(&[("DB_PORT", "nope")]).is_err());
        assert!(cfg(&[("REDIS_HOST", "r"), ("REDIS_TLS", "maybe")]).is_err());
        assert!(cfg(&[("TRACES_SAMPLING_PROBABILITY", "2")]).is_err());
    }

    #[test]
    fn ttl_zero_disables_expiry() {
        let c = cfg(&[("REDIS_HOST", "r"), ("CACHE_TTL_SECONDS", "0")]).unwrap();
        assert_eq!(c.redis.unwrap().ttl, None);
    }
}
