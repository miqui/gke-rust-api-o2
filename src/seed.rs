//! Pre-populates a welcome message (and its system author) on a fresh database. Idempotent, and
//! safe with all replicas starting at once: a transaction-scoped advisory lock serialises the
//! check-then-insert, so exactly one welcome message is created.

use sqlx::PgPool;

pub const SYSTEM_EMAIL: &str = "system@message-service.local";
/// Arbitrary fixed key (same as the Java service); distinct from the migration lock.
pub const SEED_LOCK_KEY: i64 = 7_265_490_001;
pub const WELCOME_TITLE: &str = "Welcome to the Kubernetes gRPC API";
pub const WELCOME_CONTENT: &str =
    "A sample message backed by Rust (tonic) gRPC and Cloud SQL for PostgreSQL on GKE.";

/// Returns true when it inserted the welcome message.
pub async fn seed(pool: &PgPool) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(SEED_LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let messages: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&mut *tx)
        .await?;
    if messages > 0 {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO authors (name, email) VALUES ('system', $1) ON CONFLICT (email) DO NOTHING",
    )
    .bind(SYSTEM_EMAIL)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO messages (title, content, author_id) \
         SELECT $1, $2, id FROM authors WHERE email = $3",
    )
    .bind(WELCOME_TITLE)
    .bind(WELCOME_CONTENT)
    .bind(SYSTEM_EMAIL)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}
