//! Startup migrations (`migrations/V<n>__<description>.sql`, embedded at compile time).
//!
//! Adopts a schema created by the Spring Boot service's Flyway: when `flyway_schema_history`
//! exists, every successfully applied Flyway version counts as applied and is not re-run. The
//! Flyway table itself is never written (so a rollback to the Java image keeps working); the
//! versions this runner applies are tracked in `schema_migrations_rust`. All replicas may start
//! at once: a session advisory lock serialises them.

use sqlx::{Connection, PgPool, Row};
use std::collections::BTreeSet;

pub struct Migration {
    pub version: i64,
    pub description: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "initial schema",
        sql: include_str!("../migrations/V1__initial_schema.sql"),
    },
    Migration {
        version: 2,
        description: "author message pagination index",
        sql: include_str!("../migrations/V2__author_message_pagination_index.sql"),
    },
];

/// Distinct from the seeder's lock and from Flyway's own advisory lock.
const MIGRATE_LOCK_KEY: i64 = 7_265_490_002;

/// Versions already applied, per `schema_migrations_rust` and (if present) Flyway's history.
pub async fn applied_versions(conn: &mut sqlx::PgConnection) -> Result<BTreeSet<i64>, sqlx::Error> {
    let mut applied = BTreeSet::new();
    for row in sqlx::query("SELECT version FROM schema_migrations_rust")
        .fetch_all(&mut *conn)
        .await?
    {
        applied.insert(row.try_get::<i64, _>(0)?);
    }
    let flyway: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('flyway_schema_history')::text")
            .fetch_one(&mut *conn)
            .await?;
    if flyway.is_some() {
        // Flyway keeps `version` as text ("1", "2"); baseline rows count as applied too.
        let rows = sqlx::query(
            "SELECT version FROM flyway_schema_history WHERE success AND version IS NOT NULL",
        )
        .fetch_all(&mut *conn)
        .await?;
        for row in rows {
            if let Ok(v) = row.try_get::<String, _>(0)?.trim().parse::<i64>() {
                applied.insert(v);
            }
        }
    }
    Ok(applied)
}

/// Applies every pending migration (each in its own transaction); returns the versions applied.
pub async fn run(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(MIGRATE_LOCK_KEY)
        .execute(&mut *conn)
        .await?;
    let result = run_locked(&mut conn).await;
    let unlock = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(MIGRATE_LOCK_KEY)
        .execute(&mut *conn)
        .await;
    let applied = result?;
    unlock?;
    Ok(applied)
}

async fn run_locked(conn: &mut sqlx::PgConnection) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS schema_migrations_rust (\
         version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
         installed_on TIMESTAMPTZ NOT NULL DEFAULT now())",
    )
    .execute(&mut *conn)
    .await?;
    let done = applied_versions(conn).await?;
    let mut newly = Vec::new();
    for m in MIGRATIONS {
        if done.contains(&m.version) {
            continue;
        }
        tracing::info!(
            version = m.version,
            description = m.description,
            "applying migration"
        );
        let mut tx = conn.begin().await?;
        sqlx::Executor::execute(&mut *tx, m.sql).await?;
        sqlx::query("INSERT INTO schema_migrations_rust (version, description) VALUES ($1, $2)")
            .bind(m.version)
            .bind(m.description)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        newly.push(m.version);
    }
    Ok(newly)
}
