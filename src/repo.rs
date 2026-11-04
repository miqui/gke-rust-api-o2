//! SQL, unchanged from the Java repositories (same statements, same ordering). Every function
//! takes an executor so callers choose pool vs explicit transaction.

use crate::pb::{Author, Message, MessageSummary};
use chrono::{DateTime, Utc};
use prost_types::Timestamp;
use sqlx::{PgExecutor, Row, postgres::PgRow};
use uuid::Uuid;

fn ts(t: DateTime<Utc>) -> Timestamp {
    Timestamp {
        seconds: t.timestamp(),
        nanos: t.timestamp_subsec_nanos() as i32,
    }
}

fn author_row(row: &PgRow, prefix: &str) -> Result<Author, sqlx::Error> {
    Ok(Author {
        id: row
            .try_get::<Uuid, _>(format!("{prefix}id").as_str())?
            .to_string(),
        name: row.try_get(format!("{prefix}name").as_str())?,
        email: row.try_get(format!("{prefix}email").as_str())?,
        created_at: Some(ts(row.try_get(format!("{prefix}created_at").as_str())?)),
    })
}

fn summary_row(row: &PgRow) -> Result<MessageSummary, sqlx::Error> {
    Ok(MessageSummary {
        id: row.try_get::<Uuid, _>("id")?.to_string(),
        title: row.try_get("title")?,
        content: row.try_get("content")?,
        created_at: Some(ts(row.try_get("created_at")?)),
        version: row.try_get("version")?,
    })
}

/// Message columns unprefixed, author columns prefixed with `a_`.
fn message_row(row: &PgRow) -> Result<Message, sqlx::Error> {
    Ok(Message {
        id: row.try_get::<Uuid, _>("id")?.to_string(),
        title: row.try_get("title")?,
        content: row.try_get("content")?,
        created_at: Some(ts(row.try_get("created_at")?)),
        version: row.try_get("version")?,
        author: Some(author_row(row, "a_")?),
    })
}

pub mod messages {
    use super::*;

    const SELECT_WITH_AUTHOR: &str = "SELECT m.id, m.title, m.content, m.created_at, m.version, \
         a.id AS a_id, a.name AS a_name, a.email AS a_email, a.created_at AS a_created_at \
         FROM messages m JOIN authors a ON a.id = m.author_id";

    pub async fn page<'e>(
        db: impl PgExecutor<'e>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        sqlx::query(&format!(
            "{SELECT_WITH_AUTHOR} ORDER BY m.created_at DESC, m.id DESC LIMIT $1 OFFSET $2"
        ))
        .bind(limit)
        .bind(offset)
        .try_map(|r: PgRow| message_row(&r))
        .fetch_all(db)
        .await
    }

    pub async fn count<'e>(db: impl PgExecutor<'e>) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT count(*) FROM messages")
            .fetch_one(db)
            .await
    }

    pub async fn find<'e>(
        db: impl PgExecutor<'e>,
        id: Uuid,
    ) -> Result<Option<Message>, sqlx::Error> {
        sqlx::query(&format!("{SELECT_WITH_AUTHOR} WHERE m.id = $1"))
            .bind(id)
            .try_map(|r: PgRow| message_row(&r))
            .fetch_optional(db)
            .await
    }

    pub async fn exists<'e>(db: impl PgExecutor<'e>, id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM messages WHERE id = $1)")
            .bind(id)
            .fetch_one(db)
            .await
    }

    /// Returns the new message's id. FK violation (23503) for a gone author.
    pub async fn insert<'e>(
        db: impl PgExecutor<'e>,
        title: &str,
        content: &str,
        author_id: Uuid,
    ) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO messages (title, content, author_id) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(title)
        .bind(content)
        .bind(author_id)
        .fetch_one(db)
        .await
    }

    /// Optimistic locking in a single statement: the row only changes if it is still at the
    /// version the caller read. `None` when no row matched.
    pub async fn update_if_version<'e>(
        db: impl PgExecutor<'e>,
        id: Uuid,
        title: Option<&str>,
        content: &str,
        version: i32,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "UPDATE messages SET title = COALESCE($2::varchar, title), content = $3, \
             version = version + 1 WHERE id = $1 AND version = $4 RETURNING id",
        )
        .bind(id)
        .bind(title)
        .bind(content)
        .bind(version)
        .fetch_optional(db)
        .await
    }

    pub async fn delete<'e>(db: impl PgExecutor<'e>, id: Uuid) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(id)
            .execute(db)
            .await?
            .rows_affected()
            > 0)
    }
}

pub mod authors {
    use super::*;

    pub async fn page<'e>(
        db: impl PgExecutor<'e>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Author>, sqlx::Error> {
        sqlx::query(
            "SELECT id, name, email, created_at FROM authors \
             ORDER BY created_at ASC, id ASC LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .try_map(|r: PgRow| author_row(&r, ""))
        .fetch_all(db)
        .await
    }

    pub async fn count<'e>(db: impl PgExecutor<'e>) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT count(*) FROM authors")
            .fetch_one(db)
            .await
    }

    pub async fn find<'e>(
        db: impl PgExecutor<'e>,
        id: Uuid,
    ) -> Result<Option<Author>, sqlx::Error> {
        sqlx::query("SELECT id, name, email, created_at FROM authors WHERE id = $1")
            .bind(id)
            .try_map(|r: PgRow| author_row(&r, ""))
            .fetch_optional(db)
            .await
    }

    pub async fn exists<'e>(db: impl PgExecutor<'e>, id: Uuid) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM authors WHERE id = $1)")
            .bind(id)
            .fetch_one(db)
            .await
    }

    /// Newest first, without the author (no author -> messages -> author cycle).
    pub async fn messages_of<'e>(
        db: impl PgExecutor<'e>,
        author_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<MessageSummary>, sqlx::Error> {
        sqlx::query(
            "SELECT id, title, content, created_at, version FROM messages \
             WHERE author_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2 OFFSET $3",
        )
        .bind(author_id)
        .bind(limit)
        .bind(offset)
        .try_map(|r: PgRow| summary_row(&r))
        .fetch_all(db)
        .await
    }

    /// Unique violation (23505) for an email that is taken.
    pub async fn insert<'e>(
        db: impl PgExecutor<'e>,
        name: &str,
        email: &str,
    ) -> Result<Author, sqlx::Error> {
        sqlx::query(
            "INSERT INTO authors (name, email) VALUES ($1, $2) \
             RETURNING id, name, email, created_at",
        )
        .bind(name)
        .bind(email)
        .try_map(|r: PgRow| author_row(&r, ""))
        .fetch_one(db)
        .await
    }

    /// `None` arguments leave the column unchanged. `None` result when the id doesn't exist.
    pub async fn update<'e>(
        db: impl PgExecutor<'e>,
        id: Uuid,
        name: Option<&str>,
        email: Option<&str>,
    ) -> Result<Option<Author>, sqlx::Error> {
        sqlx::query(
            "UPDATE authors SET name = COALESCE($2::varchar, name), \
             email = COALESCE($3::varchar, email) WHERE id = $1 \
             RETURNING id, name, email, created_at",
        )
        .bind(id)
        .bind(name)
        .bind(email)
        .try_map(|r: PgRow| author_row(&r, ""))
        .fetch_optional(db)
        .await
    }

    /// FK violation (23503) while the author still has messages.
    pub async fn delete<'e>(db: impl PgExecutor<'e>, id: Uuid) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("DELETE FROM authors WHERE id = $1")
            .bind(id)
            .execute(db)
            .await?
            .rows_affected()
            > 0)
    }
}
