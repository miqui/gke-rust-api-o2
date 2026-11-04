//! Use cases (the Java `Messages` / `Authors` services). Transactions are explicit
//! (`pool.begin()` .. `commit()`, PostgreSQL's default READ COMMITTED isolation: nothing sets a
//! level, as in the Java service), so cache evictions visibly happen after the commit.

use crate::{
    cache::MessageCache,
    error::{ApiError, FK_VIOLATION, UNIQUE_VIOLATION, sqlstate},
    pb::{Author, GetAuthorResponse, ListAuthorsResponse, ListMessagesResponse, Message},
    repo,
};
use sqlx::PgPool;
use tonic::Code;
use uuid::Uuid;

fn message_not_found(id: Uuid) -> ApiError {
    ApiError::NotFound(format!("Message with ID '{id}' was not found."))
}

fn author_not_found(id: Uuid) -> ApiError {
    ApiError::NotFound(format!("Author with ID '{id}' was not found."))
}

#[derive(Clone)]
pub struct Messages {
    pool: PgPool,
    cache: MessageCache,
}

impl Messages {
    pub fn new(pool: PgPool, cache: MessageCache) -> Self {
        Self { pool, cache }
    }

    pub async fn list(&self, limit: i32, offset: i64) -> Result<ListMessagesResponse, ApiError> {
        let mut tx = self.pool.begin().await?;
        let items = repo::messages::page(&mut *tx, limit.into(), offset).await?;
        let total_count = repo::messages::count(&mut *tx).await?;
        tx.commit().await?;
        Ok(ListMessagesResponse { items, total_count })
    }

    /// Cache-aside: read the cache first; on a miss load from the DB and populate the cache.
    pub async fn get(&self, id: Uuid) -> Result<Message, ApiError> {
        let guard = self.cache.lock(id).await;
        let result = self.get_locked(id, guard.is_some()).await;
        self.cache.unlock(guard).await;
        result
    }

    async fn get_locked(&self, id: Uuid, locked: bool) -> Result<Message, ApiError> {
        if let Some(mut cached) = self.cache.get(id).await {
            // The author is always read live, so author edits show up on cached messages.
            let author_id = cached
                .author
                .as_ref()
                .and_then(|a| Uuid::parse_str(&a.id).ok())
                .ok_or_else(|| ApiError::Internal("cache entry without author".into()))?;
            let author = repo::authors::find(&self.pool, author_id)
                .await?
                .ok_or_else(|| author_not_found(author_id))?;
            cached.author = Some(author);
            return Ok(cached);
        }
        let message = repo::messages::find(&self.pool, id)
            .await?
            .ok_or_else(|| message_not_found(id))?;
        // Only fill while holding the lock: without it a concurrent update could be overwritten
        // by this (possibly stale) read.
        if locked {
            self.cache.put(&message).await;
        }
        Ok(message)
    }

    pub async fn create(
        &self,
        title: &str,
        content: &str,
        author_id: Uuid,
    ) -> Result<Message, ApiError> {
        let attempt = async {
            let mut tx = self.pool.begin().await?;
            if !repo::authors::exists(&mut *tx, author_id).await? {
                return Ok(Err(author_not_found(author_id)));
            }
            let id = repo::messages::insert(&mut *tx, title, content, author_id).await?;
            let message = repo::messages::find(&mut *tx, id)
                .await?
                .ok_or(sqlx::Error::RowNotFound)?;
            tx.commit().await?;
            Ok::<_, sqlx::Error>(Ok(message))
        }
        .await;
        match attempt {
            Ok(r) => r,
            // The author was deleted between the check and the insert.
            Err(e) if sqlstate(&e).as_deref() == Some(FK_VIOLATION) => {
                Err(author_not_found(author_id))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Applies only if the row is still at the version the caller read. When nothing matched:
    /// NOT_FOUND if the id doesn't exist, otherwise ABORTED (the row moved on).
    pub async fn update(
        &self,
        id: Uuid,
        title: Option<&str>,
        content: &str,
        version: i32,
    ) -> Result<Message, ApiError> {
        let guard = self.cache.lock(id).await;
        let result = self.update_locked(id, title, content, version).await;
        self.cache.unlock(guard).await;
        result
    }

    async fn update_locked(
        &self,
        id: Uuid,
        title: Option<&str>,
        content: &str,
        version: i32,
    ) -> Result<Message, ApiError> {
        let mut tx = self.pool.begin().await?;
        let updated =
            match repo::messages::update_if_version(&mut *tx, id, title, content, version).await? {
                Some(updated_id) => Some(
                    repo::messages::find(&mut *tx, updated_id)
                        .await?
                        .ok_or(sqlx::Error::RowNotFound)?,
                ),
                None => None,
            };
        tx.commit().await?;
        self.cache.evict(id).await;
        match updated {
            Some(m) => Ok(m),
            None if !repo::messages::exists(&self.pool, id).await? => Err(message_not_found(id)),
            None => Err(ApiError::conflict(
                Code::Aborted,
                format!(
                    "Message with ID '{id}' has changed since version {version} was read; \
                     refetch and retry."
                ),
            )),
        }
    }

    pub async fn delete(&self, id: Uuid) -> Result<(), ApiError> {
        let guard = self.cache.lock(id).await;
        let result = async {
            let deleted = repo::messages::delete(&self.pool, id).await?;
            self.cache.evict(id).await;
            if deleted {
                Ok(())
            } else {
                Err(message_not_found(id))
            }
        }
        .await;
        self.cache.unlock(guard).await;
        result
    }
}

#[derive(Clone)]
pub struct Authors {
    pool: PgPool,
}

fn duplicate_email(email: &str) -> ApiError {
    ApiError::conflict(
        Code::AlreadyExists,
        format!("Author with email '{email}' already exists."),
    )
}

impl Authors {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn list(&self, limit: i32, offset: i64) -> Result<ListAuthorsResponse, ApiError> {
        let mut tx = self.pool.begin().await?;
        let items = repo::authors::page(&mut *tx, limit.into(), offset).await?;
        let total_count = repo::authors::count(&mut *tx).await?;
        tx.commit().await?;
        Ok(ListAuthorsResponse { items, total_count })
    }

    pub async fn get(
        &self,
        id: Uuid,
        include_messages: bool,
        limit: i32,
        offset: i64,
    ) -> Result<GetAuthorResponse, ApiError> {
        let mut tx = self.pool.begin().await?;
        let author = repo::authors::find(&mut *tx, id)
            .await?
            .ok_or_else(|| author_not_found(id))?;
        let mut response = GetAuthorResponse {
            author: Some(author),
            ..Default::default()
        };
        if include_messages {
            // One extra row tells whether another page exists.
            let mut messages =
                repo::authors::messages_of(&mut *tx, id, i64::from(limit) + 1, offset).await?;
            let more = messages.len() > limit as usize;
            messages.truncate(limit as usize);
            response.messages = messages;
            if more {
                // Java: Math.addExact(offset, limit) -> ArithmeticException -> INTERNAL.
                let next = offset
                    .checked_add(i64::from(limit))
                    .ok_or_else(|| ApiError::Internal("messages offset overflow".into()))?;
                response.next_messages_offset = Some(next);
            }
        }
        tx.commit().await?;
        Ok(response)
    }

    pub async fn create(&self, name: &str, email: &str) -> Result<Author, ApiError> {
        match repo::authors::insert(&self.pool, name, email).await {
            Ok(a) => Ok(a),
            Err(e) if sqlstate(&e).as_deref() == Some(UNIQUE_VIOLATION) => {
                Err(duplicate_email(email))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// `None` name/email mean unchanged; with nothing to change it still 404s for an unknown id.
    pub async fn update(
        &self,
        id: Uuid,
        name: Option<&str>,
        email: Option<&str>,
    ) -> Result<Author, ApiError> {
        if name.is_none() && email.is_none() {
            return repo::authors::find(&self.pool, id)
                .await?
                .ok_or_else(|| author_not_found(id));
        }
        match repo::authors::update(&self.pool, id, name, email).await {
            Ok(Some(a)) => Ok(a),
            Ok(None) => Err(author_not_found(id)),
            Err(e) if sqlstate(&e).as_deref() == Some(UNIQUE_VIOLATION) => {
                Err(duplicate_email(email.unwrap_or_default()))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn delete(&self, id: Uuid) -> Result<(), ApiError> {
        match repo::authors::delete(&self.pool, id).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(author_not_found(id)),
            Err(e) if sqlstate(&e).as_deref() == Some(FK_VIOLATION) => Err(ApiError::conflict(
                Code::FailedPrecondition,
                format!("Author with ID '{id}' still has messages and cannot be deleted."),
            )),
            Err(e) => Err(e.into()),
        }
    }
}
